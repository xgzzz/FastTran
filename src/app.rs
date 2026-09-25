use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use eframe::egui;
use tokio::runtime::Runtime;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use uuid::Uuid;

use crate::config::{AppConfig, config_path, normalize_device_name};
use crate::discovery::{DiscoveryService, Peer};
use crate::format::{human_bytes, human_duration, human_speed};
use crate::model::{TransferDirection, TransferHub, TransferSnapshot, TransferState};
use crate::network::AppEvent;
use crate::receiver::ReceiverServer;
use crate::sender::TransferService;

#[cfg(target_os = "android")]
use android_activity::AndroidApp;

fn init_logging() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "fasttran=info".into()),
        )
        .try_init();
}

#[cfg(target_os = "windows")]
fn windows_centered_window_position(inner_width: f32, inner_height: f32) -> Option<egui::Pos2> {
    use std::mem::size_of;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct NativeRect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetDpiForSystem() -> u32;
        fn AdjustWindowRectEx(rect: *mut NativeRect, style: u32, menu: i32, ex_style: u32) -> i32;
        fn SystemParametersInfoW(
            action: u32,
            param: u32,
            result: *mut NativeRect,
            win_ini: u32,
        ) -> i32;
    }

    const SPI_GETWORKAREA: u32 = 0x0030;
    const WS_OVERLAPPEDWINDOW: u32 = 0x00CF_0000;

    let dpi = unsafe { GetDpiForSystem() }.max(96) as f32;
    let scale = dpi / 96.0;
    let mut frame = NativeRect {
        left: 0,
        top: 0,
        right: (inner_width * scale).round() as i32,
        bottom: (inner_height * scale).round() as i32,
    };
    if unsafe { AdjustWindowRectEx(&mut frame, WS_OVERLAPPEDWINDOW, 0, 0) } == 0 {
        return None;
    }

    let mut work_area = NativeRect {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    if unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            size_of::<NativeRect>() as u32,
            &mut work_area,
            0,
        )
    } == 0
    {
        return None;
    }

    let work_width = (work_area.right - work_area.left) as f32 / scale;
    let work_height = (work_area.bottom - work_area.top) as f32 / scale;
    let outer_width = (frame.right - frame.left) as f32 / scale;
    let outer_height = (frame.bottom - frame.top) as f32 / scale;
    if work_width <= outer_width || work_height <= outer_height {
        return None;
    }

    Some(egui::pos2(
        work_area.left as f32 / scale + (work_width - outer_width) / 2.0,
        work_area.top as f32 / scale + (work_height - outer_height) / 2.0,
    ))
}

fn native_options() -> eframe::NativeOptions {
    #[allow(unused_mut)]
    let mut viewport = egui::ViewportBuilder::default()
        .with_app_id("fasttran")
        .with_icon(app_icon());
    #[cfg(not(target_os = "android"))]
    {
        viewport = viewport
            .with_inner_size([1040.0, 700.0])
            .with_min_inner_size([900.0, 600.0]);
    }
    #[cfg(target_os = "windows")]
    if let Some(position) = windows_centered_window_position(1040.0, 700.0) {
        // ViewportBuilder positions the outer frame. Account for the
        // Windows title bar/taskbar so the visible window is truly centered.
        viewport = viewport.with_position(position);
    }

    #[allow(unused_mut)]
    let mut options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    #[cfg(not(target_os = "android"))]
    {
        // Ignore a stale persisted position from a previous monitor/DPI
        // configuration. Windows uses the work-area-aware position above.
        options.persist_window = false;
        #[cfg(not(target_os = "windows"))]
        {
            options.centered = true;
        }
        #[cfg(target_os = "windows")]
        {
            options.centered = false;
        }
    }
    options
}

pub fn run() -> Result<(), eframe::Error> {
    init_logging();
    eframe::run_native(
        "FastTran",
        native_options(),
        Box::new(|creation_context| Ok(Box::new(FastTranApp::new(creation_context)))),
    )
}

#[cfg(target_os = "android")]
pub fn run_android(app: AndroidApp) -> Result<(), eframe::Error> {
    init_logging();
    crate::android_bridge::set_app(app.clone());
    let config_file = app
        .internal_data_path()
        .or_else(|| app.external_data_path())
        .map(|path| path.join("config.json"));
    let download_dir = app
        .external_data_path()
        .or_else(|| app.internal_data_path())
        .map(|path| path.join("FastTran"));
    let options = eframe::NativeOptions {
        android_app: Some(app),
        ..native_options()
    };
    eframe::run_native(
        "FastTran",
        options,
        Box::new(|creation_context| {
            Ok(Box::new(FastTranApp::new_with_download_fallback(
                creation_context,
                download_dir,
                config_file,
            )))
        }),
    )
}

#[derive(Debug, Clone, Copy)]
struct PageContext {
    width: f32,
    height: f32,
}

impl PageContext {
    fn two_columns(self) -> bool {
        self.width >= 760.0 && self.height >= 500.0
    }

    fn compact(self) -> bool {
        self.width < 700.0 || self.height < 560.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Send,
    Transfers,
    Settings,
}

impl Page {
    fn title(self) -> &'static str {
        match self {
            Self::Send => "发送文件",
            Self::Transfers => "传输任务",
            Self::Settings => "设置",
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum NoticeKind {
    Info,
    Success,
    Error,
}

struct Notice {
    message: String,
    kind: NoticeKind,
    expires_at: Instant,
}

impl Notice {
    fn new(message: impl Into<String>, kind: NoticeKind, duration: Duration) -> Self {
        Self {
            message: message.into(),
            kind,
            expires_at: Instant::now() + duration,
        }
    }
}

struct FastTranApp {
    _runtime: Runtime,
    config: AppConfig,
    config_file: PathBuf,
    shared_config: Arc<RwLock<AppConfig>>,
    download_dir: Arc<RwLock<PathBuf>>,
    hub: Arc<TransferHub>,
    sender: TransferService,
    discovery: Option<DiscoveryService>,
    receiver: Option<ReceiverServer>,
    events: UnboundedReceiver<AppEvent>,
    event_tx: UnboundedSender<AppEvent>,
    services_enabled: bool,
    page: Page,
    selected_files: Vec<PathBuf>,
    selected_peer: Option<Uuid>,
    peer_selection_explicit: bool,
    discovered_peers: Vec<Peer>,
    manual_peers: Vec<Peer>,
    manual_address: String,
    device_name_input: String,
    dark_mode: bool,
    notice: Option<Notice>,
}

impl FastTranApp {
    fn new(creation_context: &eframe::CreationContext<'_>) -> Self {
        Self::new_with_download_fallback(creation_context, None, None)
    }

    fn new_with_download_fallback(
        creation_context: &eframe::CreationContext<'_>,
        download_dir: Option<PathBuf>,
        config_file: Option<PathBuf>,
    ) -> Self {
        install_system_font(&creation_context.egui_ctx);

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("fasttran-network")
            .build()
            .expect("failed to create FastTran runtime");
        let config_file = config_file.unwrap_or_else(config_path);
        let mut config = AppConfig::load_from_path(&config_file);
        if let Some(download_dir) = download_dir {
            config.download_dir = download_dir;
        }
        if let Err(error) = config.save_to_path(&config_file) {
            tracing::warn!(%error, "failed to save startup configuration");
        }
        let _ = std::fs::create_dir_all(&config.download_dir);

        let shared_config = Arc::new(RwLock::new(config.clone()));
        let download_dir = Arc::new(RwLock::new(config.download_dir.clone()));
        let hub = Arc::new(TransferHub::default());
        let sender = TransferService::new(runtime.handle().clone(), hub.clone());
        let (event_tx, events) = unbounded_channel();

        let dark_mode = config.dark_mode;
        set_theme(&creation_context.egui_ctx, dark_mode);
        #[cfg(target_os = "android")]
        crate::android_bridge::set_system_bar_style(dark_mode);

        let mut app = Self {
            _runtime: runtime,
            config: config.clone(),
            config_file,
            shared_config,
            download_dir,
            hub,
            sender,
            discovery: None,
            receiver: None,
            events,
            event_tx,
            services_enabled: false,
            page: Page::Send,
            selected_files: Vec::new(),
            selected_peer: None,
            peer_selection_explicit: false,
            discovered_peers: Vec::new(),
            manual_peers: Vec::new(),
            manual_address: String::new(),
            device_name_input: config.device_name.clone(),
            dark_mode,
            notice: None,
        };
        app.start_services();
        app
    }

    fn start_services(&mut self) {
        self.services_enabled = true;
        if self.receiver.is_none() {
            let result = self._runtime.block_on(ReceiverServer::start(
                self.config.transfer_port,
                self.download_dir.clone(),
                self.hub.clone(),
                self.event_tx.clone(),
            ));
            match result {
                Ok(server) => self.receiver = Some(server),
                Err(error) => {
                    let _ = self.event_tx.send(AppEvent::Error {
                        scope: "接收服务",
                        message: error.to_string(),
                    });
                }
            }
        }

        if self.discovery.is_none() {
            let result = self._runtime.block_on(DiscoveryService::start(
                self.shared_config.clone(),
                self.event_tx.clone(),
            ));
            match result {
                Ok(service) => self.discovery = Some(service),
                Err(error) => {
                    tracing::warn!(%error, "device discovery did not start");
                    let _ = self.event_tx.send(AppEvent::Error {
                        scope: "设备发现",
                        message: error.to_string(),
                    });
                }
            }
        }
    }

    fn stop_services(&mut self) {
        self.hub.cancel_receiving();
        self.receiver.take();
        self.discovery.take();
        self.discovered_peers.clear();
        self.services_enabled = false;
    }

    fn set_services_enabled(&mut self, enabled: bool) {
        if enabled {
            self.start_services();
        } else {
            self.stop_services();
        }
    }

    fn render_ui(&mut self, root_ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll_events();
        self.poll_android_callbacks();
        self.refresh_peers();
        self.collect_dropped_files(root_ui.ctx());
        root_ui
            .ctx()
            .request_repaint_after(Duration::from_millis(120));

        let palette = self.palette();
        let screen_width = root_ui.ctx().viewport_rect().width();
        let sidebar_width = if screen_width < 1_050.0 { 196.0 } else { 216.0 };
        self.show_top_bar(root_ui, palette);
        if cfg!(target_os = "android") {
            self.show_mobile_navigation(root_ui, palette);
        } else {
            self.show_sidebar(root_ui, palette, sidebar_width);
        }
        self.show_notice(root_ui, palette);

        let [_top_inset, right_inset, bottom_inset, left_inset] = self.safe_insets();
        let horizontal_inset = left_inset.max(right_inset).clamp(0.0, 40.0);
        let page_margin = if cfg!(target_os = "android") {
            (12.0 + horizontal_inset).round().clamp(0.0, 127.0) as i8
        } else {
            22
        };
        let content_bottom_inset = if self.notice.is_some() {
            0.0
        } else {
            bottom_inset.clamp(0.0, 40.0)
        };

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(palette.background).inner_margin(0))
            .show(root_ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("fasttran_page_scroll")
                    .auto_shrink([false, false])
                    .content_margin(egui::Margin {
                        left: page_margin,
                        right: page_margin,
                        top: 18,
                        bottom: (18.0 + content_bottom_inset).round().clamp(0.0, 127.0) as i8,
                    })
                    .show_viewport(ui, |ui, _viewport| {
                        let available_width = ui.available_width().max(1.0);
                        let content_width = available_width.min(1_160.0);
                        let content_height = ui.available_height().max(1.0);
                        let left_space = ((available_width - content_width) * 0.5).max(0.0);

                        ui.horizontal(|ui| {
                            ui.add_space(left_space);
                            ui.vertical(|ui| {
                                ui.set_width(content_width);
                                ui.set_max_width(content_width);
                                let context = PageContext {
                                    width: content_width,
                                    height: content_height,
                                };
                                match self.page {
                                    Page::Send => self.show_send_page(ui, palette, context),
                                    Page::Transfers => {
                                        self.show_transfers_page(ui, palette, context)
                                    }
                                    Page::Settings => self.show_settings_page(ui, palette, context),
                                }
                            });
                        });
                    });
            });
    }

