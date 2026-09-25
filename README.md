# FastTran

FastTran 是一个使用 **Rust + egui/eframe** 开发的原生 Windows/macOS/Linux 桌面文件传输软件。它通过局域网进行设备发现和 TCP 直传，不上传云端，支持多文件、实时进度、取消、SHA-256 完整性校验和同名文件自动避让。

> 当前定位：可信局域网中的轻量级 P2P 文件分享工具。

## 已实现功能

- 原生桌面 GUI，无浏览器和 Web 服务依赖
- UDP 广播自动发现同一局域网内的设备
- 自动避让被占用的发现端口
- TCP 文件直传，传输端口冲突时显示错误提示
- 一次选择或拖放多个文件
- 实时进度、平均速度和任务状态
- 主动取消发送或接收任务
- 传输完成、取消或失败后停止计时
- 启动后默认开启设备发现与接收服务，顶部 Switch 可随时关闭/开启；关闭时停止搜索并取消正在接收的任务
- 接收端确认接收；文件校验通过后自动完成并提示
- 接收前与接收后的 SHA-256 完整性校验
- 危险文件名清理，发送方不能指定接收目录
- 同名文件自动生成 `name (1).ext`
- 深色/浅色主题、桌面端主题选择持久化和中文界面
- 响应式单栏/双栏布局，适配窄窗口与高 DPI 屏幕
- 长文件名、设备名、路径和网络信息的截断/换行
- 本机名称、接收目录持久化配置
- Windows 防火墙提示与手动 IP 连接入口

## Android 构建

Android 包使用 `android-activity` NativeActivity 和 `cargo-apk2` 构建，支持：

- Android 7.0（API 24）及以上
- `arm64-v8a`、`armeabi-v7a`、`x86_64` 三种 ABI
- OpenGL ES 原生 egui 界面
- 应用专属外部目录接收文件
- Android 系统文件选择器（支持多选文件发送到电脑）
- 通过只读 ContentProvider 打开已接收文件/目录
- 移动端顶部导航、系统安全区和单栏响应式布局

构建环境要求：

- JDK 17+
- Android SDK Platform 36
- Android Build Tools 36.1+
- Android NDK r30
- `cargo-apk2`

```powershell
rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android
cargo install cargo-apk2 --locked

$env:JAVA_HOME = "C:\path\to\jdk-17"
$env:ANDROID_SDK_ROOT = "C:\path\to\android-sdk"
$env:CARGO_APK_RELEASE_KEYSTORE = "C:\path\to\release.jks"
$env:CARGO_APK_RELEASE_KEYSTORE_PASSWORD = "your-password"
.\scripts\build-android.ps1
```

Linux/macOS 或 CI 环境可以使用：

```bash
bash scripts/build-android.sh
```

输出文件：

```text
dist/FastTran-android-universal.apk
```

> Android 文件选择器会优先直接读取系统文档；只有无法安全流式读取时才复制到应用私有缓存目录，原始文档权限不会被持久化。发布到 Google Play 时必须使用自己长期保管的发布密钥，不能使用临时测试密钥。

## GitHub CI/CD

项目已配置 GitHub Actions：

- 推送到 `main`/`master` 或创建 Pull Request 时执行格式检查、编译、测试、Clippy、Windows/Linux/macOS 桌面构建和 Android 调试 APK 构建。
- 推送形如 `v0.1.0` 的 Git tag 时，Release 工作流会构建并上传 Windows、Linux、macOS 和 Android 产物。
- Release 页面使用 GitHub 自动生成的 release notes，并附带 `SHA256SUMS` 校验文件。
- Android 正式 Release 使用长期发布密钥签名；密钥不会提交到 Git。

在仓库 **Settings → Secrets and variables → Actions** 中配置以下 Repository secrets：

| Secret | 内容 |
| --- | --- |
| `ANDROID_KEYSTORE_BASE64` | 长期 Android `.jks`/`.keystore` 文件的单行 Base64 内容 |
| `ANDROID_KEYSTORE_PASSWORD` | 该 keystore 的密码 |

生成 Base64 的 PowerShell 示例：

```powershell
[Convert]::ToBase64String([IO.File]::ReadAllBytes("fasttran-release.jks"))
```

发布新版本：

```powershell
git switch main
git pull --ff-only
# 先确认 Cargo.toml 中的 version 已更新
git tag -a v0.1.0 -m "Release v0.1.0"
git push origin v0.1.0
```

