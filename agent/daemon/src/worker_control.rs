//! The daemon side of the worker-to-daemon channel for hosted Tick lifecycles and Shell spawns:
//! one private endpoint, one grant per running worker. It verifies that a request's Chat runs in
//! the asking worker, derives the config root from the worker's launch itself, and dispatches
//! through the daemon's own host controller.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::Duration,
};

use serde_json::{Value, json};
use tickhost::{
    host::{OpenRequest, Receipt, TickOwner},
    shell::{ShellIdentity, SpawnBinding},
};
use tickhost_connect::{
    host_error::HostError,
    hosted::{
        BUSINESS_FRAME_BYTES, CONTROL_TIMEOUT, peer_uid, read_frame, same_secret, write_frame,
    },
};
use tokio::{
    io::BufReader,
    net::{UnixListener, UnixStream},
    sync::Mutex,
};
use uuid::Uuid;

use crate::DaemonServer;
use tickloop_agent_protocol::{
    AgentAccess, AgentDetailQuery, ExecutionRef, HostEndpointIdentity, HostToolAuthority,
    LiveChatSnapshot, LoopState, ObservationRequest, ObservationValue,
};
use tickloop_agent_runtime::{
    hosted::{
        controller::HostController,
        registry::{
            LaunchIdentity, LaunchRecord, LaunchState, OwnerState, Registry, RootRegistry,
            TARGET_GONE, Transition,
        },
        worker_control::{
            ENDPOINT_NAME, HostedAdmission, HostedTickPage, IssuedSpawn, LostTick,
            WorkerControlGrant, WorkerEnvelope, WorkerRequest, root_identity,
        },
    },
    tick_tools::MODIFY_TICK,
};

/// A worker request is small: an open carries argv, cwd and the launch environment.
const REQUEST_FRAME_BYTES: usize = tick_protocol::MAX_REQUEST_BYTES;
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);
/// Watch intents are renewed this many times within the idle timeout that lapses them.
const INTENT_RENEWALS_PER_LEASE: u32 = 3;

struct Grant {
    capability: String,
    worker: Arc<dyn AgentAccess>,
    root: Option<String>,
    config_root: Option<PathBuf>,
}

/// The daemon side: one private endpoint, one grant per running worker.
pub struct WorkerControl {
    endpoint: PathBuf,
    epoch: Uuid,
    grants: Mutex<HashMap<Uuid, Grant>>,
    controller: Arc<HostController>,
    registry: Registry,
    /// Per config root: reconciled with the host once since this daemon started.
    reconciled: std::sync::Mutex<HashMap<String, Arc<tokio::sync::OnceCell<()>>>>,
    daemon: OnceLock<DaemonServer>,
}

impl WorkerControl {
    pub fn new(directory: &Path, epoch: Uuid, controller: Arc<HostController>) -> Self {
        Self {
            endpoint: directory.join(ENDPOINT_NAME),
            epoch,
            grants: Mutex::new(HashMap::new()),
            controller,
            registry: Registry::default(),
            reconciled: std::sync::Mutex::new(HashMap::new()),
            daemon: OnceLock::new(),
        }
    }

    pub fn endpoint(&self) -> &Path {
        &self.endpoint
    }

    /// Completes the daemon wiring once the daemon server exists.
    pub fn attach_daemon(&self, daemon: DaemonServer) {
        let _ = self.daemon.set(daemon);
    }

    /// A fresh grant for a worker about to start.
    pub fn grant(&self) -> WorkerControlGrant {
        WorkerControlGrant {
            endpoint: self.endpoint.clone(),
            daemon_epoch: self.epoch,
            binding: Uuid::new_v4(),
            capability: tickhost::host::random_token(),
            host_endpoint: self.controller.endpoint(),
            intent_renewal: self.controller.idle_timeout() / INTENT_RENEWALS_PER_LEASE,
        }
    }