    fn poll_events(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                AppEvent::Error { scope, message } => {
                    self.notice = Some(Notice::new(
                        format!("{scope}：{message}"),
                        NoticeKind::Error,
                        Duration::from_secs(8),
                    ));
                }
                AppEvent::Incoming { transfer_id } => {
                    self.page = Page::Transfers;
                    let name = self
                        .hub
                        .get(transfer_id)
                        .map(|job| job.file_name.clone())
                        .unwrap_or_else(|| "文件".to_owned());
                    self.notice = Some(Notice::new(
                        format!("收到 {} 的传输请求，请确认接收", name),
                        NoticeKind::Info,
                        Duration::from_secs(8),
                    ));
                }
                AppEvent::Received {
                    transfer_id: _,
                    file_path,
                } => {
                    self.notice = Some(Notice::new(
                        format!("已接收 {}", file_path.display()),
                        NoticeKind::Success,
                        Duration::from_secs(5),
                    ));
                }
            }
        }
    }

    #[cfg(target_os = "android")]
    fn poll_android_callbacks(&mut self) {
        let picked = crate::android_bridge::take_picked_files();
        if !picked.is_empty() {
            let mut added = 0_usize;
            for path in picked {
                if path.is_file() && !self.selected_files.iter().any(|selected| selected == &path) {
                    self.selected_files.push(path);
                    added += 1;
                }
            }
            if added > 0 {
                self.notice = Some(Notice::new(
                    format!("已添加 {added} 个文件"),
                    NoticeKind::Success,
                    Duration::from_secs(3),
                ));
            }
        }

        for notification in crate::android_bridge::take_notifications() {
            self.notice = Some(Notice::new(
                notification.message,
                if notification.is_error {
                    NoticeKind::Error
                } else {
                    NoticeKind::Info
                },
                Duration::from_secs(6),
            ));
        }
    }

    #[cfg(not(target_os = "android"))]
    fn poll_android_callbacks(&mut self) {}

    fn refresh_peers(&mut self) {
        if self.services_enabled {
            if let Some(discovery) = &self.discovery {
                self.discovered_peers = discovery.peers();
            } else {
                self.discovered_peers.clear();
            }
        } else {
            self.discovered_peers.clear();
        }
        let peers = self.all_peers();
        if self.selected_peer.is_none() {
            if !self.peer_selection_explicit {
                self.selected_peer = peers.first().map(|peer| peer.id);
            }
        } else if !peers.iter().any(|peer| Some(peer.id) == self.selected_peer) {
            // Do not silently redirect a user's selection to another device.
            self.selected_peer = None;
        }
    }

    fn collect_dropped_files(&mut self, context: &egui::Context) {
        let dropped = context.input(|input| input.raw.dropped_files.clone());
        if !dropped.is_empty() {
            let mut added = 0_usize;
            for dropped_file in dropped {
                let path = dropped_file.path();
                if path.is_file() && !self.selected_files.iter().any(|selected| selected == path) {
                    self.selected_files.push(path.to_path_buf());
                    added += 1;
                } else if path.is_dir() {
                    self.notice = Some(Notice::new(
                        "暂不支持直接发送文件夹，请先压缩后再拖入",
                        NoticeKind::Info,
                        Duration::from_secs(5),
                    ));
                }
            }
            if added > 0 {
                self.notice = Some(Notice::new(
                    format!("已添加 {added} 个文件"),
                    NoticeKind::Success,
                    Duration::from_secs(2),
                ));
            }
        }
    }

    fn safe_insets(&self) -> [f32; 4] {
        #[cfg(target_os = "android")]
        {
            crate::android_bridge::safe_insets()
        }
        #[cfg(not(target_os = "android"))]
        {
            [0.0; 4]
        }
    }

    fn show_top_bar(&mut self, ui: &mut egui::Ui, palette: Palette) {
        let [top_inset, right_inset, _bottom_inset, left_inset] = self.safe_insets();
        let top_inset = top_inset.clamp(0.0, 40.0);
        let horizontal_inset = left_inset.max(right_inset).clamp(0.0, 40.0);
        egui::Panel::top("top_bar")
            .exact_size(68.0 + top_inset)
            .frame(
                egui::Frame::new()
                    .fill(palette.surface)
                    .stroke(egui::Stroke::NONE)
                    .inner_margin(egui::Margin {
                        left: (24.0 + horizontal_inset).round().clamp(0.0, 127.0) as i8,
                        right: (24.0 + horizontal_inset).round().clamp(0.0, 127.0) as i8,
                        top: (14.0 + top_inset).round().clamp(0.0, 127.0) as i8,
                        bottom: 14,
                    }),
            )
            .show(ui, |ui| {
                let compact = ui.available_width() < 1_000.0;
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            egui::RichText::new(self.page.title())
                                .size(20.0)
                                .strong()
                                .color(palette.text),
                        );
                        ui.label(
                            egui::RichText::new("FastTran · 局域网 P2P 文件传输")
                                .size(11.0)
                                .color(palette.muted),
                        );
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let mut enabled = self.services_enabled;
                        if ui
                            .add(ServiceSwitch {
                                value: &mut enabled,
                                label: "服务",
                                palette,
                            })
                            .on_hover_text("开启或关闭设备发现与接收服务；关闭会取消正在接收的任务")
                            .changed()
                        {
                            self.set_services_enabled(enabled);
                        }
                        let receiver_running = self.receiver.is_some();
                        let discovery_running = self.discovery.is_some();
                        let (service_color, service_label) =
                            if receiver_running && discovery_running {
                                (palette.success, "运行中")
                            } else if receiver_running || discovery_running {
                                (palette.warning, "部分不可用")
                            } else if self.services_enabled {
                                (palette.danger, "启动失败")
                            } else {
                                (palette.muted, "已关闭")
                            };
                        ui.label(
                            egui::RichText::new(service_label)
                                .color(service_color)
                                .size(12.0),
                        );
                        if !compact && let Some(receiver) = &self.receiver {
                            ui.label(
                                egui::RichText::new(format!("TCP :{}", receiver.local_port()))
                                    .color(palette.muted)
                                    .monospace(),
                            );
                        }
                    });
                });
            });
    }

    fn show_mobile_navigation(&mut self, ui: &mut egui::Ui, palette: Palette) {
        let [_top_inset, right_inset, _bottom_inset, left_inset] = self.safe_insets();
        let horizontal_inset = left_inset.max(right_inset).clamp(0.0, 40.0);
        egui::Panel::top("mobile_navigation")
            .exact_size(52.0)
            .frame(
                egui::Frame::new()
                    .fill(palette.surface)
                    .stroke(egui::Stroke::new(1.0, palette.border))
                    .inner_margin(egui::Margin {
                        left: (8.0 + horizontal_inset).round().clamp(0.0, 127.0) as i8,
                        right: (8.0 + horizontal_inset).round().clamp(0.0, 127.0) as i8,
                        top: 6,
                        bottom: 6,
                    }),
            )
            .show(ui, |ui| {
                let spacing = ui.spacing().item_spacing.x;
                let item_width = ((ui.available_width() - spacing * 2.0) / 3.0).max(1.0);
                ui.horizontal(|ui| {
                    for (page, label) in [
                        (Page::Send, "发送"),
                        (Page::Transfers, "任务"),
                        (Page::Settings, "设置"),
                    ] {
                        ui.allocate_ui(egui::vec2(item_width, 40.0), |ui| {
                            let selected = self.page == page;
                            let color = if selected {
                                palette.accent
                            } else {
                                palette.muted
                            };
                            let response = ui
                                .add(
                                    egui::Button::new(
                                        egui::RichText::new(label).color(color).strong(),
                                    )
                                    .fill(if selected {
                                        palette.accent_soft
                                    } else {
                                        egui::Color32::TRANSPARENT
                                    })
                                    .corner_radius(8)
                                    .min_size(egui::vec2(item_width, 40.0)),
                                )
                                .on_hover_text(page.title());
                            if response.clicked() {
                                self.page = page;
                            }
                        });
                    }
                });
            });
    }

    fn show_sidebar(&mut self, ui: &mut egui::Ui, palette: Palette, sidebar_width: f32) {
        let horizontal_margin = if sidebar_width < 210.0 { 14.0 } else { 18.0 };
        egui::Panel::left("sidebar")
            .exact_size(sidebar_width)
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(palette.sidebar)
                    .stroke(egui::Stroke::NONE)
                    .inner_margin(egui::Margin::symmetric(horizontal_margin as i8, 20)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let logo = egui::Frame::new()
                        .fill(palette.accent_soft)
                        .corner_radius(10)
                        .inner_margin(egui::Margin::same(9))
                        .show(ui, |ui| {
                            ui.label(
                                egui::RichText::new("F")
                                    .color(palette.accent)
                                    .size(22.0)
                                    .strong(),
                            );
                        });
                    let _ = logo;
                    ui.vertical(|ui| {
                        ui.label(
                            egui::RichText::new("FastTran")
                                .size(19.0)
                                .strong()
                                .color(palette.text),
                        );
                        ui.label(
                            egui::RichText::new("飞传 · 快速分享")
                                .size(11.0)
                                .color(palette.muted),
                        );
                    });
                });

                ui.add_space(28.0);
                self.nav_button(ui, Page::Send, "发送文件", palette);
                self.nav_button(ui, Page::Transfers, "传输任务", palette);
                self.nav_button(ui, Page::Settings, "设置", palette);

                ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                    card(ui, palette, |ui| {
                        ui.label(
                            egui::RichText::new("本机设备")
                                .size(11.0)
                                .color(palette.muted),
                        );
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(&self.config.device_name)
                                    .strong()
                                    .color(palette.text),
                            )
                            .truncate(),
                        );
                        ui.label(
                            egui::RichText::new(format!(
                                "设备 ID  {}",
                                short_id(self.config.device_id)
                            ))
                            .size(10.0)
                            .monospace()
                            .color(palette.muted),
                        );
                    });
                    ui.add_space(12.0);
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new(if self.dark_mode {
                                    "浅色模式"
                                } else {
                                    "深色模式"
                                })
                                .color(palette.muted),
                            )
                            .frame(false),
                        )
                        .clicked()
                    {
                        let dark_mode = !self.dark_mode;
                        self.set_dark_mode(dark_mode, ui.ctx());
                    }
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new("FastTran 0.1.0")
                            .size(10.0)
                            .color(palette.subtle),
                    );
                });
            });
    }

    fn nav_button(&mut self, ui: &mut egui::Ui, page: Page, label: &str, palette: Palette) {
        let selected = self.page == page;
        let mut text = egui::RichText::new(label).color(if selected {
            palette.accent
        } else {
            palette.muted
        });
        if selected {
            text = text.strong();
        }
        let response = ui
            .add(
                egui::Button::new(text)
                    .fill(if selected {
                        palette.accent_soft
                    } else {
                        else_color()
                    })
                    .corner_radius(9)
                    .min_size(egui::vec2(ui.available_width(), 40.0)),
            )
            .on_hover_text(match page {
                Page::Send => "选择设备并发送本地文件",
                Page::Transfers => "查看发送和接收进度",
                Page::Settings => "设备名称、接收目录和外观",
            });
        if response.clicked() {
            self.page = page;
        }
    }

    fn show_send_page(&mut self, ui: &mut egui::Ui, palette: Palette, context: PageContext) {
        add_page_header(
            ui,
            "局域网极速传输",
            "将文件直接发送到同一网络中的另一台电脑，不经过云端。",
            palette,
        );
        ui.add_space(18.0);

        if context.two_columns() {
            // Use the already constrained page width instead of the parent
            // horizontal cursor width. The latter can include space outside
            // the central panel and push the second card off-screen.
            let gap = ui.spacing().item_spacing.x;
            let usable_width = (context.width - gap).max(600.0);
            let device_width = (usable_width * 0.42).max(300.0);
            let file_width = (usable_width - device_width).max(300.0);
            ui.horizontal_top(|ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(device_width, 0.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        self.show_device_panel(ui, palette, context);
                    },
                );
                ui.allocate_ui_with_layout(
                    egui::vec2(file_width, 0.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        self.show_file_panel(ui, palette, context);
                    },
                );
            });
        } else {
            self.show_device_panel(ui, palette, context);
            ui.add_space(14.0);
            self.show_file_panel(ui, palette, context);
        }

        ui.add_space(16.0);
        card(ui, palette, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    egui::RichText::new("发送流程")
                        .size(12.0)
                        .strong()
                        .color(palette.text),
                );
                for (index, label) in ["选择设备", "确认接收", "校验完成"].iter().enumerate()
                {
                    if index > 0 {
                        ui.label(egui::RichText::new("→").color(palette.subtle));
                    }
                    ui.label(
                        egui::RichText::new(format!("{}. {label}", index + 1)).color(palette.muted),
                    );
                }
                ui.label(
                    egui::RichText::new("TCP 直连 · SHA-256 校验 · 同名文件自动避让")
                        .color(palette.subtle),
                );
            });
        });
    }

    fn show_device_panel(&mut self, ui: &mut egui::Ui, palette: Palette, context: PageContext) {
        card(ui, palette, |ui| {
            let discovery_active = self.services_enabled && self.discovery.is_some();
            let device_subtitle = if discovery_active {
                "自动发现同一局域网中的 FastTran"
            } else {
                "服务已关闭，仅保留手动设备"
            };
            panel_heading(ui, "接收设备", device_subtitle, palette);

            let peers = self.all_peers();
            if peers.is_empty() {
                ui.add_space(10.0);
                ui.vertical_centered(|ui| {
                    ui.add_space(12.0);
                    if discovery_active {
                        ui.label(
                            egui::RichText::new("正在搜索附近设备…")
                                .color(palette.muted)
                                .size(14.0),
                        );
                        ui.label(
                            egui::RichText::new("请确认对方已启动 FastTran，且防火墙允许访问")
                                .color(palette.subtle)
                                .size(11.0),
                        );
                    } else {
                        ui.label(
                            egui::RichText::new("设备发现已关闭")
                                .color(palette.muted)
                                .size(14.0),
                        );
                        ui.label(
                            egui::RichText::new("打开顶部服务开关后，将自动搜索局域网设备")
                                .color(palette.subtle)
                                .size(11.0),
                        );
                        if ui
                            .button(
                                egui::RichText::new("开启服务")
                                    .color(palette.accent)
                                    .strong(),
                            )
                            .clicked()
                        {
                            self.set_services_enabled(true);
                        }
                    }
                    ui.add_space(12.0);
                });
            } else {
                egui::ScrollArea::vertical()
                    .max_height(if context.compact() { 170.0 } else { 250.0 })
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        for peer in peers {
                            let selected = self.selected_peer == Some(peer.id);
                            let response = egui::Frame::new()
                                .fill(if selected {
                                    palette.accent_soft
                                } else {
                                    palette.elevated
                                })
                                .stroke(egui::Stroke::new(
                                    1.0,
                                    if selected {
                                        palette.accent
                                    } else {
                                        palette.border
                                    },
                                ))
                                .corner_radius(10)
                                .inner_margin(11)
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        let action_width = 18.0;
                                        let gap = ui.spacing().item_spacing.x;
                                        let text_width =
                                            (ui.available_width() - action_width - gap).max(0.0);
                                        ui.vertical(|ui| {
                                            ui.set_width(text_width);
                                            ui.add(
                                                egui::Label::new(
                                                    egui::RichText::new(&peer.name)
                                                        .strong()
                                                        .color(palette.text),
                                                )
                                                .truncate(),
                                            );
                                            ui.add(
                                                egui::Label::new(
                                                    egui::RichText::new(format!(
                                                        "{}  ·  {}",
                                                        peer.display_address(),
                                                        peer.os
                                                    ))
                                                    .size(10.5)
                                                    .monospace()
                                                    .color(palette.muted),
                                                )
                                                .truncate(),
                                            );
                                        });
                                        ui.add_sized(
                                            [18.0, 20.0],
                                            egui::Label::new(
                                                egui::RichText::new(if selected {
                                                    "✓"
                                                } else {
                                                    ""
                                                })
                                                .color(palette.accent)
                                                .strong(),
                                            ),
                                        );
                                    });
                                })
                                .response
                                .interact(egui::Sense::click())
                                .on_hover_text("点击选择此接收设备");
                            if response.clicked() {
                                self.selected_peer = Some(peer.id);
                                self.peer_selection_explicit = true;
                            }
                            ui.add_space(7.0);
                        }
                    });
            }

            ui.add_space(10.0);
            ui.separator();
            let manual_row_width = ui.available_width();
            if manual_row_width >= 200.0 {
                ui.horizontal(|ui| {
                    let button_width = 92.0;
                    let input_width =
                        (ui.available_width() - button_width - ui.spacing().item_spacing.x)
                            .max(0.0);
                    ui.add(
                        egui::TextEdit::singleline(&mut self.manual_address)
                            .hint_text("手动输入 192.168.1.20")
                            .desired_width(input_width),
                    );
                    if ui
                        .add(egui::Button::new("手动连接").min_size(egui::vec2(button_width, 0.0)))
                        .clicked()
                    {
                        self.add_manual_peer();
                    }
                });
            } else {
                ui.add(
                    egui::TextEdit::singleline(&mut self.manual_address)
                        .hint_text("手动输入 192.168.1.20")
                        .desired_width(manual_row_width),
                );
                if ui.button("手动连接").clicked() {
                    self.add_manual_peer();
                }
            }
            ui.add_space(5.0);
            ui.add(
                egui::Label::new(
                    egui::RichText::new(format!(
                        "未自动发现时，可输入对方 IPv4 地址；端口默认 {}",
                        self.config.transfer_port
                    ))
                    .size(10.5)
                    .color(palette.subtle),
                )
                .wrap(),
            );
        });
    }

    fn show_file_panel(&mut self, ui: &mut egui::Ui, palette: Palette, context: PageContext) {
        card(ui, palette, |ui| {
            panel_heading(
                ui,
                "选择文件",
                if cfg!(target_os = "android") {
                    "通过 Android 系统文件选择器选择要发送的文件"
                } else {
                    "拖放文件到窗口，或从本机选择"
                },
                palette,
            );
            let file_entries = self
                .selected_files
                .iter()
                .map(|path| {
                    let (size, valid) = match std::fs::metadata(path) {
                        Ok(metadata) if metadata.is_file() => (metadata.len(), true),
                        _ => (0, false),
                    };
                    (path.clone(), size, valid)
                })
                .collect::<Vec<_>>();
            let total_size = file_entries.iter().map(|(_, size, _)| *size).sum::<u64>();
            let has_valid_file = file_entries.iter().any(|(_, _, valid)| *valid);

            let mut browse_clicked = false;
            let drop_response = egui::Frame::new()
                .fill(palette.elevated)
                .stroke(egui::Stroke::new(1.0, palette.border))
                .corner_radius(12)
                .inner_margin(18)
                .show(ui, |ui| {
                    ui.set_min_height(if context.compact() { 88.0 } else { 104.0 });
                    ui.vertical_centered(|ui| {
                        ui.label(
                            egui::RichText::new("↓")
                                .size(30.0)
                                .color(palette.accent)
                                .strong(),
                        );
                        ui.label(
                            egui::RichText::new(if cfg!(target_os = "android") {
                                "从系统选择文件"
                            } else {
                                "将文件拖到这里"
                            })
                            .size(15.0)
                            .strong()
                            .color(palette.text),
                        );
                        ui.label(
                            egui::RichText::new(if cfg!(target_os = "android") {
                                "支持同时选择多个文件，优先直接读取系统文件"
                            } else {
                                "支持同时选择多个文件"
                            })
                            .size(11.0)
                            .color(palette.muted),
                        );
                        ui.add_space(7.0);
                        let browse_label = if cfg!(target_os = "android") {
                            "选择文件"
                        } else {
                            "浏览本机文件"
                        };
                        if ui.button(browse_label).clicked() {
                            browse_clicked = true;
                            self.choose_files();
                        }
                    });
                })
                .response
                .interact(egui::Sense::click())
                .on_hover_text(if cfg!(target_os = "android") {
                    "点击打开 Android 系统文件选择器"
                } else {
                    "点击选择文件，也可以直接拖放到窗口"
                });
            if drop_response.clicked() && !browse_clicked {
                self.choose_files();
            }
            if drop_response.hovered() {
                ui.painter().rect_stroke(
                    drop_response.rect,
                    12.0,
                    egui::Stroke::new(1.5, palette.accent),
                    egui::StrokeKind::Inside,
                );
            }

            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("待发送文件")
                        .size(12.0)
                        .strong()
                        .color(palette.text),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if !self.selected_files.is_empty()
                        && ui
                            .small_button("清空")
                            .on_hover_text("移除全部待发送文件")
                            .clicked()
                    {
                        let removed = std::mem::take(&mut self.selected_files);
                        #[cfg(target_os = "android")]
                        for path in removed {
                            crate::android_bridge::remove_picked_file(&path);
                        }
                        #[cfg(not(target_os = "android"))]
                        let _ = removed;
                    }
                });
            });
            ui.add_space(5.0);
            if self.selected_files.is_empty() {
                egui::Frame::new()
                    .fill(palette.background)
                    .stroke(egui::Stroke::new(1.0, palette.border))
                    .corner_radius(9)
                    .inner_margin(12)
                    .show(ui, |ui| {
                        ui.set_min_height(42.0);
                        ui.label(
                            egui::RichText::new("尚未选择文件，选择后会显示在这里")
                                .color(palette.subtle)
                                .italics(),
                        );
                    });
            } else {
                egui::ScrollArea::vertical()
                    .max_height(if context.compact() { 125.0 } else { 165.0 })
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        let mut path_to_remove = None;
                        for (path, size, valid) in &file_entries {
                            ui.push_id(path, |ui| {
                                ui.horizontal(|ui| {
                                    ui.label(
                                        egui::RichText::new("文").color(palette.accent).size(15.0),
                                    );
                                    let action_width = 30.0;
                                    let gap = ui.spacing().item_spacing.x;
                                    let text_width =
                                        (ui.available_width() - action_width - gap).max(0.0);
                                    ui.vertical(|ui| {
                                        ui.set_width(text_width);
                                        ui.add(
                                            egui::Label::new(
                                                egui::RichText::new(
                                                    path.file_name()
                                                        .map(|name| name.to_string_lossy())
                                                        .unwrap_or_default(),
                                                )
                                                .color(palette.text),
                                            )
                                            .truncate(),
                                        );
                                        ui.label(
                                            egui::RichText::new(if *valid {
                                                human_bytes(*size)
                                            } else {
                                                "文件不可用".to_owned()
                                            })
                                            .size(10.0)
                                            .color(
                                                if *valid {
                                                    palette.muted
                                                } else {
                                                    palette.danger
                                                },
                                            ),
                                        );
                                    });
                                    if ui.small_button("×").on_hover_text("移除").clicked() {
                                        path_to_remove = Some(path.clone());
                                    }
                                });
                            });
                            ui.add_space(5.0);
                        }
                        if let Some(path) = path_to_remove {
                            #[cfg(target_os = "android")]
                            crate::android_bridge::remove_picked_file(&path);
                            self.selected_files.retain(|selected| selected != &path);
                        }
                    });
            }

            ui.add_space(10.0);
            let selected_peer = self
                .all_peers()
                .into_iter()
                .find(|peer| Some(peer.id) == self.selected_peer);
            let target_name = selected_peer
                .as_ref()
                .map(|peer| peer.name.as_str())
                .unwrap_or("尚未选择设备");
            egui::Frame::new()
                .fill(palette.elevated)
                .stroke(egui::Stroke::new(1.0, palette.border))
                .corner_radius(10)
                .inner_margin(12)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label(
                                egui::RichText::new("发送至")
                                    .size(10.5)
                                    .color(palette.subtle),
                            );
                            ui.label(
                                egui::RichText::new(target_name)
                                    .color(if selected_peer.is_some() {
                                        palette.text
                                    } else {
                                        palette.muted
                                    })
                                    .strong(),
                            );
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.vertical(|ui| {
                                ui.label(
                                    egui::RichText::new("文件").size(10.5).color(palette.subtle),
                                );
                                ui.label(
                                    egui::RichText::new(format!(
                                        "{} 个 · {}",
                                        self.selected_files.len(),
                                        human_bytes(total_size)
                                    ))
                                    .color(palette.text)
                                    .strong(),
                                );
                            });
                        });
                    });
                });
            ui.add_space(10.0);
            let button_text = if self.selected_files.is_empty() {
                "选择文件后发送".to_owned()
            } else if !has_valid_file {
                "没有可用文件".to_owned()
            } else if selected_peer.is_none() {
                "请选择接收设备".to_owned()
            } else {
                format!("发送到 {}", target_name)
            };
            let can_send = selected_peer.is_some() && has_valid_file;
            let response = ui
                .add_enabled(
                    can_send,
                    egui::Button::new(egui::RichText::new(&button_text).strong().color(
                        if can_send {
                            egui::Color32::WHITE
                        } else {
                            palette.subtle
                        },
                    ))
                    .fill(if can_send {
                        palette.accent
                    } else {
                        palette.elevated
                    })
                    .corner_radius(9)
                    .min_size(egui::vec2(ui.available_width(), 42.0))
                    .truncate(),
                )
                .on_hover_text(if can_send {
                    "开始发送已选择文件"
                } else {
                    "请先选择文件和接收设备"
                });
            if response.clicked() {
                self.send_selected_files();
            }
        });
    }

    fn show_transfers_page(&mut self, ui: &mut egui::Ui, palette: Palette, context: PageContext) {
        let snapshots = self.hub.snapshots();
        let active = snapshots
            .iter()
            .filter(|snapshot| snapshot.state.is_active())
            .count();
        let completed = snapshots
            .iter()
            .filter(|snapshot| snapshot.state == TransferState::Completed)
            .count();

        if context.width >= 680.0 {
            ui.horizontal(|ui| {
                add_page_header(
                    ui,
                    "传输任务",
                    "实时进度、速度和错误详情。传输期间请保持 FastTran 开启。",
                    palette,
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if !snapshots.is_empty() && ui.button("清除已完成").clicked() {
                        self.hub.clear_finished();
                    }
                });
            });
        } else {
            add_page_header(
                ui,
                "传输任务",
                "实时进度、速度和错误详情。传输期间请保持 FastTran 开启。",
                palette,
            );
            if !snapshots.is_empty() && ui.button("清除已完成").clicked() {
                self.hub.clear_finished();
            }
        }
        ui.add_space(18.0);

        let pending_receives: Vec<_> = snapshots
            .iter()
            .filter(|snapshot| {
                snapshot.direction == TransferDirection::Receiving
                    && snapshot.state == TransferState::AwaitingConfirmation
            })
            .cloned()
            .collect();
        if !pending_receives.is_empty() {
            card(ui, palette, |ui| {
                ui.label(
                    egui::RichText::new("确认接收")
                        .size(16.0)
                        .strong()
                        .color(palette.accent),
                );
                ui.label(
                    egui::RichText::new("对方正在等待你的确认，文件尚未开始传输。")
                        .color(palette.muted),
                );
                ui.add_space(6.0);
                for snapshot in pending_receives {
                    ui.horizontal(|ui| {
                        let action_width = 154.0;
                        let gap = ui.spacing().item_spacing.x;
                        let text_width = (ui.available_width() - action_width - gap).max(0.0);
                        ui.vertical(|ui| {
                            ui.set_width(text_width);
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(&snapshot.file_name)
                                        .strong()
                                        .color(palette.text),
                                )
                                .truncate(),
                            );
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(format!(
                                        "{} · {} · {}",
                                        snapshot.peer_name,
                                        snapshot.peer_address,
                                        human_bytes(snapshot.total_bytes)
                                    ))
                                    .size(10.5)
                                    .color(palette.muted),
                                )
                                .truncate(),
                            );
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("确认接收").clicked() {
                                self.hub.accept_incoming(snapshot.id);
                            }
                            if ui.button("拒绝").clicked() {
                                self.hub.reject_incoming(snapshot.id);
                            }
                        });
                    });
                }
            });
            ui.add_space(12.0);
        }

        show_metric_cards(
            ui,
            palette,
            active,
            completed,
            snapshots.len(),
            context.width,
        );
        ui.add_space(16.0);

        if snapshots.is_empty() {
            card(ui, palette, |ui| {
                ui.set_min_height(if context.compact() { 240.0 } else { 330.0 });
                ui.vertical_centered(|ui| {
                    ui.add_space(72.0);
                    egui::Frame::new()
                        .fill(palette.accent_soft)
                        .corner_radius(28)
                        .inner_margin(20)
                        .show(ui, |ui| {
                            ui.label(
                                egui::RichText::new("传")
                                    .size(28.0)
                                    .strong()
                                    .color(palette.accent),
                            );
                        });
                    ui.label(
                        egui::RichText::new("还没有传输任务")
                            .size(18.0)
                            .strong()
                            .color(palette.text),
                    );
                    ui.label(
                        egui::RichText::new("从“发送文件”页面开始，或保持 FastTran 开启以接收文件")
                            .color(palette.muted),
                    );
                    ui.add_space(14.0);
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new("去发送文件")
                                    .strong()
                                    .color(egui::Color32::WHITE),
                            )
                            .fill(palette.accent)
                            .min_size(egui::vec2(132.0, 36.0)),
                        )
                        .clicked()
                    {
                        self.page = Page::Send;
                    }
                    if !self.services_enabled {
                        ui.add_space(6.0);
                        if ui
                            .button(egui::RichText::new("开启服务").color(palette.accent))
                            .clicked()
                        {
                            self.set_services_enabled(true);
                        }
                    }
                });
            });
            return;
        }

        for snapshot in snapshots {
            show_transfer_row(ui, palette, &snapshot, &self.hub, context.width < 900.0);
            ui.add_space(10.0);
        }
    }

    fn show_settings_page(&mut self, ui: &mut egui::Ui, palette: Palette, context: PageContext) {
        add_page_header(
            ui,
            "应用设置",
            "配置本机身份、文件保存位置和网络状态。",
            palette,
        );
        ui.add_space(18.0);

        if context.two_columns() {
            ui.columns(2, |columns| {
                self.show_identity_settings(&mut columns[0], palette);
                columns[0].add_space(12.0);
                self.show_appearance_settings(&mut columns[0], palette);
                columns[0].add_space(12.0);
                self.show_download_settings(&mut columns[1], palette);
                columns[1].add_space(12.0);
                self.show_network_settings(&mut columns[0], palette);
                columns[0].add_space(12.0);
                self.show_security_settings(&mut columns[1], palette);
            });
        } else {
            self.show_identity_settings(ui, palette);
            ui.add_space(12.0);
            self.show_appearance_settings(ui, palette);
            ui.add_space(12.0);
            self.show_download_settings(ui, palette);
            ui.add_space(12.0);
            self.show_network_settings(ui, palette);
            ui.add_space(12.0);
            self.show_security_settings(ui, palette);
        }
    }

    fn show_identity_settings(&mut self, ui: &mut egui::Ui, palette: Palette) {
        card(ui, palette, |ui| {
            panel_heading(ui, "本机设备", "此名称将显示在其他设备上", palette);
            ui.add_space(8.0);
            ui.label("设备名称");
            ui.add(
                egui::TextEdit::singleline(&mut self.device_name_input)
                    .desired_width(ui.available_width())
                    .char_limit(32),
            );
            ui.add_space(8.0);
            if ui.button("保存设备名称").clicked() {
                self.save_device_name();
            }
        });
    }

    fn show_appearance_settings(&mut self, ui: &mut egui::Ui, palette: Palette) {
        card(ui, palette, |ui| {
            panel_heading(ui, "外观", "选择界面主题，文字颜色会同步调整", palette);
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let dark_selected = self.dark_mode;
                let light_selected = !self.dark_mode;
                if ui
                    .add(
                        egui::Button::new(egui::RichText::new("深色").strong().color(
                            if dark_selected {
                                palette.accent
                            } else {
                                palette.text
                            },
                        ))
                        .fill(if dark_selected {
                            palette.accent_soft
                        } else {
                            palette.elevated
                        })
                        .min_size(egui::vec2(88.0, 34.0)),
                    )
                    .clicked()
                    && !dark_selected
                {
                    self.set_dark_mode(true, ui.ctx());
                }
                if ui
                    .add(
                        egui::Button::new(egui::RichText::new("浅色").strong().color(
                            if light_selected {
                                palette.accent
                            } else {
                                palette.text
                            },
                        ))
                        .fill(if light_selected {
                            palette.accent_soft
                        } else {
                            palette.elevated
                        })
                        .min_size(egui::vec2(88.0, 34.0)),
                    )
                    .clicked()
                    && !light_selected
                {
                    self.set_dark_mode(false, ui.ctx());
                }
            });
            ui.add_space(7.0);
            ui.label(
                egui::RichText::new("浅色和深色模式分别使用适合背景的文本、按钮和输入框颜色")
                    .size(10.5)
                    .color(palette.subtle),
            );
        });
    }

    fn show_download_settings(&mut self, ui: &mut egui::Ui, palette: Palette) {
        card(ui, palette, |ui| {
            panel_heading(ui, "接收目录", "收到的文件将保存到此文件夹", palette);
            ui.add_space(8.0);
            let path = self
                .download_dir
                .read()
                .unwrap_or_else(|error| error.into_inner())
                .clone();
            let path_width = ui.available_width();
            egui::Frame::new()
                .fill(palette.elevated)
                .corner_radius(8)
                .inner_margin(10)
                .show(ui, |ui| {
                    ui.set_width((path_width - 20.0).max(0.0));
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(path.display().to_string())
                                .color(palette.text)
                                .monospace(),
                        )
                        .wrap(),
                    );
                });
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                if ui.button("更改目录").clicked() {
                    self.choose_download_dir();
                }
                if ui.button("打开目录").clicked()
                    && let Err(error) = open_path(&path)
                {
                    self.notice = Some(Notice::new(
                        format!("无法打开目录：{error}"),
                        NoticeKind::Error,
                        Duration::from_secs(5),
                    ));
                }
            });
        });
    }

    fn show_network_settings(&self, ui: &mut egui::Ui, palette: Palette) {
        card(ui, palette, |ui| {
            panel_heading(
                ui,
                "网络信息",
                "FastTran 使用局域网广播和 TCP 直连",
                palette,
            );
            ui.add_space(8.0);
            info_row(
                ui,
                palette,
                "设备 ID",
                &self.config.device_id.to_string(),
                true,
            );
            info_row(
                ui,
                palette,
                "接收端口",
                &self
                    .receiver
                    .as_ref()
                    .map(|server| server.local_port().to_string())
                    .unwrap_or_else(|| "未启动".to_owned()),
                false,
            );
            info_row(
                ui,
                palette,
                "发现端口",
                &self
                    .discovery
                    .as_ref()
                    .map(|service| service.local_port().to_string())
                    .unwrap_or_else(|| "未启动".to_owned()),
                false,
            );
            info_row(ui, palette, "本机 IPv4", &local_ipv4_summary(), false);
        });
    }

    fn show_security_settings(&self, ui: &mut egui::Ui, palette: Palette) {
        card(ui, palette, |ui| {
            panel_heading(ui, "安全说明", "当前版本适用于可信局域网", palette);
            ui.add_space(8.0);
            ui.add(
                egui::Label::new(
                    egui::RichText::new(
                        "所有传输均使用 TCP，并在完成后进行 SHA-256 校验。FastTran 会清理危险文件名，且不会让发送方指定接收路径。",
                    )
                    .color(palette.muted),
                )
                .wrap(),
            );
            ui.add_space(6.0);
            ui.add(
                egui::Label::new(
                    egui::RichText::new(
                        "当前版本尚未加密或配对，请勿直接暴露到公网；公网使用建议通过可信 VPN。",
                    )
                    .color(palette.warning),
                )
                .wrap(),
            );
        });
    }

    fn choose_files(&mut self) {
        #[cfg(target_os = "android")]
        {
            if let Err(error) = crate::android_bridge::choose_files() {
                self.notice = Some(Notice::new(
                    format!("无法打开系统文件选择器：{error}"),
                    NoticeKind::Error,
                    Duration::from_secs(5),
                ));
            } else {
                self.notice = Some(Notice::new(
                    "请选择要发送的文件，可多选",
                    NoticeKind::Info,
                    Duration::from_secs(3),
                ));
            }
        }

        #[cfg(not(target_os = "android"))]
        {
            let Some(paths) = rfd::FileDialog::new().pick_files() else {
                return;
            };
            for path in paths {
                if !self.selected_files.contains(&path) {
                    self.selected_files.push(path);
                }
            }
        }
    }

    fn send_selected_files(&mut self) {
        let Some(peer) = self
            .all_peers()
            .into_iter()
            .find(|peer| Some(peer.id) == self.selected_peer)
        else {
            self.notice = Some(Notice::new(
                "请先选择接收设备",
                NoticeKind::Error,
                Duration::from_secs(4),
            ));
            return;
        };

        let sender_name = self
            .shared_config
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .device_name
            .clone();
        let candidates = std::mem::take(&mut self.selected_files);
        let mut retained = Vec::new();
        let mut queued = 0_usize;
        let mut errors = Vec::new();
        for path in candidates {
            match self.sender.enqueue(&peer, &path, sender_name.clone()) {
                Ok(_) => queued += 1,
                Err(error) => {
                    retained.push(path);
                    errors.push(error.to_string());
                }
            }
        }
        self.selected_files = retained;

        if !errors.is_empty() {
            let message = if queued > 0 {
                format!("已加入 {queued} 个文件；失败：{}", errors.join("；"))
            } else {
                errors.join("；")
            };
            self.notice = Some(Notice::new(
                message,
                if queued > 0 {
                    NoticeKind::Info
                } else {
                    NoticeKind::Error
                },
                Duration::from_secs(8),
            ));
        } else {
            self.notice = Some(Notice::new(
                format!("已将 {queued} 个文件加入发送队列"),
                NoticeKind::Success,
                Duration::from_secs(4),
            ));
        }
        if queued > 0 {
            self.page = Page::Transfers;
        }
    }

    fn add_manual_peer(&mut self) {
        let raw = self.manual_address.trim();
        if raw.is_empty() {
            return;
        }
        let (address_text, port_text) = match raw.rsplit_once(':') {
            Some((address, port)) => (address, port.to_owned()),
            None => (raw, self.config.transfer_port.to_string()),
        };
        let Ok(address) = address_text
            .parse::<Ipv4Addr>()
            .map_err(|_| anyhow::anyhow!("请输入有效的 IPv4 地址"))
        else {
            self.notice = Some(Notice::new(
                "请输入有效格式，例如 192.168.1.20:45455",
                NoticeKind::Error,
                Duration::from_secs(5),
            ));
            return;
        };
        let port = match port_text.parse::<u16>() {
            Ok(port) => port,
            Err(_) => {
                self.notice = Some(Notice::new(
                    "端口格式无效，请输入 1-65535 的数字",
                    NoticeKind::Error,
                    Duration::from_secs(5),
                ));
                return;
            }
        };
        if port == 0 {
            self.notice = Some(Notice::new(
                "端口不能为 0",
                NoticeKind::Error,
                Duration::from_secs(4),
            ));
            return;
        }

        let peer = Peer::manual(address, port);
        self.manual_peers
            .retain(|existing| existing.address != address || existing.transfer_port != port);
        self.selected_peer = Some(peer.id);
        self.peer_selection_explicit = true;
        self.manual_peers.push(peer);
        self.manual_address.clear();
    }

    fn set_dark_mode(&mut self, dark_mode: bool, context: &egui::Context) {
        self.dark_mode = dark_mode;
        self.config.dark_mode = dark_mode;
        set_theme(context, dark_mode);
        #[cfg(target_os = "android")]
        crate::android_bridge::set_system_bar_style(dark_mode);
        if let Err(error) = self.config.save_to_path(&self.config_file) {
            tracing::warn!(%error, "failed to persist appearance preference");
            #[cfg(not(target_os = "android"))]
            {
                self.notice = Some(Notice::new(
                    format!("主题已切换，但保存失败：{error}"),
                    NoticeKind::Error,
                    Duration::from_secs(5),
                ));
            }
        }
    }

    fn save_device_name(&mut self) {
        let name = normalize_device_name(&self.device_name_input);
        self.device_name_input = name.clone();
        self.config.device_name = name;
        *self
            .shared_config
            .write()
            .unwrap_or_else(|error| error.into_inner()) = self.config.clone();
        match self.config.save_to_path(&self.config_file) {
            Ok(()) => {
                self.notice = Some(Notice::new(
                    "设备名称已保存，其他设备刷新后会看到新名称",
                    NoticeKind::Success,
                    Duration::from_secs(5),
                ))
            }
            Err(error) => {
                self.notice = Some(Notice::new(
                    format!("保存失败：{error}"),
                    NoticeKind::Error,
                    Duration::from_secs(6),
                ))
            }
        }
    }

    fn choose_download_dir(&mut self) {
        #[cfg(target_os = "android")]
        {
            self.notice = Some(Notice::new(
                "Android 版本使用应用专属外部目录保存接收文件",
                NoticeKind::Info,
                Duration::from_secs(5),
            ));
        }

        #[cfg(not(target_os = "android"))]
        {
            let Some(path) = rfd::FileDialog::new()
                .set_title("选择 FastTran 接收目录")
                .pick_folder()
            else {
                return;
            };
            if let Err(error) = std::fs::create_dir_all(&path) {
                self.notice = Some(Notice::new(
                    format!("无法使用该目录：{error}"),
                    NoticeKind::Error,
                    Duration::from_secs(6),
                ));
                return;
            }

            self.config.download_dir = path.clone();
            *self
                .download_dir
                .write()
                .unwrap_or_else(|error| error.into_inner()) = path.clone();
            let mut shared = self
                .shared_config
                .write()
                .unwrap_or_else(|error| error.into_inner());
            shared.download_dir = path.clone();
            drop(shared);

            match self.config.save_to_path(&self.config_file) {
                Ok(()) => {
                    self.notice = Some(Notice::new(
                        "接收目录已更新",
                        NoticeKind::Success,
                        Duration::from_secs(4),
                    ))
                }
                Err(error) => {
                    self.notice = Some(Notice::new(
                        format!("设置已生效，但保存失败：{error}"),
                        NoticeKind::Error,
                        Duration::from_secs(6),
                    ))
                }
            }
        }
    }

    fn all_peers(&self) -> Vec<Peer> {
        let mut peers = self.discovered_peers.clone();
        for manual in &self.manual_peers {
            if !peers.iter().any(|peer| {
                peer.address == manual.address && peer.transfer_port == manual.transfer_port
            }) {
                peers.push(manual.clone());
            }
        }
        peers
    }

    fn show_notice(&mut self, ui: &mut egui::Ui, palette: Palette) {
        if self
            .notice
            .as_ref()
            .is_some_and(|notice| Instant::now() >= notice.expires_at)
        {
            self.notice = None;
        }
        let Some(notice) = self.notice.as_ref() else {
            return;
        };
        let message = notice.message.clone();
        let color = match notice.kind {
            NoticeKind::Info => palette.accent,
            NoticeKind::Success => palette.success,
            NoticeKind::Error => palette.danger,
        };
        let bottom_inset = self.safe_insets()[2].clamp(0.0, 40.0);
        egui::Panel::bottom("notice")
            .exact_size(40.0 + bottom_inset)
            .frame(
                egui::Frame::new()
                    .fill(palette.surface)
                    .stroke(egui::Stroke::new(1.0, palette.border))
                    .inner_margin(egui::Margin {
                        left: 20,
                        right: 20,
                        top: 10,
                        bottom: (10.0 + bottom_inset).round().clamp(0.0, 127.0) as i8,
                    }),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("●").color(color));
                    let message_width = (ui.available_width() - 36.0).max(0.0);
                    ui.add_sized(
                        [message_width, 0.0],
                        egui::Label::new(
                            egui::RichText::new(&message).color(palette.text).size(12.0),
                        )
                        .truncate(),
                    )
                    .on_hover_text(&message);
                    if ui.small_button("×").clicked() {
                        self.notice = None;
                    }
                });
            });
    }

    fn palette(&self) -> Palette {
        if self.dark_mode {
            Palette::dark()
        } else {
            Palette::light()
        }
    }
}

