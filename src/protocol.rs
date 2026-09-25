use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use uuid::Uuid;

pub const PROTOCOL_VERSION: u16 = 2;
pub const MAX_FRAME_SIZE: usize = 64 * 1024;
const LENGTH_PREFIX_SIZE: usize = 4;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TransferRequest {
    pub version: u16,
    pub transfer_id: Uuid,
    pub file_name: String,
    pub file_size: u64,
    pub sha256: String,
    pub sender_name: String,
    pub sender_os: String,
}

impl TransferRequest {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.version != PROTOCOL_VERSION {
            return Err(ProtocolError::UnsupportedVersion(self.version));
        }
        if self.file_name.chars().count() > 255 {
            return Err(ProtocolError::InvalidFilename(
                "文件名超过 255 个字符".to_owned(),
            ));
        }
        if sanitize_file_name(&self.file_name).is_empty() {
            return Err(ProtocolError::InvalidFilename("文件名无效".to_owned()));
        }
        if self.sha256.len() != 64 || !self.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(ProtocolError::InvalidDigest);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum TransferResponseStatus {
    Pending,
    Accepted,
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TransferResponse {
    pub transfer_id: Uuid,
    pub status: TransferResponseStatus,
    pub message: Option<String>,
}

impl TransferResponse {
    pub fn pending(transfer_id: Uuid, message: impl Into<String>) -> Self {
        Self {
            transfer_id,
            status: TransferResponseStatus::Pending,
            message: Some(message.into()),
        }
    }

    pub fn accepted(transfer_id: Uuid) -> Self {
        Self {
            transfer_id,
            status: TransferResponseStatus::Accepted,
            message: None,
        }
    }

    pub fn rejected(transfer_id: Uuid, message: impl Into<String>) -> Self {
        Self {
            transfer_id,
            status: TransferResponseStatus::Rejected,
            message: Some(message.into()),
        }
    }
}

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("网络读写失败: {0}")]
    Io(#[from] std::io::Error),
    #[error("协议数据解析失败: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("协议帧过大: {0} 字节")]
    FrameTooLarge(usize),
    #[error("不支持的协议版本: {0}")]
    UnsupportedVersion(u16),
    #[error("文件名无效: {0}")]
    InvalidFilename(String),
    #[error("文件校验值无效")]
    InvalidDigest,
}

pub async fn write_frame<T, W>(writer: &mut W, value: &T) -> Result<(), ProtocolError>
where
    T: Serialize + ?Sized,
    W: AsyncWrite + Unpin,
{
    let bytes = serde_json::to_vec(value)?;
    if bytes.is_empty() {
        return Err(ProtocolError::FrameTooLarge(0));
    }
    if bytes.len() > MAX_FRAME_SIZE {
        return Err(ProtocolError::FrameTooLarge(bytes.len()));
    }

    writer
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .await?;
    writer.write_all(&bytes).await?;
    Ok(())
}

pub async fn read_frame<T, R>(reader: &mut R) -> Result<T, ProtocolError>
where
    T: DeserializeOwned,
    R: AsyncRead + Unpin,
{
    let mut length_bytes = [0_u8; LENGTH_PREFIX_SIZE];
    reader.read_exact(&mut length_bytes).await?;
    let length = u32::from_be_bytes(length_bytes) as usize;

    if length == 0 || length > MAX_FRAME_SIZE {
        return Err(ProtocolError::FrameTooLarge(length));
    }

    let mut payload = vec![0_u8; length];
    reader.read_exact(&mut payload).await?;
    Ok(serde_json::from_slice(&payload)?)
}

pub fn sanitize_file_name(raw: &str) -> String {
    let base_name = raw.rsplit(['/', '\\']).next().unwrap_or_default();
    let mut sanitized: String = base_name
        .chars()
        .map(|character| {
            if character.is_control()
                || matches!(
                    character,
                    '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
                )
            {
                '_'
            } else {
                character
            }
        })
        .collect();

    while sanitized.ends_with([' ', '.']) {
        sanitized.pop();
    }

    if sanitized.is_empty() || sanitized == "." || sanitized == ".." {
        "未命名文件".to_owned()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::duplex;

    use super::{
        PROTOCOL_VERSION, ProtocolError, TransferRequest, TransferResponse, read_frame,
        sanitize_file_name, write_frame,
    };

    fn request() -> TransferRequest {
        TransferRequest {
            version: PROTOCOL_VERSION,
            transfer_id: uuid::Uuid::new_v4(),
            file_name: "report.txt".into(),
            file_size: 42,
            sha256: "a".repeat(64),
            sender_name: "Test PC".into(),
            sender_os: "windows".into(),
        }
    }

    #[test]
    fn sanitizes_paths_and_windows_characters() {
        assert_eq!(sanitize_file_name("../../secret.txt"), "secret.txt");
        assert_eq!(sanitize_file_name(r"C:\temp\a:b?.txt"), "a_b_.txt");
        assert_eq!(sanitize_file_name("..."), "未命名文件");
    }

    #[test]
    fn validates_request() {
        let mut request = request();
        request.validate().unwrap();
        request.version = 99;
        assert!(matches!(
            request.validate(),
            Err(ProtocolError::UnsupportedVersion(99))
        ));
    }

    #[tokio::test]
    async fn frame_round_trip() {
        let (mut client, mut server) = duplex(4096);
        let value = TransferResponse::accepted(uuid::Uuid::new_v4());
        write_frame(&mut client, &value).await.unwrap();
        let decoded: TransferResponse = read_frame(&mut server).await.unwrap();
        assert_eq!(value, decoded);
    }
}
