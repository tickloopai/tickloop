use serde_json::{Value, json};
use tickhost_connect::hosted::{
    self, BUSINESS_FRAME_BYTES, CONTROL_TIMEOUT, HostConnection, OBSERVATION_FRAME_BYTES,
};
use tokio::{
    net::unix::OwnedWriteHalf,
    sync::{mpsc, oneshot, watch},
};

use super::*;

const TERMINAL_QUEUE: usize = 16;

pub(super) struct LocalObservation {
    pub instance: tick_protocol::InstanceRef,
    pub samples: mpsc::Sender<Sample>,
}

pub(super) struct Sample {
    pub reply: oneshot::Sender<Result<ExecutionRead, ExecutionError>>,
}

pub(super) async fn observe(
    port: &LocalPort,
    request: ObserveRequest,
) -> Result<ExecutionObservation, ExecutionError> {
    port.authority.check(port.scope.chat)?;
    if request
        .after
        .as_ref()
        .is_some_and(|cursor| cursor.instance != request.instance)
    {
        return Err(ExecutionError::new(
            "Unauthorized",
            "cursor belongs to another Tick",
        ));
    }
    let tools = port
        .tools
        .as_ref()
        .ok_or_else(|| ExecutionError::new("Unsupported", "this environment has no Tickhost"))?;
    let adapter = tools
        .adapter(port.scope.chat, &request.instance)
        .await
        .map_err(access_error)?;
    let mut connection = tools
        .data(port.scope.chat, &request.instance)
        .await
        .map_err(access_error)?;
    let epoch = connection.epoch();
    if request
        .after
        .as_ref()
        .is_some_and(|cursor| cursor.epoch != epoch)
    {
        return Err(ExecutionError::new(
            "HostRestarted",
            "the Tickhost restarted",
        ));
    }
    let registered = connection
        .request("watch", json!({}), CONTROL_TIMEOUT)
        .await
        .map_err(access_error)?;
    port.authority.check(port.scope.chat)?;
    let (baseline, received) = match adapter {
        tickhost::host::TickAdapter::Protocol => {
            let state =
                serde_json::from_value(registered["read"].clone()).map_err(protocol_error)?;
            let received = registered["received"].as_u64().ok_or_else(|| {
                ExecutionError::new("ProtocolViolation", "missing observation count")
            })?;
            (
                ExecutionRead {
                    kind: ExecutionKind::Protocol,
                    state,
                    truncated: false,
                },
                received,
            )
        }
        tickhost::host::TickAdapter::Terminal => {
            let read: TerminalRead =
                serde_json::from_value(registered["read"].clone()).map_err(protocol_error)?;
            let received = read.seq;
            (read.into_read(), received)
        }
    };
    let cursor = ObservationCursor {
        instance: request.instance,
        epoch,
        watch: tickloop_ids::generate(),
        position: received,
    };
    let (sender, updates) = watch::channel(ExecutionUpdate {
        update: None,
        received,
        state: ExecutionLinkState::Live,
        terminal: None,
    });
    let authority = port.authority.clone();
    let chat = port.scope.chat;
    match adapter {
        tickhost::host::TickAdapter::Protocol => {
            tokio::spawn(async move {
                loop {
                    let event = tokio::select! {
                        _ = sender.closed() => return,
                        _ = authority.revoked() => return,
                        event = connection.next_event_within(OBSERVATION_FRAME_BYTES) => event,
                    };
                    if authority.check(chat).is_err() {
                        return;
                    }
                    match event {
                        Ok(Some(event)) if event["event"] == "observation" => {
                            match serde_json::from_value(event["observation"].clone()) {
                                Ok(update) => {
                                    sender.send_replace(update);
                                }
                                Err(error) => {
                                    sender.send_modify(|current| {
                                        current.state =
                                            ExecutionLinkState::ProtocolError(error.to_string())
                                    });
                                    return;
                                }
                            }
                        }
                        Ok(Some(event)) if event["event"] == "gone" => {
                            sender.send_modify(|current| current.state = ExecutionLinkState::Gone);
                            return;
                        }
                        _ => {
                            sender.send_modify(|current| {
                                current.state = ExecutionLinkState::Disconnected
                            });
                            return;
                        }
                    }
                }
            });
        }
        tickhost::host::TickAdapter::Terminal => {
            let (samples, requests) = mpsc::channel(TERMINAL_QUEUE);
            let mut observations = port.samples.lock().await;
            observations.retain(|_, observation| !observation.samples.is_closed());
            observations.insert(
                cursor.watch,
                LocalObservation {
                    instance: cursor.instance.clone(),
                    samples,
                },
            );
            tokio::spawn(follow_terminal(
                connection, sender, requests, authority, chat,
            ));
        }
    }
    Ok(ExecutionObservation {
        baseline,
        cursor,
        gap: request.after.is_some(),
        updates,
    })
}

