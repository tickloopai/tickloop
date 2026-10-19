use tickloop_agent_client::daemon_management::{read, write};
use tickloop_agent_protocol::daemon_management::{CONTROL_MAJOR, Error, Hello, Reply, Request};
use tokio::net::UnixStream;
use uuid::Uuid;

pub(crate) async fn serve_shutdown(
    mut stream: UnixStream,
    epoch: Uuid,
    lifecycle: crate::lifecycle::DaemonLifecycle,
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
        _ = lifecycle.shutdown().cancelled() => return Ok(()),
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
            let stop = match lifecycle.reserve_stop() {
                Ok(stop) => stop,
                Err(error) => {
                    write(
                        &mut stream,
                        &Reply::Error {
                            request_id: Some(request_id),
                            error: Error {
                                code: "Unavailable".into(),
                                message: error.message,
                            },
                        },
                    )
                    .await?;
                    return Ok(());
                }
            };
            write(&mut stream, &Reply::DaemonStopped { request_id }).await?;
            drop(stop);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::DaemonLifecycle;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn management_stop_acknowledges_only_the_daemon_that_can_still_stop() {
        for transferred in [false, true] {
            let epoch = Uuid::new_v4();
            let lifecycle = DaemonLifecycle::new(CancellationToken::new());
            let (server, mut client) = UnixStream::pair().unwrap();
            let serving = tokio::spawn(serve_shutdown(server, epoch, lifecycle.clone()));
            assert!(matches!(
                read::<_, Reply>(&mut client).await.unwrap(),
                Some(Reply::Hello(_))
            ));
            if transferred {
                lifecycle.begin_handoff().unwrap();
                lifecycle.begin_activation().unwrap();
            }
            let request_id = Uuid::new_v4();
            write(
                &mut client,
                &Request::ShutdownDaemon {
                    request_id,
                    daemon_epoch: epoch,
                },
            )
            .await
            .unwrap();
            let reply = read::<_, Reply>(&mut client).await.unwrap().unwrap();
            if transferred {
                assert!(
                    matches!(reply, Reply::Error { request_id: Some(id), error } if id == request_id && error.code == "Unavailable")
                );
                assert!(!lifecycle.shutdown().is_cancelled());
            } else {
                assert!(
                    matches!(reply, Reply::DaemonStopped { request_id: id } if id == request_id)
                );
                assert!(lifecycle.begin_handoff().is_err());
            }
            serving.await.unwrap().unwrap();
            assert_eq!(lifecycle.shutdown().is_cancelled(), !transferred);
        }
    }
}