    /// Records the grant once its worker is running, with the config root it was launched for,
    /// and reconciles that root with the host if this daemon has not yet.
    pub async fn register(
        self: &Arc<Self>,
        grant: &WorkerControlGrant,
        worker: Arc<dyn AgentAccess>,
        config_root: Option<&Path>,
    ) {
        let root = config_root.and_then(root_identity);
        self.grants.lock().await.insert(
            grant.binding,
            Grant {
                capability: grant.capability.clone(),
                worker,
                root: root.clone(),
                config_root: config_root.map(Path::to_path_buf),
            },
        );
        if let (Some(config_root), Some(root)) = (config_root.map(Path::to_path_buf), root) {
            let control = self.clone();
            // A failed reconcile is tried again by the root's next request.
            tokio::spawn(async move {
                let _ = control.root_registry(&config_root, &root).await;
            });
        }
    }

    pub async fn revoke(&self, worker: &Arc<dyn AgentAccess>) {
        self.grants
            .lock()
            .await
            .retain(|_, grant| !Arc::ptr_eq(&grant.worker, worker));
    }

    pub async fn serve(self: Arc<Self>, listener: UnixListener) {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let control = self.clone();
                    tokio::spawn(async move {
                        let _ = control.connection(stream).await;
                    });
                }
                Err(_) => tokio::time::sleep(ACCEPT_RETRY_DELAY).await,
            }
        }
    }

    /// Serves exactly one bounded request per connection, read within the control timeout.
    async fn connection(&self, stream: UnixStream) -> Result<(), HostError> {
        if peer_uid(&stream)? != unsafe { libc::geteuid() } {
            return Err(HostError::before("Forbidden", "peer is another user"));
        }
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let frame = tokio::time::timeout(
            CONTROL_TIMEOUT,
            read_frame(&mut reader, REQUEST_FRAME_BYTES),
        )
        .await
        .map_err(|_| HostError::before("Timeout", "no worker request arrived"))??
        .ok_or_else(|| HostError::before("Disconnected", "no worker request"))?;
        let id = frame["id"].clone();
        let reply = match serde_json::from_value::<WorkerEnvelope>(frame["envelope"].clone()) {
            Ok(envelope) => self.handle(envelope).await,
            Err(error) => Err(HostError::before("InvalidArguments", error.to_string())),
        };
        let frame = match reply {
            Ok(result) => json!({"id":id,"result":result}),
            Err(error) => json!({"id":id,"error":error}),
        };
        write_frame(&mut writer, &frame, BUSINESS_FRAME_BYTES).await
    }

    async fn handle(&self, envelope: WorkerEnvelope) -> Result<Value, HostError> {
        if envelope.daemon_epoch != self.epoch {
            return Err(HostError::before(
                "EpochChanged",
                "this worker belongs to an earlier Agent daemon",
            ));
        }
        let (worker, root, config_root) = {
            let grants = self.grants.lock().await;
            let grant = grants
                .get(&envelope.binding)
                .filter(|grant| same_secret(&grant.capability, &envelope.capability))
                .ok_or_else(|| HostError::before("Forbidden", "unknown worker grant"))?;
            (
                grant.worker.clone(),
                grant.root.clone(),
                grant.config_root.clone(),
            )
        };
        let daemon = self
            .daemon
            .get()
            .ok_or_else(|| HostError::before("HostUnavailable", "the daemon is starting"))?;
        let chat = envelope.request.chat();
        // Every request acts for a Chat that runs in the asking worker.
        let execution = daemon
            .verify_worker_chat(chat, &worker)
            .await
            .map_err(|error| HostError::before("Forbidden", error.message))?;
        let shell_identity = ShellIdentity {
            daemon_epoch: self.epoch,
            worker: envelope.binding,
            chat,
        };
        match envelope.request {
            WorkerRequest::AllowShellSpawn {
                operation,
                approval,
                ..
            } => {
                // The worker approved and checked the command; the host only learns who may
                // start which one, once.
                let binding = SpawnBinding {
                    operation,
                    identity: shell_identity,
                    approval,
                };
                let grant = self.controller.allow_spawn(&binding).await?;
                return Ok(json!(IssuedSpawn {
                    grant,
                    binding,
                    endpoint: self.controller.endpoint(),
                }));
            }
            // The host admits a view only of a task this worker submitted for this Chat.
            WorkerRequest::AllowShellView { instance, .. } => {
                return self
                    .controller
                    .allow_shell_view(&shell_identity, &instance)
                    .await
                    .map(|grant| json!(grant));
            }
            _ => {}
        }
        let snapshot = match envelope.request.admission() {
            Some(admission) => Some(self.admit(&worker, execution, admission).await?),
            None => None,
        };
        let (Some(root), Some(config_root)) = (root, config_root) else {
            return Err(HostError::before(
                "Unsupported",
                "hosted Ticks need a config root for durable ownership",
            ));
        };
        let registry = self.root_registry(&config_root, &root).await?;
        let tick_owner = TickOwner { root, chat };
        match envelope.request {
            WorkerRequest::OpenHostedTick {
                operation,
                adapter,
                command,
                cwd,
                environment,
                title,
                ..
            } => {
                let _lifecycle = registry.lifecycle(chat).await;
                let owner_revision = opening_revision(
                    &self.controller,
                    &registry,
                    &tick_owner,
                    snapshot.as_deref(),
                )
                .await?;
                let record = LaunchRecord::opening(operation, chat, adapter, command, cwd, title)?;
                self.launch(&registry, tick_owner, owner_revision, record, environment)
                    .await
                    .map(|opened| json!(opened))
            }
            WorkerRequest::ReopenHostedTick {
                operation,
                instance,
                environment,
                ..
            } => {
                let _lifecycle = registry.lifecycle(chat).await;
                // An instance whose host died since the last listing is lost only once settled.
                let listing = self.controller.owner_listing(&tick_owner).await?;
                registry
                    .settle_launches(&self.controller, &tick_owner, &listing)
                    .await?;
                let mut lost = registry
                    .launches(chat)
                    .await?
                    .into_iter()
                    .find(|record| record.instance() == Some(&instance))
                    .ok_or_else(|| {
                        HostError::before(TARGET_GONE, "this Chat recorded no such hosted Tick")
                    })?;
                if let LaunchState::Reopened { by, host_epoch, .. } = lost.state {
                    // A reopen whose answer was lost settles from its own receipt, so the
                    // instance is never started twice; one that certainly never started frees
                    // the lost Tick for another reopen.
                    match self.controller.receipt(by, &tick_owner).await? {
                        Some(Receipt::Opened(opened)) => return Ok(json!(opened)),
                        Some(Receipt::Failed(failure)) if !failure.dispatched => {
                            let restored = LaunchState::Lost {
                                instance: instance.clone(),
                                host_epoch,
                            };
                            registry
                                .transition_launch(chat, lost.operation, move |mut current| {
                                    if matches!(current.state, LaunchState::Reopened { by: earlier, .. } if earlier == by)
                                    {
                                        current.state = restored;
                                        Transition::Write(Box::new(current))
                                    } else {
                                        Transition::Keep
                                    }
                                })
                                .await?;
                            registry.remove_launch(chat, by).await?;
                            // The claim below compares against the record as it is now.
                            lost = registry
                                .launches(chat)
                                .await?
                                .into_iter()
                                .find(|record| record.operation == lost.operation)
                                .ok_or_else(|| {
                                    HostError::before(
                                        TARGET_GONE,
                                        "this Chat recorded no such hosted Tick",
                                    )
                                })?;
                        }
                        _ => {
                            return Err(HostError::after(
                                "OutcomeUnknown",
                                format!("{instance} was already reopened by operation {by}"),
                            ));
                        }
                    }
                }
                let LaunchState::Lost { host_epoch, .. } = lost.state else {
                    return Err(HostError::before(
                        "InvalidState",
                        "only a hosted Tick whose host was lost can be reopened",
                    ));
                };
                let current = LaunchIdentity::of(&lost.command, &lost.cwd)
                    .map_err(|error| HostError::before("LaunchFailed", error))?;
                if current != lost.identity {
                    return Err(HostError::before(
                        "LaunchChanged",
                        "its program or working directory changed since it was opened; open it again",
                    ));
                }
                let owner_revision = opening_revision(
                    &self.controller,
                    &registry,
                    &tick_owner,
                    snapshot.as_deref(),
                )
                .await?;
                let lost_state = lost.state.clone();
                let reopened = LaunchState::Reopened {
                    instance: instance.clone(),
                    host_epoch,
                    by: operation,
                };
                // The lost record turns Reopened only as it was judged: a settlement or another
                // reopen in between leaves it alone and nothing starts.
                let judged = lost.clone();
                let is_claimed = registry
                    .transition_launch(chat, lost.operation, move |mut current| {
                        if current == judged {
                            current.state = reopened;
                            Transition::Write(Box::new(current))
                        } else {
                            Transition::Keep
                        }
                    })
                    .await?;
                if !is_claimed {
                    return Err(HostError::before(
                        "InvalidState",
                        "the lost hosted Tick changed meanwhile; list it again",
                    ));
                }
                let record = LaunchRecord::opening(
                    operation,
                    chat,
                    lost.adapter,
                    lost.command.clone(),
                    lost.cwd.clone(),
                    lost.title.clone(),
                )?;
                match self
                    .launch(&registry, tick_owner, owner_revision, record, environment)
                    .await
                {
                    Ok(opened) => Ok(json!(opened)),
                    Err(error) if !error.dispatched => {
                        registry
                            .transition_launch(chat, lost.operation, move |mut current| {
                                if matches!(current.state, LaunchState::Reopened { by, .. } if by == operation)
                                {
                                    current.state = lost_state;
                                    Transition::Write(Box::new(current))
                                } else {
                                    Transition::Keep
                                }
                            })
                            .await?;
                        Err(error)
                    }
                    Err(error) => Err(error),
                }
            }
            WorkerRequest::CloseHostedTick {
                operation,
                instance,
                ..
            } => {
                let receipt = self
                    .controller
                    .close(operation, &tick_owner, &instance)
                    .await?;
                for record in registry.launches(chat).await? {
                    if record.instance() == Some(&instance) {
                        registry.remove_launch(chat, record.operation).await?;
                    }
                }
                Ok(json!(receipt))
            }
            WorkerRequest::ListHostedTicks { cursor, .. } => {
                // A listing from the start first settles the Chat's records against every page
                // of the host, then names the instances lost with their host: the explicit
                // recovery entries.
                let mut lost = Vec::new();
                if cursor.is_none() {
                    let listing = self.controller.owner_listing(&tick_owner).await?;
                    registry
                        .settle_launches(&self.controller, &tick_owner, &listing)
                        .await?;
                    lost = registry
                        .launches(chat)
                        .await?
                        .into_iter()
                        .filter_map(|record| match record.state {
                            LaunchState::Lost { instance, .. } => Some(LostTick {
                                instance,
                                title: record.title,
                            }),
                            _ => None,
                        })
                        .collect();
                }
                let listed = self.controller.list(&tick_owner, cursor).await?;
                let page = serde_json::from_value(listed)
                    .map_err(|error| HostError::after("ProtocolViolation", error.to_string()))?;
                Ok(json!(HostedTickPage {
                    page,
                    endpoint: self.controller.endpoint(),
                    lost,
                }))
            }
            WorkerRequest::GetHostedOperationReceipt { operation, .. } => {
                let receipt = self.controller.receipt(operation, &tick_owner).await?;
                let settled = receipt.clone();
                registry
                    .transition_launch(chat, operation, move |mut current| {
                        match (&current.state, settled) {
                            (LaunchState::Opening, Some(Receipt::Opened(opened))) => {
                                current.state = LaunchState::Live {
                                    instance: opened.instance,
                                    host_epoch: opened.terminal.epoch,
                                };
                                Transition::Write(Box::new(current))
                            }
                            (LaunchState::Opening, Some(Receipt::Failed(_))) => Transition::Remove,
                            _ => Transition::Keep,
                        }
                    })
                    .await?;
                Ok(json!({"operation":operation,"receipt":receipt}))
            }
            WorkerRequest::SetWatchIntents {
                instance, watches, ..
            } => {
                let count = watches.len();
                registry
                    .set_watches(&self.controller, &tick_owner, &instance, watches)
                    .await?;
                Ok(json!({"instance":instance,"watches":count}))
            }
            WorkerRequest::AllowHostedView { instance, .. } => self
                .controller
                .allow_view(&tick_owner, &instance)
                .await
                .map(|grant| json!(grant)),
            WorkerRequest::AllowShellSpawn { .. } | WorkerRequest::AllowShellView { .. } => {
                unreachable!("Shell spawns and views are granted before a config root is required")
            }
            WorkerRequest::EndStoppedChild {
                child, operation, ..
            } => {
                self.verify_stopped_child(daemon, &worker, chat, child)
                    .await?;
                let child_owner = TickOwner {
                    root: tick_owner.root,
                    chat: child,
                };
                registry
                    .end_stopped(&self.controller, &child_owner, operation)
                    .await?;
                Ok(json!({"operation":operation,"child":child}))
            }
        }
    }
}

