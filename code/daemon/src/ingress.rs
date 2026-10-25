use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use tickhost_connect::hosted::same_secret;
use tickloop_code_client::frame::write_frame;

use tickloop_code_access::{
    AccessError, AccessErrorCode, AccessLimits, CLIENT_HANDOFF_VERSION, ClientHandoffHello,
    ServerHandoffHello, ServerMessage, VIEW_LEASE_SECONDS,
};

use tokio::{
    net::unix::OwnedWriteHalf,
    sync::{Mutex, Notify, mpsc, oneshot},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const QUIESCE_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) struct DaemonIngress {
    epoch: Uuid,
    limits: AccessLimits,
    state: StdMutex<IngressState>,
    changed: Arc<Notify>,
}

struct IngressState {
    phase: Phase,
    sessions: HashMap<Uuid, Session>,
}

#[derive(Clone, Copy)]
enum Phase {
    Serving,
    Preparing(Uuid),
    Committed(Uuid),
}

struct Session {
    token: Option<String>,
    transfer: Option<Uuid>,
    handshakes: usize,
    peers: HashMap<Uuid, Arc<Peer>>,
    expires: Option<Instant>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct IngressCheckpoint {
    limits: AccessLimits,
    sessions: Vec<SavedSession>,
}

#[derive(Serialize, Deserialize)]
struct SavedSession {
    session_id: Uuid,
    token: String,
    transfer_id: Uuid,
}

pub(crate) struct SessionTicket {
    ingress: Arc<DaemonIngress>,
    pub session_id: Uuid,
    pub handoff: Option<ServerHandoffHello>,
    connection_id: Uuid,
    attached: bool,
}

pub(crate) struct Peer {
    supported: bool,
    ready: StdMutex<Option<Uuid>>,
    pending: AtomicUsize,
    notices: mpsc::UnboundedSender<Notice>,
    cancel: CancellationToken,
    changed: Arc<Notify>,
}

struct Notice {
    message: ServerMessage,
    written: Option<oneshot::Sender<()>>,
}

pub(crate) struct RequestActivity(Arc<Peer>);
impl Drop for RequestActivity {
    fn drop(&mut self) {
        self.0.pending.fetch_sub(1, Ordering::SeqCst);
        self.0.changed.notify_one();
    }
}

pub(crate) struct Quiescence {
    ingress: Arc<DaemonIngress>,
    transfer_id: Uuid,
    peers: Vec<Arc<Peer>>,
    committed: bool,
}

impl DaemonIngress {
    pub(crate) fn new(epoch: Uuid, limits: AccessLimits) -> Self {
        Self {
            epoch,
            limits,
            state: StdMutex::new(IngressState {
                phase: Phase::Serving,
                sessions: HashMap::new(),
            }),
            changed: Arc::new(Notify::new()),
        }
    }

    pub(crate) fn register(
        self: &Arc<Self>,
        hello: Option<ClientHandoffHello>,
    ) -> Result<SessionTicket, AccessError> {
        let mut state = self.state.lock().unwrap();
        if !matches!(state.phase, Phase::Serving) {
            let code = if hello
                .as_ref()
                .is_some_and(|hello| hello.version == CLIENT_HANDOFF_VERSION)
            {
                AccessErrorCode::HandoffNotAccepted
            } else {
                AccessErrorCode::Unavailable
            };
            return Err(AccessError::new(
                code,
                "the daemon is transferring its connections",
            ));
        }
        state.sessions.retain(|_, session| {
            session
                .expires
                .is_none_or(|expires| expires > Instant::now())
        });
        let supported = hello
            .as_ref()
            .is_some_and(|hello| hello.version == CLIENT_HANDOFF_VERSION);
        if hello.is_some() && !supported {
            return Err(AccessError::new(
                AccessErrorCode::VersionMismatch,
                "unsupported connection handoff protocol",
            ));
        }
        let session_id = if let Some(resume) = hello.and_then(|hello| hello.resume) {
            if resume.daemon_epoch != self.epoch {
                return Err(AccessError::new(
                    AccessErrorCode::EpochChanged,
                    "session resume belongs to another daemon epoch",
                ));
            }
            let session = state
                .sessions
                .get_mut(&resume.session_id)
                .filter(|session| {
                    session.transfer == Some(resume.transfer_id)
                        && session
                            .token
                            .as_ref()
                            .is_some_and(|token| same_secret(token, &resume.token))
                })
                .ok_or_else(|| {
                    AccessError::new(
                        AccessErrorCode::AccessDenied,
                        "session handoff proof is no longer valid",
                    )
                })?;
            session.handshakes += 1;
            session.expires = None;
            resume.session_id
        } else {
            let session_id = tickloop_ids::generate();
            state.sessions.insert(
                session_id,
                Session {
                    token: supported.then(tickhost::host::random_token),
                    transfer: None,
                    handshakes: 1,
                    peers: HashMap::new(),
                    expires: None,
                },
            );
            session_id
        };
        let handoff = state.sessions[&session_id]
            .token
            .clone()
            .map(|resume_token| ServerHandoffHello {
                version: CLIENT_HANDOFF_VERSION,
                resume_token,
            });
        Ok(SessionTicket {
            ingress: self.clone(),
            session_id,
            handoff,
            connection_id: tickloop_ids::generate(),
            attached: false,
        })
    }