impl eframe::App for FastTranApp {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.render_ui(ui, frame);
    }
}

#[derive(Debug, Clone, Copy)]
struct Palette {
    background: egui::Color32,
    sidebar: egui::Color32,
    surface: egui::Color32,
    elevated: egui::Color32,
    text: egui::Color32,
    muted: egui::Color32,
    subtle: egui::Color32,
    border: egui::Color32,
    accent: egui::Color32,
    accent_soft: egui::Color32,
    success: egui::Color32,
    warning: egui::Color32,
    danger: egui::Color32,
}

impl Palette {
    fn dark() -> Self {
        Self {
            background: egui::Color32::from_rgb(13, 18, 28),
            sidebar: egui::Color32::from_rgb(17, 24, 36),
            surface: egui::Color32::from_rgb(21, 29, 42),
            elevated: egui::Color32::from_rgb(27, 37, 53),
            text: egui::Color32::from_rgb(235, 242, 250),
            muted: egui::Color32::from_rgb(156, 171, 190),
            subtle: egui::Color32::from_rgb(132, 149, 171),
            border: egui::Color32::from_rgb(43, 57, 76),
            accent: egui::Color32::from_rgb(70, 151, 255),
            accent_soft: egui::Color32::from_rgb(31, 61, 101),
            success: egui::Color32::from_rgb(44, 196, 135),
            warning: egui::Color32::from_rgb(244, 177, 68),
            danger: egui::Color32::from_rgb(244, 91, 105),
        }
    }