/// The registry revision a new launch of `chat` is fenced with, making the Chat Active again when
/// that is what its state means.
async fn opening_revision(
    controller: &HostController,
    registry: &RootRegistry,
    owner: &TickOwner,
    snapshot: Option<&LiveChatSnapshot>,
) -> Result<u64, HostError> {
    let chat = owner.chat;
    match registry.owner_state(chat).await? {
        OwnerState::Active => registry.revision().await,
        // Only an explicit Continue after StopSubagent makes a stopped Chat own Ticks again, under
        // a revision later than the one its end fenced. An end that never completed finishes
        // first: the Continue restores the Chat, not its earlier Ticks.
        stopped @ (OwnerState::Stopping { .. } | OwnerState::Stopped { .. })
            if snapshot.is_some_and(continued_after_stop) =>
        {
            if let OwnerState::Stopping { operation } = stopped {
                registry.finish_stop(controller, owner, operation).await?;
            }
            registry.set_owner_state(chat, &OwnerState::Active).await?;
            registry.next_revision().await
        }
        OwnerState::Stopping { .. } | OwnerState::Stopped { .. } => Err(HostError::before(
            "OwnerEnded",
            "this Chat was stopped and owns no hosted Ticks until it continues",
        )),
    }
}

/// A stopped Chat has continued once one of its Loops is live again: StopSubagent leaves every
/// Loop Stopped or Expired, and only an explicit Continue or new input revives one.
fn continued_after_stop(snapshot: &LiveChatSnapshot) -> bool {
    snapshot
        .conversation
        .loops
        .iter()
        .any(|goal| !matches!(goal.state, LoopState::Stopped | LoopState::Expired))
}