    pub(crate) async fn quiesce(self: &Arc<Self>, transfer_id: Uuid) -> Result<Quiescence, String> {
        let peers = {
            let mut state = self.state.lock().unwrap();
            if !matches!(state.phase, Phase::Serving) {
                return Err("another connection handoff is pending".into());
            }
            if state
                .sessions
                .values()
                .any(|session| session.handshakes != 0)
            {
                return Err("waiting for a View connection to finish opening".into());
            }
            let peers = state
                .sessions
                .values()
                .flat_map(|session| session.peers.values().cloned())
                .collect::<Vec<_>>();
            if peers.iter().any(|peer| !peer.supported) {
                return Err(
                    "a connected View does not support automatic connection handoff".into(),
                );
            }
            state.phase = Phase::Preparing(transfer_id);
            peers
        };
        let quiescence = Quiescence {
            ingress: self.clone(),
            transfer_id,
            peers,
            committed: false,
        };
        for peer in &quiescence.peers {
            peer.send(ServerMessage::HandoffPrepare { transfer_id });
        }
        let ready = tokio::time::timeout(QUIESCE_TIMEOUT, async {
            loop {
                if quiescence.peers.iter().all(|peer| {
                    peer.cancel.is_cancelled() || *peer.ready.lock().unwrap() == Some(transfer_id)
                }) {
                    return;
                }
                self.changed.notified().await;
            }
        })
        .await;
        if ready.is_err() {
            return Err("waiting for connected Views to preserve their pending input".into());
        }
        Ok(quiescence)
    }

    pub(crate) fn committed_transfer(&self) -> Option<Uuid> {
        match self.state.lock().unwrap().phase {
            Phase::Committed(id) => Some(id),
            _ => None,
        }
    }

    pub(crate) fn restore(&self, checkpoint: IngressCheckpoint) -> Result<(), String> {
        if serde_json::to_value(checkpoint.limits).map_err(|error| error.to_string())?
            != serde_json::to_value(self.limits).map_err(|error| error.to_string())?
        {
            return Err("the successor cannot preserve the negotiated connection limits".into());
        }
        let mut state = self.state.lock().unwrap();
        if !state.sessions.is_empty() {
            return Err("cannot restore sessions into a serving daemon".into());
        }
        let expires = Instant::now() + Duration::from_secs(VIEW_LEASE_SECONDS);
        for saved in checkpoint.sessions {
            if state
                .sessions
                .insert(
                    saved.session_id,
                    Session {
                        token: Some(saved.token),
                        transfer: Some(saved.transfer_id),
                        handshakes: 0,
                        peers: HashMap::new(),
                        expires: Some(expires),
                    },
                )
                .is_some()
            {
                return Err("duplicate session in daemon handoff".into());
            }
        }
        Ok(())
    }
}

impl SessionTicket {
    pub(crate) fn attach(
        &mut self,
        writer: Arc<Mutex<OwnedWriteHalf>>,
        cancel: CancellationToken,
        frame_bytes: usize,
    ) -> Arc<Peer> {
        let (notices, mut incoming) = mpsc::unbounded_channel::<Notice>();
        let peer = Arc::new(Peer {
            supported: self.handoff.is_some(),
            ready: StdMutex::new(None),
            pending: AtomicUsize::new(0),
            notices,
            cancel: cancel.clone(),
            changed: self.ingress.changed.clone(),
        });
        let publishing = peer.clone();
        tokio::spawn(async move {
            loop {
                let notice = tokio::select! { () = cancel.cancelled() => return, notice = incoming.recv() => notice };
                let Some(notice) = notice else {
                    return;
                };
                if write_frame(&mut *writer.lock().await, &notice.message, frame_bytes)
                    .await
                    .is_err()
                {
                    cancel.cancel();
                    publishing.changed.notify_one();
                    return;
                }
                if let Some(written) = notice.written {
                    let _ = written.send(());
                }
            }
        });
        let mut state = self.ingress.state.lock().unwrap();
        let session = state
            .sessions
            .get_mut(&self.session_id)
            .expect("registered connection session");
        session.handshakes -= 1;
        session.peers.insert(self.connection_id, peer.clone());
        self.attached = true;
        peer
    }