    fn light() -> Self {
        Self {
            background: egui::Color32::from_rgb(244, 247, 251),
            sidebar: egui::Color32::from_rgb(255, 255, 255),
            surface: egui::Color32::WHITE,
            elevated: egui::Color32::from_rgb(247, 249, 252),
            text: egui::Color32::from_rgb(24, 34, 49),
            muted: egui::Color32::from_rgb(91, 108, 130),
            subtle: egui::Color32::from_rgb(105, 120, 140),
            border: egui::Color32::from_rgb(218, 226, 236),
            accent: egui::Color32::from_rgb(38, 119, 221),
            accent_soft: egui::Color32::from_rgb(226, 239, 255),
            success: egui::Color32::from_rgb(26, 156, 101),
            warning: egui::Color32::from_rgb(205, 128, 18),
            danger: egui::Color32::from_rgb(215, 55, 70),
        }
    }
}

struct ServiceSwitch<'a> {
    value: &'a mut bool,
    label: &'a str,
    palette: Palette,
}

impl egui::Widget for ServiceSwitch<'_> {
    fn ui(self, ui: &mut egui::Ui) -> egui::Response {
        let mut response = ui.allocate_response(egui::vec2(82.0, 24.0), egui::Sense::click());
        let rect = response.rect;
        let track = egui::Rect::from_center_size(
            egui::pos2(rect.left() + 20.0, rect.center().y),
            egui::vec2(40.0, 22.0),
        );
        let track_color = if *self.value {
            self.palette.accent
        } else if response.hovered() {
            self.palette.muted
        } else {
            self.palette.subtle
        };
        let track_stroke = egui::Stroke::new(1.0, track_color);
        ui.painter().rect(
            track,
            11.0,
            track_color,
            track_stroke,
            egui::StrokeKind::Inside,
        );
        let knob_x = if *self.value {
            track.right() - 11.0
        } else {
            track.left() + 11.0
        };
        let knob_center = egui::pos2(knob_x, track.center().y);
        ui.painter()
            .circle_filled(knob_center, 8.0, egui::Color32::WHITE);
        ui.painter().circle_stroke(
            knob_center,
            8.0,
            egui::Stroke::new(1.0, self.palette.border),
        );
        let font_id = egui::TextStyle::Body.resolve(ui.style());
        ui.painter().text(
            egui::pos2(track.right() + 7.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            self.label,
            font_id,
            self.palette.text,
        );

        if response.clicked() {
            *self.value = !*self.value;
            response.mark_changed();
        }
        response.widget_info(|| {
            egui::WidgetInfo::selected(
                egui::WidgetType::Button,
                ui.is_enabled(),
                *self.value,
                self.label,
            )
        });
        response
    }
}