impl WorkerControl {
    /// Opens the Tick `record` describes. The record is durable before the host is asked, so an
    /// unknown outcome is still known to the registry; a failure before dispatch leaves none.
    async fn launch(
        &self,
        registry: &RootRegistry,
        owner: TickOwner,
        owner_revision: u64,
        record: LaunchRecord,
        environment: Vec<(String, String)>,
    ) -> Result<tickhost::host::OpenedTick, HostError> {
        registry.record_launch(&record).await?;
        let (chat, operation) = (record.chat, record.operation);
        let opened = self
            .controller
            .open(OpenRequest {
                operation,
                owner,
                owner_revision,
                adapter: record.adapter,
                command: record.command.clone(),
                cwd: record.cwd.clone(),
                environment,
                title: record.title.clone(),
            })
            .await;
        match opened {
            Ok(opened) => {
                let live = LaunchState::Live {
                    instance: opened.instance.clone(),
                    host_epoch: opened.terminal.epoch,
                };
                let moved_on = live.clone();
                let is_recorded = registry
                    .transition_launch(chat, operation, move |mut current| {
                        if current.state == LaunchState::Opening {
                            current.state = moved_on;
                            Transition::Write(Box::new(current))
                        } else {
                            Transition::Keep
                        }
                    })
                    .await
                    .map_err(|error| HostError::after(&error.code, error.message))?;
                // A settlement that gave up on the record while the open was slow is undone: the
                // Tick runs, so it keeps a record.
                if !is_recorded {
                    let mut record = record;
                    record.state = live;
                    registry
                        .record_launch(&record)
                        .await
                        .map_err(|error| HostError::after(&error.code, error.message))?;
                }
                Ok(opened)
            }
            Err(error) if !error.dispatched => {
                registry.remove_launch(chat, operation).await?;
                Err(error)
            }
            Err(error) => Err(error),
        }
    }

