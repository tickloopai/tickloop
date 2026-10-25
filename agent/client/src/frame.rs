use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::UnixStream,
};

use tickloop_agent_access::{
    AccessError, AccessErrorCode, check_frame_length, decode_frame, encode_frame,
};

pub async fn write_frame<W: AsyncWrite + Unpin, T: serde::Serialize>(
    writer: &mut W,
    value: &T,
    frame_bytes: usize,
) -> Result<(), AccessError> {
    let bytes = encode_frame(value, frame_bytes)?;
    writer
        .write_u32(bytes.len() as u32)
        .await
        .map_err(|error| AccessError::unavailable(error.to_string()))?;
    writer
        .write_all(&bytes)
        .await
        .map_err(|error| AccessError::unavailable(error.to_string()))?;
    writer
        .flush()
        .await
        .map_err(|error| AccessError::unavailable(error.to_string()))
}

pub async fn read_frame<R: AsyncRead + Unpin, T: serde::de::DeserializeOwned>(
    reader: &mut R,
    frame_bytes: usize,
) -> Result<Option<T>, AccessError> {
    match read_frame_body(reader, frame_bytes).await? {
        Some(bytes) => decode_frame(&bytes).map(Some),
        None => Ok(None),
    }
}

/// One frame's undecoded body, for a reader that answers a body it cannot decode instead of
/// dropping the connection.
pub async fn read_frame_body<R: AsyncRead + Unpin>(
    reader: &mut R,
    frame_bytes: usize,
) -> Result<Option<Vec<u8>>, AccessError> {
    let length = match reader.read_u32().await {
        Ok(length) => length as usize,
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(AccessError::unavailable(error.to_string())),
    };
    check_frame_length(length, frame_bytes)?;
    let mut bytes = vec![0; length];
    reader
        .read_exact(&mut bytes)
        .await
        .map_err(|error| AccessError::unavailable(error.to_string()))?;
    Ok(Some(bytes))
}

pub fn verify_peer(stream: &UnixStream) -> Result<(), AccessError> {
    // A peer that disconnected before its credentials could be read is gone, not foreign: the
    // caller treats the connection as unavailable, as a lost worker.
    let credentials = stream
        .peer_cred()
        .map_err(|error| AccessError::unavailable(error.to_string()))?;
    if credentials.uid() != unsafe { libc::geteuid() } {
        return Err(AccessError::new(
            AccessErrorCode::AccessDenied,
            "AgentAccess permits only the current user",
        ));
    }
    Ok(())
}
