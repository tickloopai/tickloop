use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::UnixStream,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub(crate) const CONTROL_MAJOR: u16 = 1;
pub(crate) const CONTROL_FRAME_BYTES: usize = 64 * 1024;

#[derive(Deserialize, Serialize)]
pub(crate) enum Request {
    Hello {
        major: u16,
        minor: u16,
    },
    ShutdownDaemon {
        request_id: Uuid,
        daemon_epoch: Uuid,
    },
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Hello {
    pub major: u16,
    pub minor: u16,
    pub daemon_epoch: Uuid,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Error {
    pub code: String,
    pub message: String,
}

#[derive(Deserialize, Serialize)]
pub(crate) enum Reply {
    Hello(Hello),
    DaemonStopped {
        request_id: Uuid,
    },
    Error {
        request_id: Option<Uuid>,
        error: Error,
    },
}

pub(crate) async fn write<W: AsyncWrite + Unpin, T: Serialize>(
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

pub(crate) async fn read<R: AsyncRead + Unpin, T: serde::de::DeserializeOwned>(
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

pub(crate) async fn serve_shutdown(
    mut stream: UnixStream,
    epoch: Uuid,
    shutdown: CancellationToken,
) -> Result<(), String> {
    write(
        &mut stream,
        &Reply::Hello(Hello {
            major: CONTROL_MAJOR,
            minor: 0,
            daemon_epoch: epoch,
        }),
    )
    .await?;
    let incoming = tokio::select! {
        _ = shutdown.cancelled() => return Ok(()),
        incoming = read::<_, serde_json::Value>(&mut stream) => incoming?,
    };
    let Some(incoming) = incoming else {
        return Ok(());
    };
    let request_id = incoming
        .as_object()
        .and_then(|object| object.values().next())
        .and_then(|body| body.get("request_id"))
        .and_then(|value| serde_json::from_value::<Uuid>(value.clone()).ok());
    let request = serde_json::from_value::<Request>(incoming);
    match request {
        Ok(Request::ShutdownDaemon {
            request_id,
            daemon_epoch,
        }) if daemon_epoch == epoch => {
            write(&mut stream, &Reply::DaemonStopped { request_id }).await?;
            shutdown.cancel();
        }
        Ok(Request::ShutdownDaemon { request_id, .. }) => {
            write(
                &mut stream,
                &Reply::Error {
                    request_id: Some(request_id),
                    error: Error {
                        code: "EpochChanged".into(),
                        message: "daemon shutdown belongs to another epoch".into(),
                    },
                },
            )
            .await?;
        }
        _ => {
            write(&mut stream, &Reply::Error { request_id, error: Error {
                code: "VersionMismatch".into(), message: "This management connection accepts only ShutdownDaemon; incompatible business requests are never dispatched.".into(),
            }}).await?;
        }
    }
    Ok(())
}