fn card<R>(
    ui: &mut egui::Ui,
    palette: Palette,
    contents: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<R> {
    egui::Frame::new()
        .fill(palette.surface)
        .stroke(egui::Stroke::new(1.0, palette.border))
        .corner_radius(13)
        .inner_margin(15)
        .show(ui, |ui| {
            let content_width = ui.available_width();
            ui.set_min_width(content_width);
            ui.set_max_width(content_width);
            contents(ui)
        })
}

fn panel_heading(ui: &mut egui::Ui, title: &str, subtitle: &str, palette: Palette) {
    ui.add(egui::Label::new(
        egui::RichText::new(title)
            .size(17.0)
            .strong()
            .color(palette.text),
    ));
    ui.add(
        egui::Label::new(
            egui::RichText::new(subtitle)
                .size(11.0)
                .color(palette.muted),
        )
        .wrap(),
    );
}

fn add_page_header(
    ui: &mut egui::Ui,
    title: &str,
    subtitle: &str,
    palette: Palette,
) -> egui::InnerResponse<()> {
    ui.vertical(|ui| {
        ui.add(egui::Label::new(
            egui::RichText::new(title)
                .size(29.0)
                .strong()
                .color(palette.text),
        ));
        ui.add(
            egui::Label::new(
                egui::RichText::new(subtitle)
                    .size(13.0)
                    .color(palette.muted),
            )
            .wrap(),
        );
    })
}

fn metric_card(
    ui: &mut egui::Ui,
    palette: Palette,
    label: &str,
    value: String,
    color: egui::Color32,
) {
    egui::Frame::new()
        .fill(palette.surface)
        .stroke(egui::Stroke::new(1.0, palette.border))
        .corner_radius(11)
        .inner_margin(egui::Margin::symmetric(18, 13))
        .show(ui, |ui| {
            let content_width = ui.available_width();
            ui.set_min_width(content_width);
            ui.set_max_width(content_width);
            ui.label(egui::RichText::new(value).size(23.0).strong().color(color));
            ui.label(egui::RichText::new(label).size(11.0).color(palette.muted));
        });
}

fn show_metric_cards(
    ui: &mut egui::Ui,
    palette: Palette,
    active: usize,
    completed: usize,
    total: usize,
    available_width: f32,
) {
    if available_width >= 600.0 {
        ui.columns(3, |columns| {
            metric_card(
                &mut columns[0],
                palette,
                "进行中",
                active.to_string(),
                palette.accent,
            );
            metric_card(
                &mut columns[1],
                palette,
                "已完成",
                completed.to_string(),
                palette.success,
            );
            metric_card(
                &mut columns[2],
                palette,
                "总任务",
                total.to_string(),
                palette.text,
            );
        });
    } else if available_width >= 360.0 {
        ui.columns(2, |columns| {
            metric_card(
                &mut columns[0],
                palette,
                "进行中",
                active.to_string(),
                palette.accent,
            );
            columns[0].add_space(8.0);
            metric_card(
                &mut columns[0],
                palette,
                "总任务",
                total.to_string(),
                palette.text,
            );
            metric_card(
                &mut columns[1],
                palette,
                "已完成",
                completed.to_string(),
                palette.success,
            );
        });
    } else {
        metric_card(ui, palette, "进行中", active.to_string(), palette.accent);
        ui.add_space(8.0);
        metric_card(
            ui,
            palette,
            "已完成",
            completed.to_string(),
            palette.success,
        );
        ui.add_space(8.0);
        metric_card(ui, palette, "总任务", total.to_string(), palette.text);
    }
}

fn show_transfer_row(
    ui: &mut egui::Ui,
    palette: Palette,
    snapshot: &TransferSnapshot,
    hub: &TransferHub,
    compact: bool,
) {
    let state_color = match snapshot.state {
        TransferState::Completed => palette.success,
        TransferState::Failed => palette.danger,
        TransferState::Cancelled => palette.warning,
        _ => palette.accent,
    };
    let direction = match snapshot.direction {
        TransferDirection::Sending => "↑  发送",
        TransferDirection::Receiving => "↓  接收",
    };
    let amount = if snapshot.state.is_active() {
        format!(
            "{} / {}",
            human_bytes(snapshot.transferred_bytes),
            human_bytes(snapshot.total_bytes)
        )
    } else {
        human_duration(snapshot.elapsed)
    };
    egui::Frame::new()
        .fill(palette.surface)
        .stroke(egui::Stroke::new(1.0, palette.border))
        .corner_radius(11)
        .inner_margin(14)
        .show(ui, |ui| {
            let content_width = ui.available_width();
            ui.set_min_width(content_width);
            ui.set_max_width(content_width);

            if compact {
                ui.horizontal(|ui| {
                    transfer_direction_icon(ui, palette, snapshot, state_color);
                    let text_width = ui.available_width().max(0.0);
                    ui.vertical(|ui| {
                        ui.set_width(text_width);
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(&snapshot.file_name)
                                    .strong()
                                    .color(palette.text),
                            )
                            .truncate(),
                        );
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(format!(
                                    "{direction}  ·  {}  ·  {}",
                                    snapshot.peer_name, snapshot.peer_address
                                ))
                                .size(10.5)
                                .color(palette.muted),
                            )
                            .truncate(),
                        );
                    });
                });
                ui.add_space(7.0);
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        egui::RichText::new(snapshot.state.label())
                            .color(state_color)
                            .strong(),
                    );
                    ui.label(egui::RichText::new(&amount).size(10.5).color(palette.muted));
                    transfer_action_buttons(ui, snapshot, hub);
                });
            } else {
                ui.horizontal(|ui| {
                    transfer_direction_icon(ui, palette, snapshot, state_color);
                    let text_width = (ui.available_width() - 245.0).max(0.0);
                    ui.vertical(|ui| {
                        ui.set_width(text_width);
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(&snapshot.file_name)
                                    .strong()
                                    .color(palette.text),
                            )
                            .truncate(),
                        );
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(format!(
                                    "{direction}  ·  {}  ·  {}",
                                    snapshot.peer_name, snapshot.peer_address
                                ))
                                .size(10.5)
                                .color(palette.muted),
                            )
                            .truncate(),
                        );
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        transfer_action_buttons(ui, snapshot, hub);
                        ui.label(
                            egui::RichText::new(snapshot.state.label())
                                .color(state_color)
                                .strong(),
                        );
                        ui.label(egui::RichText::new(&amount).size(10.5).color(palette.muted));
                    });
                });
            }

            ui.add_space(9.0);
            ui.add(
                egui::ProgressBar::new(snapshot.progress)
                    .fill(state_color)
                    .desired_height(6.0),
            );
            ui.add_space(5.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    egui::RichText::new(format!("{:.0}%", snapshot.progress * 100.0))
                        .size(10.0)
                        .color(palette.muted),
                );
                if snapshot.state.is_active() {
                    ui.label(
                        egui::RichText::new(human_speed(snapshot.speed_bps))
                            .size(10.0)
                            .color(palette.muted),
                    );
                }
                if let Some(error) = &snapshot.error {
                    let response = ui.add(
                        egui::Label::new(
                            egui::RichText::new(error).size(10.0).color(palette.danger),
                        )
                        .truncate(),
                    );
                    response.on_hover_text(error);
                }
            });
        });
}

