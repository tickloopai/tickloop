use tickloop_agent_client::daemon_management::{read, write};
use tickloop_agent_protocol::daemon_management::{CONTROL_MAJOR, Error, Hello, Reply, Request};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

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