    /// The registry of a worker's config root. On its first use since this daemon started, the
    /// root is reconciled with the host before anything else is served for it.
    async fn root_registry(
        &self,
        config_root: &Path,
        root: &str,
    ) -> Result<Arc<RootRegistry>, HostError> {
        let registry = self.registry.root(config_root).await?;
        let reconciled = self
            .reconciled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(root.to_owned())
            .or_default()
            .clone();
        reconciled
            .get_or_try_init(|| registry.reconcile_with_host(&self.controller, root))
            .await?;
        Ok(registry)
    }

    /// A lifecycle request must have been prepared for this daemon's Tickhost with the tool
    /// that changes Ticks, and a delegated Chat must have inherited that tool for this host. The
    /// ceiling is read through the daemon's own route to the Chat, not from the request.
    async fn admit(
        &self,
        worker: &Arc<dyn AgentAccess>,
        execution: ExecutionRef,
        admission: &HostedAdmission,
    ) -> Result<Box<LiveChatSnapshot>, HostError> {
        let expected = HostToolAuthority::Native {
            endpoint: HostEndpointIdentity::Tickhost {
                endpoint: self.controller.endpoint(),
                uid: tickhost::paths::current_uid(),
            },
        };
        if admission.tool != MODIFY_TICK || admission.authority != expected {
            return Err(HostError::before(
                "Forbidden",
                "the request was not prepared for this daemon's Tickhost",
            ));
        }
        let snapshot = chat_snapshot(worker, execution).await?;
        if let Some(delegation) = &snapshot.conversation.delegation
            && !delegation
                .ceiling
                .host_tools
                .get(&admission.tool)
                .is_some_and(|allowed| allowed.contains(&admission.authority))
        {
            return Err(HostError::before(
                "Forbidden",
                "this delegated Chat did not inherit hosted Tick lifecycles",
            ));
        }
        Ok(snapshot)
    }