fn transfer_direction_icon(
    ui: &mut egui::Ui,
    palette: Palette,
    snapshot: &TransferSnapshot,
    state_color: egui::Color32,
) {
    egui::Frame::new()
        .fill(palette.elevated)
        .corner_radius(9)
        .inner_margin(10)
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new(match snapshot.direction {
                    TransferDirection::Sending => "↑",
                    TransferDirection::Receiving => "↓",
                })
                .size(20.0)
                .strong()
                .color(state_color),
            );
        });
}

fn transfer_action_buttons(ui: &mut egui::Ui, snapshot: &TransferSnapshot, hub: &TransferHub) {
    let waiting_for_confirmation = snapshot.direction == TransferDirection::Receiving
        && snapshot.state == TransferState::AwaitingConfirmation;
    if waiting_for_confirmation {
        ui.label(egui::RichText::new("请在上方确认").color(ui.visuals().weak_text_color()));
    } else if snapshot.state.is_active() {
        if ui.button("取消").clicked() {
            hub.request_cancel(snapshot.id);
        }
    } else if snapshot.state == TransferState::Completed
        && snapshot.direction == TransferDirection::Receiving
        && ui.button("打开").clicked()
    {
        open_received_path(&snapshot.file_path);
    }
    if snapshot.state.is_finished() && ui.small_button("×").clicked() {
        hub.remove(snapshot.id);
    }
}