fn access_error(error: tickhost_connect::host_error::HostError) -> ExecutionError {
    ExecutionError::new(error.code, error.message)
}

fn protocol_error(error: impl std::fmt::Display) -> ExecutionError {
    ExecutionError::new("ProtocolViolation", error.to_string())
}

struct TerminalConnection {
    frames: mpsc::Receiver<Value>,
    writer: OwnedWriteHalf,
    next_id: u64,
}

impl TerminalConnection {
    fn new(connection: HostConnection) -> Self {
        let (mut reader, writer) = connection.into_parts();
        let (sender, frames) = mpsc::channel(TERMINAL_QUEUE);
        tokio::spawn(async move {
            loop {
                let frame = tokio::select! {
                    _ = sender.closed() => return,
                    frame = hosted::read_frame(&mut reader, BUSINESS_FRAME_BYTES) => frame,
                };
                let Ok(Some(frame)) = frame else {
                    return;
                };
                if sender.send(frame).await.is_err() {
                    return;
                }
            }
        });
        Self {
            frames,
            writer,
            next_id: 1,
        }
    }

    async fn read(&mut self) -> Result<TerminalRead, ExecutionError> {
        let id = self.next_id;
        self.next_id += 1;
        tokio::time::timeout(
            CONTROL_TIMEOUT,
            hosted::write_frame(
                &mut self.writer,
                &json!({"id":id,"method":"read","params":{}}),
                BUSINESS_FRAME_BYTES,
            ),
        )
        .await
        .map_err(|_| ExecutionError::new("Disconnected", "terminal sample timed out"))?
        .map_err(access_error)?;
        loop {
            let frame = tokio::time::timeout(CONTROL_TIMEOUT, self.frames.recv())
                .await
                .ok()
                .flatten()
                .ok_or_else(|| {
                    ExecutionError::new("Disconnected", "terminal sample did not arrive")
                })?;
            if frame.get("id").is_none() {
                match frame["event"].as_str() {
                    Some("changed" | "exited") => continue,
                    Some("gone") => {
                        return Err(ExecutionError::new("TargetGone", "terminal Tick ended"));
                    }
                    _ => return Err(protocol_error("unknown terminal event")),
                }
            }
            let read = hosted::response_result(frame, Some(id)).map_err(access_error)?;
            return serde_json::from_value(read).map_err(protocol_error);
        }
    }
}

async fn follow_terminal(
    connection: HostConnection,
    sender: watch::Sender<ExecutionUpdate>,
    mut samples: mpsc::Receiver<Sample>,
    authority: ExecutionAuthority,
    chat: tickloop_agent_protocol::ChatId,
) {
    let mut connection = TerminalConnection::new(connection);
    loop {
        tokio::select! {
            _ = sender.closed() => return,
            _ = authority.revoked() => return,
            Some(sample) = samples.recv() => {
                let read = connection.read().await;
                if authority.check(chat).is_err() { return; }
                match read {
                    Ok(read) => {
                        sender.send_modify(|current| current.terminal = Some(TerminalChange { dirty: false, exited: read.exit.is_some() }));
                        let _ = sample.reply.send(Ok(read.into_read()));
                    }
                    Err(error) => {
                        sender.send_modify(|current| current.state = if error.code == "TargetGone" { ExecutionLinkState::Gone } else { ExecutionLinkState::Disconnected });
                        let _ = sample.reply.send(Err(error));
                        return;
                    }
                }
            }
            frame = connection.frames.recv() => {
                if authority.check(chat).is_err() { return; }
                match frame.as_ref().and_then(|frame| frame["event"].as_str()) {
                    Some("changed" | "exited") => {
                        let exited = frame.as_ref().is_some_and(|frame| frame["event"] == "exited");
                        sender.send_modify(|current| {
                            current.received += 1;
                            let change = current.terminal.get_or_insert(TerminalChange { dirty: false, exited: false });
                            change.dirty = true;
                            change.exited |= exited;
                        });
                    }
                    Some("gone") => { sender.send_modify(|current| current.state = ExecutionLinkState::Gone); return; }
                    _ => { sender.send_modify(|current| current.state = ExecutionLinkState::Disconnected); return; }
                }
            }
        }
    }
}