    /// StopSubagent's end hook is believed only for a direct child of `root` that runs in the same
    /// worker and whose Loops StopSubagent actually stopped, read through the daemon's own route.
    async fn verify_stopped_child(
        &self,
        daemon: &DaemonServer,
        worker: &Arc<dyn AgentAccess>,
        root: Uuid,
        child: Uuid,
    ) -> Result<(), HostError> {
        let execution = daemon
            .verify_worker_chat(child, worker)
            .await
            .map_err(|error| HostError::before("Forbidden", error.message))?;
        let snapshot = chat_snapshot(worker, execution).await?;
        let is_direct_child = snapshot
            .conversation
            .delegation
            .as_ref()
            .is_some_and(|delegation| delegation.parent_chat_id == root);
        if !is_direct_child || continued_after_stop(&snapshot) {
            return Err(HostError::before(
                "Forbidden",
                "the Chat is not a stopped direct child of this root",
            ));
        }
        Ok(())
    }
}

async fn chat_snapshot(
    worker: &Arc<dyn AgentAccess>,
    execution: ExecutionRef,
) -> Result<Box<LiveChatSnapshot>, HostError> {
    let observed = worker
        .observe(ObservationRequest::Agent {
            execution,
            detail: AgentDetailQuery::Snapshot,
            after: None,
        })
        .await
        .map_err(|error| HostError::before("Forbidden", error.message))?;
    match observed.initial {
        ObservationValue::Agent(snapshot) => Ok(snapshot),
        _ => Err(HostError::before(
            "Forbidden",
            "the Chat's state could not be read",
        )),
    }
}