    pub(crate) fn preserves_lease(&self) -> bool {
        self.handoff.is_some()
    }
}

impl Drop for SessionTicket {
    fn drop(&mut self) {
        let mut state = self.ingress.state.lock().unwrap();
        if let Some(session) = state.sessions.get_mut(&self.session_id) {
            if self.attached {
                session.peers.remove(&self.connection_id);
            } else {
                session.handshakes = session.handshakes.saturating_sub(1);
            }
            if session.peers.is_empty() && session.handshakes == 0 {
                session.expires = Some(Instant::now() + Duration::from_secs(VIEW_LEASE_SECONDS));
            }
        }
        self.ingress.changed.notify_one();
    }
}

impl Peer {
    fn send(&self, message: ServerMessage) {
        let _ = self.notices.send(Notice {
            message,
            written: None,
        });
    }
    pub(crate) fn acknowledge(&self, transfer_id: Uuid) {
        *self.ready.lock().unwrap() = Some(transfer_id);
        self.changed.notify_one();
    }
    pub(crate) fn request(self: &Arc<Self>) -> RequestActivity {
        self.pending.fetch_add(1, Ordering::SeqCst);
        RequestActivity(self.clone())
    }
}

impl Quiescence {
    pub(crate) fn checkpoint(&self) -> IngressCheckpoint {
        let state = self.ingress.state.lock().unwrap();
        IngressCheckpoint {
            limits: self.ingress.limits,
            sessions: state
                .sessions
                .iter()
                .filter(|(_, session)| !session.peers.is_empty())
                .filter_map(|(session_id, session)| {
                    Some(SavedSession {
                        session_id: *session_id,
                        token: session.token.clone()?,
                        transfer_id: self.transfer_id,
                    })
                })
                .collect(),
        }
    }

    pub(crate) fn commit(&mut self) {
        self.ingress.state.lock().unwrap().phase = Phase::Committed(self.transfer_id);
        self.committed = true;
    }

    pub(crate) async fn finish(self, successor_process_id: u32) {
        loop {
            if self
                .peers
                .iter()
                .all(|peer| peer.pending.load(Ordering::SeqCst) == 0 || peer.cancel.is_cancelled())
            {
                break;
            }
            self.ingress.changed.notified().await;
        }
        for peer in &self.peers {
            if peer.cancel.is_cancelled() {
                continue;
            }
            let (written, received) = oneshot::channel();
            if peer
                .notices
                .send(Notice {
                    message: ServerMessage::HandoffCommit {
                        transfer_id: self.transfer_id,
                        successor_process_id,
                    },
                    written: Some(written),
                })
                .is_ok()
            {
                let _ = received.await;
            }
            peer.cancel.cancel();
        }
    }
}

impl Drop for Quiescence {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let mut state = self.ingress.state.lock().unwrap();
        if matches!(state.phase, Phase::Preparing(id) if id == self.transfer_id) {
            state.phase = Phase::Serving;
            for peer in &self.peers {
                peer.send(ServerMessage::HandoffAbort {
                    transfer_id: self.transfer_id,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn quiescing_handshakes_mark_only_opted_in_clients_and_resume_after_abort() {
        let ingress = Arc::new(DaemonIngress::new(Uuid::new_v4(), AccessLimits::DEFAULT));
        let hello = ClientHandoffHello {
            version: CLIENT_HANDOFF_VERSION,
            resume: None,
        };
        let quiescence = ingress.quiesce(Uuid::new_v4()).await.unwrap();
        assert_eq!(
            ingress.register(Some(hello.clone())).err().unwrap().code,
            AccessErrorCode::HandoffNotAccepted
        );
        assert_eq!(
            ingress.register(None).err().unwrap().code,
            AccessErrorCode::Unavailable
        );
        drop(quiescence);
        assert!(ingress.register(Some(hello)).is_ok());
        assert!(ingress.register(None).is_ok());
    }
}
