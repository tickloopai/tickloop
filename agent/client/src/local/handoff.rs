use std::{sync::Arc, time::Duration};

use tokio::net::{UnixStream, unix::OwnedReadHalf};
use uuid::Uuid;

use super::{ClientInner, negotiate_handoff};
use crate::frame::{verify_peer, write_frame};
use tickloop_agent_protocol::{
    AccessError, AccessErrorCode, CLIENT_HANDOFF_VERSION, ClientHandoffHello, ClientMessage,
    SessionResume,
};

const RESUME_DEADLINE: Duration = Duration::from_secs(30);
const RECONNECT_PAUSE: Duration = Duration::from_millis(50);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum WritePhase {
    #[default]
    Serving,
    Preparing,
    Connecting,
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct HandoffState {
    pub transfer: Option<Uuid>,
    pub phase: WritePhase,
    pub generation: u64,
}

fn require_protocol(inner: &ClientInner) -> Result<(), AccessError> {
    if inner
        .hello
        .read()
        .unwrap()
        .handoff
        .as_ref()
        .is_some_and(|hello| hello.version == CLIENT_HANDOFF_VERSION)
    {
        Ok(())
    } else {
        Err(AccessError::invalid(
            "server sent handoff frames without negotiating them",
        ))
    }
}

pub(super) async fn prepare(inner: &ClientInner, transfer: Uuid) -> Result<(), AccessError> {
    require_protocol(inner)?;
    let previous = *inner.handoff.borrow();
    if transfer.is_nil() || previous.phase != WritePhase::Serving {
        return Err(AccessError::invalid(
            "invalid or overlapping connection handoff",
        ));
    }
    inner.handoff.send_replace(HandoffState {
        transfer: Some(transfer),
        phase: WritePhase::Preparing,
        ..previous
    });
    let mut writer = inner.writer.lock().await;
    write_frame(
        &mut *writer,
        &ClientMessage::HandoffReady {
            transfer_id: transfer,
        },
        inner.limits.frame_bytes(),
    )
    .await
}

pub(super) fn abort(inner: &ClientInner, transfer: Uuid) -> Result<(), AccessError> {
    require_protocol(inner)?;
    let previous = *inner.handoff.borrow();
    if previous.transfer != Some(transfer) || previous.phase != WritePhase::Preparing {
        return Err(AccessError::invalid(
            "handoff abort does not match its preparation",
        ));
    }
    inner.handoff.send_replace(HandoffState {
        phase: WritePhase::Serving,
        ..previous
    });
    Ok(())
}

pub(super) fn confirms_not_accepted(inner: &ClientInner, transfer: Uuid) -> bool {
    require_protocol(inner).is_ok() && inner.handoff.borrow().transfer == Some(transfer)
}

pub(super) async fn commit(
    inner: &Arc<ClientInner>,
    transfer: Uuid,
    successor: u32,
) -> Result<OwnedReadHalf, AccessError> {
    require_protocol(inner)?;
    let previous = *inner.handoff.borrow();
    let old = inner.hello.read().unwrap().clone();
    if previous.transfer != Some(transfer)
        || previous.phase != WritePhase::Preparing
        || successor == 0
        || successor == old.process_id
    {
        return Err(AccessError::invalid(
            "handoff commit does not identify its successor",
        ));
    }
    if inner
        .pending
        .lock()
        .await
        .values()
        .any(|request| request.written)
    {
        return Err(AccessError::invalid(
            "server committed handoff with unanswered written requests",
        ));
    }
    inner.handoff.send_replace(HandoffState {
        phase: WritePhase::Connecting,
        ..previous
    });
    for (_, subscription) in inner.subscriptions.lock().await.drain() {
        *subscription.terminal_error.lock().unwrap() = Some(AccessError::unavailable(
            "observation is moving to the successor daemon",
        ));
        subscription.cancel.cancel();
    }
    let proof = SessionResume {
        daemon_epoch: old.daemon_epoch,
        session_id: old.session_id,
        token: old.handoff.as_ref().unwrap().resume_token.clone(),
        transfer_id: transfer,
    };
    let resume = async {
        loop {
            let connected = async {
                let mut stream = UnixStream::connect(&inner.endpoint)
                    .await
                    .map_err(|error| AccessError::unavailable(error.to_string()))?;
                verify_peer(&stream)?;
                let (hello, limits) = negotiate_handoff(
                    &mut stream,
                    Some(ClientHandoffHello {
                        version: CLIENT_HANDOFF_VERSION,
                        resume: Some(proof.clone()),
                    }),
                )
                .await?;
                if hello.process_id != successor {
                    return Err(AccessError::unavailable(
                        "waiting for the committed successor",
                    ));
                }
                if hello.daemon_epoch != old.daemon_epoch
                    || hello.session_id != old.session_id
                    || hello.handoff.is_none()
                    || limits.frame_bytes() != inner.limits.frame_bytes()
                    || limits.chunk_bytes() != inner.limits.chunk_bytes()
                {
                    return Err(AccessError::new(
                        AccessErrorCode::AccessDenied,
                        "successor did not resume the same authenticated session and transport",
                    ));
                }
                Ok((stream, hello))
            }
            .await;
            match connected {
                Ok(ready) => return Ok(ready),
                Err(error)
                    if matches!(
                        error.code,
                        AccessErrorCode::Unavailable | AccessErrorCode::HandoffNotAccepted
                    ) =>
                {
                    tokio::time::sleep(RECONNECT_PAUSE).await;
                }
                Err(error) => return Err(error),
            }
        }
    };
    let (stream, hello) = tokio::select! {
        _ = inner.reader_stop.cancelled() => return Err(AccessError::unavailable("client closed during handoff")),
        result = tokio::time::timeout(RESUME_DEADLINE, resume) => result
            .map_err(|_| AccessError::unavailable("committed daemon session could not be resumed"))??,
    };
    let (reader, writer) = stream.into_split();
    *inner.writer.lock().await = writer;
    *inner.hello.write().unwrap() = hello;
    inner.handoff.send_replace(HandoffState {
        transfer: Some(transfer),
        phase: WritePhase::Serving,
        generation: previous
            .generation
            .checked_add(1)
            .ok_or_else(|| AccessError::invalid("connection handoff generation exhausted"))?,
    });
    Ok(reader)
}