fn info_row(ui: &mut egui::Ui, palette: Palette, label: &str, value: &str, selectable: bool) {
    if ui.available_width() < 240.0 {
        ui.label(egui::RichText::new(label).color(palette.muted));
        if selectable {
            ui.horizontal(|ui| {
                let gap = ui.spacing().item_spacing.x;
                let text_width = (ui.available_width() - 44.0 - gap).max(0.0);
                ui.add_sized(
                    [text_width, 0.0],
                    egui::Label::new(egui::RichText::new(value).monospace()).truncate(),
                );
                if ui.small_button("复制").clicked() {
                    ui.ctx().copy_text(value.to_owned());
                }
            });
        } else {
            ui.add(
                egui::Label::new(egui::RichText::new(value).color(palette.text).monospace()).wrap(),
            );
        }
        return;
    }

    ui.horizontal(|ui| {
        let label_width = 82.0;
        ui.add_sized(
            [label_width, 0.0],
            egui::Label::new(egui::RichText::new(label).color(palette.muted)),
        );
        let value_width = ui.available_width().max(0.0);
        if selectable {
            ui.horizontal(|ui| {
                let gap = ui.spacing().item_spacing.x;
                let text_width = (value_width - 44.0 - gap).max(0.0);
                ui.add_sized(
                    [text_width, 0.0],
                    egui::Label::new(egui::RichText::new(value).monospace()).truncate(),
                );
                if ui.small_button("复制").clicked() {
                    ui.ctx().copy_text(value.to_owned());
                }
            });
        } else {
            ui.add_sized(
                [value_width, 0.0],
                egui::Label::new(egui::RichText::new(value).color(palette.text).monospace()).wrap(),
            );
        }
    });
}

