use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::runtime::Handle;
use tokio::sync::Semaphore;

use crate::discovery::Peer;
use crate::model::{TransferHub, TransferJob, TransferState};
use crate::protocol::{
    PROTOCOL_VERSION, TransferRequest, TransferResponse, TransferResponseStatus, read_frame,
    write_frame,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const FINAL_RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);
const USER_CONFIRMATION_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const COPY_BUFFER_SIZE: usize = 2 * 1024 * 1024;
const WRITE_CHUNK_TIMEOUT: Duration = Duration::from_secs(60);
const DEFAULT_MAX_CONCURRENT_TRANSFERS: usize = 2;

#[derive(Clone)]
pub struct TransferService {
    runtime: Handle,
    hub: Arc<TransferHub>,
    transfer_slots: Arc<Semaphore>,
}

impl TransferService {
    pub fn new(runtime: Handle, hub: Arc<TransferHub>) -> Self {
        Self::with_max_concurrency(runtime, hub, DEFAULT_MAX_CONCURRENT_TRANSFERS)
    }

    pub fn with_max_concurrency(
        runtime: Handle,
        hub: Arc<TransferHub>,
        max_concurrent_transfers: usize,
    ) -> Self {
        Self {
            runtime,
            hub,
            transfer_slots: Arc::new(Semaphore::new(max_concurrent_transfers.clamp(1, 8))),
        }
    }

    pub fn enqueue(&self, peer: &Peer, file: &Path, local_name: String) -> Result<uuid::Uuid> {
        let is_android_fd = cfg!(target_os = "android")
            && (file.to_string_lossy().starts_with("/proc/self/fd/")
                || std::fs::symlink_metadata(file)
                    .map(|metadata| metadata.file_type().is_symlink())
                    .unwrap_or(false));
        let (is_file, initial_size) = match std::fs::metadata(file) {
            Ok(metadata) => (metadata.is_file() || is_android_fd, metadata.len()),
            Err(_error) if is_android_fd => (true, 0),
            Err(error) => {
                return Err(error).with_context(|| format!("无法读取文件信息: {}", file.display()));
            }
        };
        if !is_file {
            bail!("暂仅支持发送单个文件；文件夹请先压缩为压缩包");
        }

        let job = Arc::new(TransferJob::sending(
            file,
            peer.name.clone(),
            peer.display_address(),
            initial_size,
        ));
        let id = job.id;
        self.hub.register(job.clone());

        let peer = peer.clone();
        let source = file.to_path_buf();
        let _cleanup_source = source.clone();
        let transfer_slots = self.transfer_slots.clone();
        self.runtime.spawn(async move {
            let _permit = match transfer_slots.acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => {
                    job.fail("发送队列已关闭");
                    return;
                }
            };
            if let Err(error) = send_file(peer, source, job.clone(), local_name).await {
                tracing::info!(transfer_id = %job.id, %error, "send failed");
                #[cfg(target_os = "android")]
                crate::android_bridge::remove_picked_file(&_cleanup_source);
                job.fail(error.to_string());
            }
        });

        Ok(id)
    }
}

