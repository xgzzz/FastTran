use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use tokio::fs::{self, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::model::{ConfirmationDecision, TransferHub, TransferJob, TransferState};
use crate::network::AppEvent;
use crate::protocol::{
    TransferRequest, TransferResponse, read_frame, sanitize_file_name, write_frame,
};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(15);
const USER_CONFIRMATION_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const COPY_BUFFER_SIZE: usize = 2 * 1024 * 1024;

pub struct ReceiverServer {
    local_port: u16,
    shutdown: Arc<Notify>,
    task: JoinHandle<()>,
}

impl ReceiverServer {
    pub async fn start(
        bind_port: u16,
        download_dir: Arc<RwLock<PathBuf>>,
        hub: Arc<TransferHub>,
        events: tokio::sync::mpsc::UnboundedSender<AppEvent>,
    ) -> Result<Self> {
        let listener = TcpListener::bind(("0.0.0.0", bind_port))
            .await
            .with_context(|| format!("无法在端口 {bind_port} 启动接收服务"))?;
        let local_port = listener.local_addr()?.port();
        let shutdown = Arc::new(Notify::new());
        let task = tokio::spawn(run_server(
            listener,
            download_dir,
            hub,
            events,
            shutdown.clone(),
        ));

        Ok(Self {
            local_port,
            shutdown,
            task,
        })
    }

    pub fn local_port(&self) -> u16 {
        self.local_port
    }
}

impl Drop for ReceiverServer {
    fn drop(&mut self) {
        self.shutdown.notify_waiters();
        self.task.abort();
    }
}

async fn run_server(
    listener: TcpListener,
    download_dir: Arc<RwLock<PathBuf>>,
    hub: Arc<TransferHub>,
    events: tokio::sync::mpsc::UnboundedSender<AppEvent>,
    shutdown: Arc<Notify>,
) {
    loop {
        let accepted = tokio::select! {
            _ = shutdown.notified() => return,
            accepted = listener.accept() => accepted,
        };

        match accepted {
            Ok((stream, address)) => {
                let download_dir = download_dir.clone();
                let hub = hub.clone();
                let events = events.clone();
                let shutdown = shutdown.clone();
                tokio::spawn(async move {
                    if let Err(error) = receive_connection(
                        stream,
                        address.to_string(),
                        download_dir,
                        hub,
                        events,
                        shutdown,
                    )
                    .await
                    {
                        tracing::debug!(%address, %error, "incoming connection closed");
                    }
                });
            }
            Err(error) => {
                let _ = events.send(AppEvent::Error {
                    scope: "接收服务",
                    message: error.to_string(),
                });
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

async fn receive_connection(
    mut stream: TcpStream,
    peer_address: String,
    download_dir: Arc<RwLock<PathBuf>>,
    hub: Arc<TransferHub>,
    events: tokio::sync::mpsc::UnboundedSender<AppEvent>,
    shutdown: Arc<Notify>,
) -> Result<()> {
    let _ = stream.set_nodelay(true);
    let request: TransferRequest = tokio::select! {
        _ = shutdown.notified() => bail!("receive service stopped"),
        result = tokio::time::timeout(HANDSHAKE_TIMEOUT, read_frame(&mut stream)) => result
            .context("等待发送端握手超时")??,
    };

    if let Err(error) = request.validate() {
        send_rejection(&mut stream, request.transfer_id, error.to_string()).await;
        bail!(error);
    }
    if hub.contains(request.transfer_id) {
        send_rejection(&mut stream, request.transfer_id, "重复的传输编号").await;
        bail!("duplicate transfer id");
    }

    let download_path = download_dir
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    fs::create_dir_all(&download_path)
        .await
        .with_context(|| format!("无法创建接收目录: {}", download_path.display()))?;
    let safe_name = sanitize_file_name(&request.file_name);
    let destination = unique_destination(&download_path, &safe_name);
    let part_path = destination.with_extension(format!(
        "{}fasttran-{}.part",
        destination
            .extension()
            .and_then(|value| value.to_str())
            .map(|value| format!("{value}."))
            .unwrap_or_default(),
        request.transfer_id
    ));

    let sender_name = clean_device_name(&request.sender_name);
    let job = Arc::new(TransferJob::receiving(
        request.transfer_id,
        sender_name,
        peer_address,
        destination.clone(),
        request.file_size,
    ));
    hub.register(job.clone());
    job.set_state(TransferState::AwaitingConfirmation);

    let (receive_decision_tx, receive_decision_rx) = tokio::sync::oneshot::channel();
    hub.register_pending_receive(request.transfer_id, receive_decision_tx);
    if job.is_cancel_requested() {
        hub.discard_pending_receive(request.transfer_id);
        job.fail("服务已关闭");
        send_rejection(&mut stream, request.transfer_id, "服务已关闭").await;
        bail!("receive service stopped");
    }
    let pending = TransferResponse::pending(request.transfer_id, "等待用户确认接收");
    if let Err(error) = send_response(&mut stream, &pending, "发送接收请求确认超时").await
    {
        hub.discard_pending_receive(request.transfer_id);
        job.fail(error.to_string());
        return Err(error);
    }
    let _ = events.send(AppEvent::Incoming {
        transfer_id: request.transfer_id,
    });

    let decision = tokio::select! {
        _ = shutdown.notified() => {
            hub.discard_pending_receive(request.transfer_id);
            job.request_cancel();
            job.fail("服务已关闭");
            bail!("receive service stopped");
        }
        result = tokio::time::timeout(USER_CONFIRMATION_TIMEOUT, receive_decision_rx) => {
            match result {
                Ok(Ok(decision)) => decision,
                Ok(Err(_)) => {
                    hub.discard_pending_receive(request.transfer_id);
                    job.fail("接收确认通道已关闭");
                    send_rejection(&mut stream, request.transfer_id, "接收确认通道已关闭").await;
                    bail!("receive confirmation channel closed");
                }
                Err(_) => {
                    hub.discard_pending_receive(request.transfer_id);
                    job.fail("接收确认等待超时");
                    send_rejection(&mut stream, request.transfer_id, "接收确认等待超时").await;
                    bail!("receive confirmation timed out");
                }
            }
        }
    };
    if decision != ConfirmationDecision::Accept {
        job.fail("用户拒绝接收");
        send_rejection(&mut stream, request.transfer_id, "用户拒绝接收").await;
        bail!("user rejected incoming transfer");
    }

    let accepted = TransferResponse::accepted(request.transfer_id);
    if let Err(error) = send_response(&mut stream, &accepted, "发送接收确认超时").await {
        job.fail(error.to_string());
        return Err(error);
    }

    job.set_state(TransferState::Receiving);
    let result = receive_payload(
        &mut stream,
        &part_path,
        request.file_size,
        &request.sha256,
        &job,
        &shutdown,
    )
    .await;

    if let Err(error) = result {
        let _ = fs::remove_file(&part_path).await;
        job.fail(error.to_string());
        send_rejection(
            &mut stream,
            request.transfer_id,
            if job.is_cancel_requested() {
                "传输已取消".to_owned()
            } else {
                error.to_string()
            },
        )
        .await;
        return Err(error);
    }

    if job.is_cancel_requested() {
        let _ = fs::remove_file(&part_path).await;
        job.fail("传输已取消");
        send_rejection(&mut stream, request.transfer_id, "传输已取消").await;
        bail!("transfer cancelled before commit");
    }

    if let Err(error) = fs::rename(&part_path, &destination).await {
        let _ = fs::remove_file(&part_path).await;
        job.fail(format!("无法保存文件: {error}"));
        send_rejection(
            &mut stream,
            request.transfer_id,
            format!("无法保存文件: {error}"),
        )
        .await;
        return Err(error).context("无法提交接收文件");
    }

    let completed = job.finish();
    let response = TransferResponse::accepted(request.transfer_id);
    if let Err(error) = send_response(&mut stream, &response, "发送完成确认超时").await {
        tracing::warn!(%error, transfer_id = %request.transfer_id, "receiver could not send final acknowledgement");
    }
    if completed {
        let _ = events.send(AppEvent::Received {
            transfer_id: request.transfer_id,
            file_path: destination,
        });
    }
    Ok(())
}

async fn receive_payload(
    stream: &mut TcpStream,
    part_path: &Path,
    expected_size: u64,
    expected_digest: &str,
    job: &TransferJob,
    shutdown: &Notify,
) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(part_path)
        .await
        .with_context(|| format!("无法创建临时文件: {}", part_path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; COPY_BUFFER_SIZE];
    let mut received = 0_u64;

    loop {
        if job.is_cancel_requested() {
            bail!("transfer cancelled");
        }
        let count = tokio::select! {
            _ = shutdown.notified() => bail!("receive service stopped"),
            _ = job.wait_for_cancel() => bail!("transfer cancelled"),
            result = stream.read(&mut buffer) => result.context("接收文件数据失败")?,
        };
        if count == 0 {
            break;
        }

        received = received
            .checked_add(count as u64)
            .context("接收文件大小溢出")?;
        if received > expected_size {
            bail!("发送的数据超过声明的文件大小");
        }

        file.write_all(&buffer[..count])
            .await
            .context("写入接收文件失败")?;
        hasher.update(&buffer[..count]);
        job.add_bytes(count as u64);
    }

    if received != expected_size {
        bail!("文件大小校验失败（预期 {expected_size} 字节，实际 {received} 字节）");
    }

    file.flush().await.context("刷新接收文件失败")?;
    file.sync_all().await.context("同步接收文件失败")?;
    drop(file);

    let actual_digest = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if !actual_digest.eq_ignore_ascii_case(expected_digest) {
        bail!("文件 SHA-256 校验失败，文件可能已损坏");
    }

    Ok(())
}

async fn send_response(
    stream: &mut TcpStream,
    response: &TransferResponse,
    timeout_message: &str,
) -> Result<()> {
    tokio::time::timeout(RESPONSE_TIMEOUT, write_frame(stream, response))
        .await
        .with_context(|| timeout_message.to_owned())?
        .map_err(anyhow::Error::from)
}

async fn send_rejection(
    stream: &mut TcpStream,
    transfer_id: uuid::Uuid,
    message: impl Into<String>,
) {
    let response = TransferResponse::rejected(transfer_id, message);
    let _ = tokio::time::timeout(RESPONSE_TIMEOUT, write_frame(stream, &response)).await;
}

fn unique_destination(directory: &Path, file_name: &str) -> PathBuf {
    let candidate = directory.join(file_name);
    if !candidate.exists() {
        return candidate;
    }

    let path = Path::new(file_name);
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("file");
    let extension = path.extension().and_then(|value| value.to_str());

    for sequence in 1..10_000_u32 {
        let mut name = format!("{stem} ({sequence})");
        if let Some(extension) = extension {
            name.push('.');
            name.push_str(extension);
        }
        let candidate = directory.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }

    directory.join(format!("fasttran-{}", uuid::Uuid::new_v4()))
}

fn clean_device_name(raw: &str) -> String {
    let name: String = raw
        .chars()
        .filter(|character| !character.is_control())
        .take(32)
        .collect();
    let name = name.trim();
    if name.is_empty() {
        "FastTran Device".to_owned()
    } else {
        name.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::{clean_device_name, sanitize_file_name, unique_destination};

    #[test]
    fn never_uses_a_directory_as_a_destination() {
        let directory = tempfile::tempdir().unwrap();
        let destination = unique_destination(directory.path(), &sanitize_file_name("../../evil"));
        assert_eq!(destination.parent(), Some(directory.path()));
        assert_eq!(destination.file_name().unwrap(), "evil");
    }

    #[test]
    fn creates_numbered_name_for_conflicts() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("report.txt"), b"old").unwrap();
        let destination = unique_destination(directory.path(), "report.txt");
        assert_eq!(destination.file_name().unwrap(), "report (1).txt");
    }

    #[test]
    fn cleans_peer_names() {
        assert_eq!(clean_device_name("\n Alice's PC \t"), "Alice's PC");
        assert_eq!(clean_device_name("\n\t"), "FastTran Device");
    }
}
