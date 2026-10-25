use serde::Serialize;
use tickloop_code_access::daemon_management::CONTROL_FRAME_BYTES;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub async fn write<W: AsyncWrite + Unpin, T: Serialize>(
    stream: &mut W,
    message: &T,
) -> Result<(), String> {
    let bytes = serde_json::to_vec(message).map_err(|error| error.to_string())?;
    if bytes.len() > CONTROL_FRAME_BYTES {
        return Err("Agent daemon control frame exceeds the 64 KiB transport limit.".into());
    }
    stream
        .write_u32(bytes.len() as u32)
        .await
        .map_err(|error| error.to_string())?;
    stream
        .write_all(&bytes)
        .await
        .map_err(|error| error.to_string())?;
    stream.flush().await.map_err(|error| error.to_string())
}

pub async fn read<R: AsyncRead + Unpin, T: serde::de::DeserializeOwned>(
    stream: &mut R,
) -> Result<Option<T>, String> {
    let length = match stream.read_u32().await {
        Ok(length) => length as usize,
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    if length > CONTROL_FRAME_BYTES {
        return Err("Agent daemon control frame exceeds the 64 KiB transport limit.".into());
    }
    let mut bytes = vec![0; length];
    stream
        .read_exact(&mut bytes)
        .await
        .map_err(|error| error.to_string())?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| "Agent daemon returned an invalid control message.".into())
}