async fn send_file(
    peer: Peer,
    source: std::path::PathBuf,
    job: Arc<TransferJob>,
    local_name: String,
) -> Result<()> {
    job.set_state(TransferState::Hashing);
    let (file_size, digest) = hash_file(&source, &job).await?;
    job.set_total_bytes(file_size);

    job.set_state(TransferState::WaitingForReceiver);
    let mut stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(peer.socket_addr()))
        .await
        .context("连接对方超时")?
        .with_context(|| format!("无法连接 {}", peer.display_address()))?;
    let _ = stream.set_nodelay(true);

    let request = TransferRequest {
        version: PROTOCOL_VERSION,
        transfer_id: job.id,
        file_name: job.file_name.clone(),
        file_size,
        sha256: digest,
        sender_name: local_name,
        sender_os: std::env::consts::OS.to_owned(),
    };
    request.validate()?;

    tokio::time::timeout(HANDSHAKE_TIMEOUT, write_frame(&mut stream, &request))
        .await
        .context("发送文件信息超时")??;
    wait_for_acceptance(
        &mut stream,
        &job,
        TransferState::WaitingForReceiver,
        HANDSHAKE_TIMEOUT,
        "等待接收确认超时",
        "对方拒绝接收",
    )
    .await?;

    job.set_state(TransferState::Sending);
    let mut file = File::open(&source)
        .await
        .with_context(|| format!("无法打开文件: {}", source.display()))?;
    let mut buffer = vec![0_u8; COPY_BUFFER_SIZE];
    let mut sent = 0_u64;

    loop {
        ensure_not_cancelled(&job)?;
        let count = file
            .read(&mut buffer)
            .await
            .with_context(|| format!("读取文件失败: {}", source.display()))?;
        if count == 0 {
            break;
        }

        tokio::time::timeout(WRITE_CHUNK_TIMEOUT, stream.write_all(&buffer[..count]))
            .await
            .context("发送文件数据超时")??;
        sent += count as u64;
        job.add_bytes(count as u64);
    }

    if sent != file_size {
        bail!("文件在传输过程中发生变化（预期 {file_size} 字节，实际 {sent} 字节）");
    }
    stream.shutdown().await.context("关闭发送连接失败")?;

    wait_for_acceptance(
        &mut stream,
        &job,
        TransferState::Sending,
        FINAL_RESPONSE_TIMEOUT,
        "等待传输完成确认超时",
        "接收端未返回完成确认",
    )
    .await?;

    job.finish();
    #[cfg(target_os = "android")]
    crate::android_bridge::remove_picked_file(&source);
    Ok(())
}

async fn wait_for_acceptance(
    stream: &mut TcpStream,
    job: &TransferJob,
    pending_state: TransferState,
    initial_timeout: Duration,
    timeout_message: &str,
    rejection_message: &str,
) -> Result<()> {
    let mut response_timeout = initial_timeout;
    for _ in 0..3 {
        ensure_not_cancelled(job)?;
        let response: TransferResponse = tokio::select! {
            _ = job.wait_for_cancel() => bail!("传输已取消"),
            result = tokio::time::timeout(response_timeout, read_frame(stream)) => result
                .with_context(|| timeout_message.to_owned())??,
        };

        if response.transfer_id != job.id {
            bail!("对方返回了不匹配的传输编号");
        }
        match response.status {
            TransferResponseStatus::Accepted => return Ok(()),
            TransferResponseStatus::Pending => {
                job.set_state(pending_state);
                response_timeout = USER_CONFIRMATION_TIMEOUT;
            }
            TransferResponseStatus::Rejected => {
                bail!(
                    "{rejection_message}: {}",
                    response.message.unwrap_or_else(|| "未知原因".to_owned())
                );
            }
        }
    }

    bail!("对方没有完成确认")
}

async fn hash_file(path: &Path, job: &TransferJob) -> Result<(u64, String)> {
    let mut file = File::open(path)
        .await
        .with_context(|| format!("无法打开文件: {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; COPY_BUFFER_SIZE];
    let mut total = 0_u64;

    loop {
        ensure_not_cancelled(job)?;
        let count = file
            .read(&mut buffer)
            .await
            .with_context(|| format!("读取文件失败: {}", path.display()))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        total += count as u64;
    }

    let digest = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok((total, digest))
}

fn ensure_not_cancelled(job: &TransferJob) -> Result<()> {
    if job.is_cancel_requested() {
        bail!("传输已取消");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::model::TransferJob;

    use super::hash_file;

    #[tokio::test]
    async fn hashes_file_contents() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("input.bin");
        std::fs::write(&file, b"FastTran").unwrap();
        let job = TransferJob::sending(&file, "peer".into(), "peer".into(), 8);

        let (size, digest) = hash_file(Path::new(&file), &job).await.unwrap();
        assert_eq!(size, 8);
        assert_eq!(
            digest,
            "6ae5dc1fd4ae8e2f4c6eee491a818c9c8f993824c045a615bd8a9f24493627c2"
        );
    }
}