Tag 必须与 `Cargo.toml` 的版本号一致，例如 `v0.1.0`。首次发布前请先在本地确认长期 Android 密钥可用；不要使用临时测试密钥或把密钥写入仓库。


- Rust 2024
- eframe / egui（原生 GUI）
- Tokio（异步网络与文件 I/O）
- Serde / JSON（发现报文与传输握手协议，传输协议版本 2；两端需使用同版本）
- SHA-256（传输完整性校验）
- if-addrs（枚举网卡广播地址）

## 快速开始

### 环境要求

- Rust 1.95 或更高版本
- Windows 10/11、macOS 或主流桌面 Linux
- 两台电脑位于可互通的网络中

### 运行

```powershell
cd FastTran
cargo run --release
```

开发模式：

```powershell
cargo run
```

### 测试

```powershell
cargo test
```

静态检查：

```powershell
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

### Windows 防火墙

首次运行时，如果 Windows 防火墙询问网络权限，请允许 **专用网络** 访问。程序需要：

- UDP 入站：`45454`（被占用时自动尝试后续端口）
- TCP 入站：`45455`

如果设备无法互相发现，请检查：

1. 两台设备是否处于同一局域网，且没有 AP/访客网络隔离；
2. 防火墙是否允许 FastTran；
3. VPN/虚拟网卡是否导致广播走错网卡；
4. 路由器是否启用了客户端隔离。

可在“发送文件 → 接收设备 → 手动连接”中直接输入对方 IPv4 地址。

## 使用方法

1. 在两台电脑上启动 FastTran，默认会开启设备发现和接收服务。
2. 等待对方出现在“接收设备”列表中；顶部“服务”开关可控制服务状态。
3. 将一个或多个文件拖入发送面板。
4. 确认目标设备后点击发送。
5. 接收端在“传输任务”页面点击“确认接收”。
6. 文件校验通过后，接收端自动保存文件并提示完成，发送端随后显示完成。
7. 收到文件后会保存在 `下载/FastTran`。

配置目录由系统应用目录规则决定，Windows 通常位于：

```text
%APPDATA%\FastTran\config.json
```

## 项目结构

```text
FastTran/
├─ .github/workflows/    # CI、tag 发布流水线
├─ src/
│  ├─ app.rs          # 桌面/Android UI 和应用状态
│  ├─ android_bridge.rs # Android JNI 文件选择、打开和安全区桥接
│  ├─ config.rs       # 持久化配置
│  ├─ discovery.rs    # 局域网 UDP 设备发现
│  ├─ sender.rs       # 文件哈希与发送端
│  ├─ receiver.rs     # 接收服务、落盘与校验
│  ├─ protocol.rs     # 传输握手与安全文件名处理
│  ├─ model.rs        # 传输任务状态管理
│  └─ format.rs       # 字节、速度和耗时格式化
├─ android/
│  ├─ AndroidManifest.xml
│  └─ java/io/fasttran/app/  # NativeActivity、文件选择器和 ContentProvider
├─ tests/
│  └─ transfer_roundtrip.rs
└─ Cargo.toml
```

## 传输流程

1. 接收端监听 TCP 端口并等待发送端握手。
2. 发送端先计算文件 SHA-256。
3. 发送端连接接收端并发送 JSON 握手信息。
4. 接收端校验协议版本、文件名、摘要格式和传输编号，并显示“确认接收”请求。
5. 接收端确认后，发送端流式发送文件字节并更新 UI 进度。
6. 接收端写入 `.part` 临时文件，同时重新计算 SHA-256。
7. 大小与摘要均一致后，接收端自动将临时文件重命名为正式文件并提示完成。
8. 接收端返回最终确认，发送端标记任务完成。

## 当前安全边界

- TCP 能检测传输错误，但协议本身尚未加密。
- 当前没有账号、访问码或配对机制。
- 接收端不会自动接收文件：每个请求都需要本机用户确认；校验通过后会自动完成并提示。
- 不应直接将 FastTran 暴露到公网；跨不可信网络时请先使用 WireGuard/Tailscale 等可信 VPN。

后续生产化版本建议加入配对码、TLS、设备白名单、端口转发、文件夹传输和分块断点续传。

## 许可证

MIT，见 [LICENSE](LICENSE)。