fn local_ipv4_summary() -> String {
    let mut addresses = Vec::new();
    for interface in if_addrs::get_if_addrs().unwrap_or_default() {
        if interface.is_loopback() {
            continue;
        }
        if let std::net::IpAddr::V4(address) = interface.ip()
            && !address.is_unspecified()
        {
            addresses.push(address.to_string());
        }
    }
    addresses.sort();
    addresses.dedup();
    if addresses.is_empty() {
        "未检测到".to_owned()
    } else {
        addresses.join(", ")
    }
}

fn open_path(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "android")]
    {
        crate::android_bridge::open_path(path)
    }
    #[cfg(not(target_os = "android"))]
    {
        open::that(path).map_err(|error| error.to_string())
    }
}

fn open_received_path(path: &Path) {
    #[cfg(target_os = "android")]
    if let Err(error) = crate::android_bridge::open_path(path) {
        tracing::warn!(%error, "failed to open received file on Android");
    }

    #[cfg(not(target_os = "android"))]
    if let Some(parent) = path.parent()
        && let Err(error) = open::that(parent)
    {
        tracing::warn!(%error, "failed to open received file directory");
    }
}

fn short_id(id: Uuid) -> String {
    id.to_string()
        .split('-')
        .next()
        .unwrap_or("UNKNOWN")
        .to_uppercase()
}

fn app_icon() -> egui::IconData {
    const SIZE: usize = 64;
    let mut rgba = vec![0_u8; SIZE * SIZE * 4];

    for y in 0..SIZE {
        for x in 0..SIZE {
            let index = (y * SIZE + x) * 4;
            let center = SIZE as f32 / 2.0;
            let radius = 11.0_f32;
            let dx = (x as f32 - center).abs() - (center - radius);
            let dy = (y as f32 - center).abs() - (center - radius);
            let outside_corner = dx.max(0.0).powi(2) + dy.max(0.0).powi(2) > radius.powi(2);
            if outside_corner {
                continue;
            }

            let vertical = y as f32 / SIZE as f32;
            rgba[index] = 35 + (25.0 * vertical) as u8;
            rgba[index + 1] = 113 + (31.0 * vertical) as u8;
            rgba[index + 2] = 232 - (18.0 * vertical) as u8;
            rgba[index + 3] = 255;

            let in_vertical = (21..=48).contains(&y);
            let in_top = (17..=23).contains(&y) && (20..=45).contains(&x);
            let in_middle = (29..=35).contains(&y) && (20..=40).contains(&x);
            if (20..=28).contains(&x) && (in_vertical || in_top || in_middle) {
                rgba[index..index + 4].copy_from_slice(&[245, 249, 255, 255]);
            }
        }
    }

    egui::IconData {
        rgba,
        width: SIZE as u32,
        height: SIZE as u32,
    }
}

fn install_system_font(context: &egui::Context) {
    let candidates = [
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\msyhl.ttc",
        r"C:\Windows\Fonts\simhei.ttf",
        "/system/fonts/NotoSansCJK-Regular.ttc",
        "/system/fonts/NotoSansSC-Regular.otf",
        "/system/fonts/DroidSansFallback.ttf",
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/Hiragino Sans GB.ttc",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
        "/usr/share/fonts/truetype/arphic/ukai.ttc",
    ];
    let Some(path) = candidates
        .into_iter()
        .find(|path| Path::new(path).is_file())
    else {
        return;
    };
    let Ok(bytes) = std::fs::read(path) else {
        return;
    };

    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "system_cjk".to_owned(),
        egui::FontData::from_owned(bytes).into(),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        if let Some(names) = fonts.families.get_mut(&family) {
            names.push("system_cjk".to_owned());
        }
    }
    context.set_fonts(fonts);
}

fn set_theme(context: &egui::Context, dark: bool) {
    let mut visuals = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    let (text, muted, input_bg, surface, elevated, border, accent, accent_soft, warning, danger) =
        if dark {
            (
                egui::Color32::from_rgb(235, 242, 250),
                egui::Color32::from_rgb(156, 171, 190),
                egui::Color32::from_rgb(16, 24, 36),
                egui::Color32::from_rgb(21, 29, 42),
                egui::Color32::from_rgb(29, 40, 56),
                egui::Color32::from_rgb(49, 65, 85),
                egui::Color32::from_rgb(70, 151, 255),
                egui::Color32::from_rgb(31, 61, 101),
                egui::Color32::from_rgb(244, 177, 68),
                egui::Color32::from_rgb(244, 91, 105),
            )
        } else {
            (
                egui::Color32::from_rgb(24, 34, 49),
                egui::Color32::from_rgb(91, 108, 130),
                egui::Color32::from_rgb(248, 250, 252),
                egui::Color32::WHITE,
                egui::Color32::from_rgb(247, 249, 252),
                egui::Color32::from_rgb(218, 226, 236),
                egui::Color32::from_rgb(38, 119, 221),
                egui::Color32::from_rgb(226, 239, 255),
                egui::Color32::from_rgb(205, 128, 18),
                egui::Color32::from_rgb(215, 55, 70),
            )
        };

    visuals.window_corner_radius = 0.into();
    visuals.panel_fill = egui::Color32::TRANSPARENT;
    visuals.window_fill = surface;
    visuals.text_edit_bg_color = Some(input_bg);
    visuals.extreme_bg_color = input_bg;
    visuals.faint_bg_color = if dark {
        egui::Color32::from_rgb(18, 25, 36)
    } else {
        egui::Color32::from_rgb(241, 245, 249)
    };
    visuals.code_bg_color = elevated;
    visuals.override_text_color = None;
    visuals.weak_text_color = Some(muted);
    visuals.hyperlink_color = accent;
    visuals.warn_fg_color = warning;
    visuals.error_fg_color = danger;
    visuals.button_frame = true;
    visuals.interact_cursor = Some(egui::CursorIcon::PointingHand);
    visuals.disabled_alpha = if dark { 0.55 } else { 0.45 };

    visuals.widgets.noninteractive.bg_fill = surface;
    visuals.widgets.noninteractive.weak_bg_fill = surface;
    visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, border);
    visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, text);

    visuals.widgets.inactive.bg_fill = elevated;
    visuals.widgets.inactive.weak_bg_fill = surface;
    visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, border);
    visuals.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, text);

    visuals.widgets.hovered.bg_fill = accent_soft;
    visuals.widgets.hovered.weak_bg_fill = accent_soft;
    visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, accent);
    visuals.widgets.hovered.fg_stroke = egui::Stroke::new(1.0, text);

    visuals.widgets.active.bg_fill = accent;
    visuals.widgets.active.weak_bg_fill = accent;
    visuals.widgets.active.bg_stroke = egui::Stroke::new(1.0, accent);
    visuals.widgets.active.fg_stroke = egui::Stroke::new(1.0, egui::Color32::WHITE);

    visuals.selection.bg_fill = accent.additive();
    context.set_visuals(visuals);
}

fn else_color() -> egui::Color32 {
    egui::Color32::TRANSPARENT
}

#[cfg(test)]
mod tests {
    use super::PageContext;

    #[test]
    fn layout_uses_single_column_on_small_or_short_windows() {
        assert!(
            !PageContext {
                width: 739.0,
                height: 700.0
            }
            .two_columns()
        );
        assert!(
            !PageContext {
                width: 900.0,
                height: 469.0
            }
            .two_columns()
        );
        assert!(
            PageContext {
                width: 900.0,
                height: 700.0
            }
            .two_columns()
        );
    }
}
