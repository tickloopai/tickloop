//! The daemon side of the worker-to-daemon channel for hosted Tick lifecycles and Shell spawns:
//! one private endpoint, one grant per running worker. It verifies that a request's Chat runs in
//! the asking worker, derives the config root from the worker's launch itself, and dispatches
//! through the daemon's own host controller.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
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
    program,
};
use tokio::{
    io::BufReader,
    net::{UnixListener, UnixStream},
    sync::{Mutex, Notify, RwLock},
};
use uuid::Uuid;

use crate::DaemonServer;
use tickloop_code_access::{
    AgentAccess, AgentDetailQuery, ExecutionRef, LiveChatSnapshot, ObservationRequest,
    ObservationValue,
};
use tickloop_harness::protocol::{HostEndpointIdentity, HostToolAuthority};

use tickloop_code_local_environment::hosted::{
    controller::HostController,
    registry::{
        LaunchIdentity, LaunchRecord, LaunchState, Registry, RootRegistry, TARGET_GONE, Transition,
    },
    worker_control::{
        ENDPOINT_NAME, HostedAdmission, HostedTickPage, IssuedSpawn, LostTick, WorkerControlGrant,
        WorkerEnvelope, WorkerRequest, root_identity,
    },
};
use tickloop_harness::runtime::tick_tools::TICK_MANAGE;

/// A worker request is small: an open carries argv, cwd and the launch environment.
const REQUEST_FRAME_BYTES: usize = tick_protocol::MAX_REQUEST_BYTES;
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);

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
    admission: Arc<RwLock<()>>,
    retired: AtomicBool,
    connections: AtomicUsize,
    connections_changed: Notify,
}

struct ConnectionActivity(Arc<WorkerControl>);

impl Drop for ConnectionActivity {
    fn drop(&mut self) {
        self.0.connections.fetch_sub(1, Ordering::SeqCst);
        self.0.connections_changed.notify_one();
    }
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
            admission: Arc::new(RwLock::new(())),
            retired: AtomicBool::new(false),
            connections: AtomicUsize::new(0),
            connections_changed: Notify::new(),
        }
    }

    /// The Tickhost this daemon controls.
    pub fn host_controller(&self) -> &HostController {
        &self.controller
    }

    pub fn endpoint(&self) -> &Path {
        &self.endpoint
    }

    pub(crate) fn try_pause(&self) -> Option<tokio::sync::OwnedRwLockWriteGuard<()>> {
        self.admission.clone().try_write_owned().ok()
    }
    pub(crate) fn retire(&self) {
        self.retired.store(true, Ordering::SeqCst);
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
            binding: tickloop_ids::generate(),
            capability: tickhost::host::random_token(),
            host_endpoint: self.controller.endpoint(),
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
        self.restore_grant(grant, worker, config_root).await;
        if let (Some(config_root), Some(root)) = (config_root.map(Path::to_path_buf), root) {
            let control = self.clone();
            // A failed reconcile is tried again by the root's next request.
            tokio::spawn(async move {
                let _ = control.root_registry(&config_root, &root).await;
            });
        }
    }

    pub(crate) async fn restore_grant(
        &self,
        grant: &WorkerControlGrant,
        worker: Arc<dyn AgentAccess>,
        config_root: Option<&Path>,
    ) {
        self.grants.lock().await.insert(
            grant.binding,
            Grant {
                capability: grant.capability.clone(),
                worker,
                root: config_root.and_then(root_identity),
                config_root: config_root.map(Path::to_path_buf),
            },
        );
    }

    pub async fn revoke(&self, worker: &Arc<dyn AgentAccess>) {
        self.grants
            .lock()
            .await
            .retain(|_, grant| !Arc::ptr_eq(&grant.worker, worker));
    }

    pub(crate) async fn drain(&self) {
        let _ = tokio::time::timeout(CONTROL_TIMEOUT, async {
            while self.connections.load(Ordering::SeqCst) != 0 {
                self.connections_changed.notified().await;
            }
        })
        .await;
    }

    pub async fn serve(self: Arc<Self>, listener: UnixListener) {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let control = self.clone();
                    control.connections.fetch_add(1, Ordering::SeqCst);
                    let activity = ConnectionActivity(control.clone());
                    tokio::spawn(async move {
                        let _activity = activity;
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
        let _admission = self.admission.read().await;
        if self.retired.load(Ordering::SeqCst) {
            return Err(HostError::before(
                "DaemonTransferred",
                "Agent daemon transferred ownership",
            ));
        }
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
        if let Some(admission) = envelope.request.admission() {
            self.admit(&worker, execution, admission).await?;
        }
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
                let owner_revision = registry.revision().await?;
                let record = LaunchRecord::opening(operation, chat, adapter, command, cwd, title)?;
                let environment =
                    with_plugin_environment(environment, &record.command, &config_root)?;
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
                if let LaunchState::Reopened {
                    by,
                    host_epoch,
                    had_ended,
                    ..
                } = lost.state
                {
                    // A reopen whose answer was lost settles from its own receipt, so the
                    // instance is never started twice; one that certainly never started frees
                    // the lost Tick for another reopen.
                    match self.controller.receipt(by, &tick_owner).await? {
                        Some(Receipt::Opened(opened)) => return Ok(json!(opened)),
                        Some(Receipt::Failed(failure)) if !failure.dispatched => {
                            let restored = if had_ended {
                                LaunchState::Ended {
                                    instance: instance.clone(),
                                    host_epoch,
                                }
                            } else {
                                LaunchState::Lost {
                                    instance: instance.clone(),
                                    host_epoch,
                                }
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
                let (host_epoch, had_ended) = match lost.state {
                    LaunchState::Lost { host_epoch, .. } => (host_epoch, false),
                    LaunchState::Ended { host_epoch, .. } => (host_epoch, true),
                    _ => {
                        return Err(HostError::before(
                            "InvalidState",
                            "only a hosted Tick that no longer runs can be reopened",
                        ));
                    }
                };
                let current = LaunchIdentity::of(&lost.command, &lost.cwd).map_err(|error| {
                    // A recorded program that went with its uninstalled Tick package names it.
                    let executable = Path::new(&lost.command[0]);
                    let packages = tick_plugins::PluginStore::in_config_root(&config_root);
                    match program::uninstalled_plugin_of(executable, &packages) {
                        Some(name)
                            if std::fs::symlink_metadata(executable).is_err_and(|error| {
                                error.kind() == std::io::ErrorKind::NotFound
                            }) =>
                        {
                            program::plugin_not_installed(
                                &executable.file_name().unwrap_or_default().to_string_lossy(),
                                &name,
                            )
                        }
                        _ => HostError::before("LaunchFailed", error),
                    }
                })?;
                if current != lost.identity {
                    return Err(HostError::before(
                        "LaunchChanged",
                        "its program or working directory changed since it was opened; open it again",
                    ));
                }
                let owner_revision = registry.revision().await?;
                let environment =
                    with_plugin_environment(environment, &lost.command, &config_root)?;
                let lost_state = lost.state.clone();
                let reopened = LaunchState::Reopened {
                    instance: instance.clone(),
                    host_epoch,
                    by: operation,
                    had_ended,
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
                // The records the closed instance was reopened from go with its own, so nothing
                // outlives its Monitor row.
                let records = registry.launches(chat).await?;
                let mut forgotten = records
                    .iter()
                    .filter(|record| record.instance() == Some(&instance))
                    .map(|record| record.operation)
                    .collect::<Vec<_>>();
                let mut checked = 0;
                while checked < forgotten.len() {
                    let successor = forgotten[checked];
                    checked += 1;
                    forgotten.extend(records.iter().filter_map(|record| {
                        matches!(record.state, LaunchState::Reopened { by, .. } if by == successor)
                            .then_some(record.operation)
                    }));
                }
                for operation in forgotten {
                    registry.remove_launch(chat, operation).await?;
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
                                is_terminal: record.adapter
                                    == tickhost::host::TickAdapter::Terminal,
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
            WorkerRequest::AllowHostedView { instance, .. } => self
                .controller
                .allow_view(&tick_owner, &instance)
                .await
                .map(|grant| json!(grant)),
            WorkerRequest::AllowShellSpawn { .. } | WorkerRequest::AllowShellView { .. } => {
                unreachable!("Shell spawns and views are granted before a config root is required")
            }
        }
    }
}

/// The worker's launch `environment` with the credentials of the Tick package whose program
/// `command` runs, read from `config_root` now: a reopen gets the current login, and no record
/// ever holds a secret. Credentials come from Tickloop alone, so the inherited token file
/// variable, and the variable a package names for its key, give way to them.
fn with_plugin_environment(
    mut environment: Vec<(String, String)>,
    command: &[String],
    config_root: &Path,
) -> Result<Vec<(String, String)>, HostError> {
    let packages = tick_plugins::PluginStore::in_config_root(config_root);
    // Only the exact canonical path of an installed package's program gets its credentials.
    let package = command
        .first()
        .and_then(|program| program::installed_plugin_of(Path::new(program), &packages));
    let key_variable = package
        .as_ref()
        .and_then(|package| match &package.manifest.auth {
            tick_packages::PackageAuth::ApiKey { env, .. } => Some(env.as_str()),
            _ => None,
        });
    const ROOT_VARIABLES: [&str; 2] = ["TICKLOOP_PLUGIN_ROOT", "CLAUDE_PLUGIN_ROOT"];
    const DATA_VARIABLES: [&str; 2] = ["TICKLOOP_PLUGIN_DATA", "CLAUDE_PLUGIN_DATA"];
    environment.retain(|(name, _)| {
        !ROOT_VARIABLES.contains(&name.as_str())
            && !DATA_VARIABLES.contains(&name.as_str())
            && name != tick_packages::OAUTH_TOKEN_FILE_VARIABLE
            && Some(name.as_str()) != key_variable
    });
    if let Some(package) = &package {
        if !package.enabled {
            return Err(program::plugin_disabled(package.name()));
        }
        let data = packages
            .create_data_directory(package.name())
            .map_err(|error| HostError::before("LaunchFailed", error.to_string()))?;
        environment.extend(
            ROOT_VARIABLES
                .map(|name| (name.to_owned(), package.root.to_string_lossy().into_owned())),
        );
        environment.extend(
            DATA_VARIABLES.map(|name| (name.to_owned(), data.to_string_lossy().into_owned())),
        );
    }
    if let Some(package) = &package
        && let Ok(credentials) = packages.credentials(&package.manifest.name)
    {
        environment.extend(credentials.launch_environment(&package.manifest.auth));
    }
    Ok(environment)
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
    ) -> Result<(), HostError> {
        let expected = HostToolAuthority::Native {
            endpoint: HostEndpointIdentity::Tickhost {
                endpoint: self.controller.endpoint(),
                uid: tickhost::paths::current_uid(),
            },
        };
        if admission.tool != TICK_MANAGE || admission.authority != expected {
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

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use tick_packages::{ApiKey, OAUTH_TOKEN_FILE_VARIABLE};
    use tick_plugins::PluginStore;

    use super::*;

    const KEY: &str = "sample-key-41c7";

    #[tokio::test]
    async fn retired_control_drains_accepted_requests_with_an_explicit_predispatch_reply() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let epoch = Uuid::new_v4();
        let controller = Arc::new(HostController::new(
            directory.path(),
            tickhost::paths::HostPaths {
                runtime: directory.path().join("host"),
            },
            epoch,
            directory.path().join("unused-tickhost"),
            Duration::from_secs(1),
        ));
        let control = Arc::new(WorkerControl::new(directory.path(), epoch, controller));
        let (listener, identity) = crate::private_listener(control.endpoint()).unwrap();
        let gate = control.try_pause().unwrap();
        let service = tokio::spawn(control.clone().serve(listener));
        let client = UnixStream::connect(control.endpoint()).await.unwrap();
        let (reader, mut writer) = client.into_split();
        let request_id = Uuid::new_v4();
        let envelope = WorkerEnvelope {
            daemon_epoch: epoch,
            binding: Uuid::new_v4(),
            capability: "private-grant".into(),
            request: WorkerRequest::ListHostedTicks {
                chat: Uuid::new_v4(),
                cursor: None,
            },
        };
        write_frame(
            &mut writer,
            &json!({"id":request_id,"envelope":envelope}),
            REQUEST_FRAME_BYTES,
        )
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while control.connections.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        control.retire();
        service.abort();
        let _ = service.await;
        drop(gate);
        control.drain().await;
        assert_eq!(control.connections.load(Ordering::SeqCst), 0);
        let reply = read_frame(&mut BufReader::new(reader), BUSINESS_FRAME_BYTES)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reply["id"], json!(request_id));
        assert_eq!(reply["error"]["code"], "DaemonTransferred");
        assert_eq!(reply["error"]["dispatched"], false);
        assert!(!directory.path().join("host").exists());
        crate::remove_owned_socket(control.endpoint(), identity).unwrap();
    }

    /// Installs the package `name` that logs in as `auth` declares into `config_root`, from
    /// sources in `directory`, and returns the canonical path of its program.
    fn install(directory: &Path, config_root: &Path, name: &str, auth: Value) -> PathBuf {
        let source = directory.join("sources").join(name);
        std::fs::create_dir_all(source.join("bin")).unwrap();
        std::fs::write(
            source.join(tick_packages::MANIFEST_FILE_NAME),
            json!({
                "name": name,
                "version": "1.0.0",
                "description": "A test package",
                "executable": format!("bin/tick-{name}"),
                "protocol": true,
                "auth": auth,
            })
            .to_string(),
        )
        .unwrap();
        let program = source.join("bin").join(format!("tick-{name}"));
        std::fs::write(&program, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let installed = PluginStore::in_config_root(config_root)
            .install_path(&source)
            .unwrap()
            .plugin
            .executable()
            .unwrap();
        installed.canonicalize().unwrap()
    }

    fn launched(program: &Path, config_root: &Path) -> Vec<(String, String)> {
        let inherited = vec![
            ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
            (
                OAUTH_TOKEN_FILE_VARIABLE.to_owned(),
                "/elsewhere/oauth2.json".to_owned(),
            ),
        ];
        let command = vec![program.to_string_lossy().into_owned(), "quote".to_owned()];
        with_plugin_environment(inherited, &command, config_root).unwrap()
    }

    fn names(environment: &[(String, String)]) -> Vec<&str> {
        environment.iter().map(|(name, _)| name.as_str()).collect()
    }

    /// Credentials go to the exact canonical path of an installed package's program only: a
    /// program of the same name elsewhere, a symlink to the installed one, and a relative or
    /// non-canonical path to it get none, and the inherited token file variable goes either way.
    #[test]
    fn only_the_canonical_installed_program_gets_its_packages_credentials() {
        let directory = tempfile::tempdir().unwrap();
        let config_root = directory.path().join("config");
        let keyed = install(
            directory.path(),
            &config_root,
            "keyed",
            json!({"type":"api_key","env":"SAMPLE_API_KEY","label":"Sample key"}),
        );
        let signed = install(
            directory.path(),
            &config_root,
            "signed",
            json!({"type":"oauth2","issuers":[{"label":"Sample","issuer":"https://auth.example.com"}]}),
        );
        let packages = PluginStore::in_config_root(&config_root);
        packages
            .credentials("keyed")
            .unwrap()
            .save_api_key(&ApiKey::new(KEY).unwrap())
            .unwrap();
        let token_file = packages
            .credentials("signed")
            .unwrap()
            .oauth_token_path()
            .to_string_lossy()
            .into_owned();

        assert_eq!(
            launched(&keyed, &config_root),
            [
                ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
                (
                    "TICKLOOP_PLUGIN_ROOT".into(),
                    packages
                        .get("keyed")
                        .unwrap()
                        .root
                        .to_string_lossy()
                        .into_owned()
                ),
                (
                    "CLAUDE_PLUGIN_ROOT".into(),
                    packages
                        .get("keyed")
                        .unwrap()
                        .root
                        .to_string_lossy()
                        .into_owned()
                ),
                (
                    "TICKLOOP_PLUGIN_DATA".into(),
                    packages
                        .data_directory("keyed")
                        .to_string_lossy()
                        .into_owned()
                ),
                (
                    "CLAUDE_PLUGIN_DATA".into(),
                    packages
                        .data_directory("keyed")
                        .to_string_lossy()
                        .into_owned()
                ),
                ("SAMPLE_API_KEY".to_owned(), KEY.to_owned()),
            ]
        );
        assert_eq!(
            launched(&signed, &config_root),
            [
                ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
                (
                    "TICKLOOP_PLUGIN_ROOT".into(),
                    packages
                        .get("signed")
                        .unwrap()
                        .root
                        .to_string_lossy()
                        .into_owned()
                ),
                (
                    "CLAUDE_PLUGIN_ROOT".into(),
                    packages
                        .get("signed")
                        .unwrap()
                        .root
                        .to_string_lossy()
                        .into_owned()
                ),
                (
                    "TICKLOOP_PLUGIN_DATA".into(),
                    packages
                        .data_directory("signed")
                        .to_string_lossy()
                        .into_owned()
                ),
                (
                    "CLAUDE_PLUGIN_DATA".into(),
                    packages
                        .data_directory("signed")
                        .to_string_lossy()
                        .into_owned()
                ),
                (OAUTH_TOKEN_FILE_VARIABLE.to_owned(), token_file),
            ]
        );

        let elsewhere = directory.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        for installed in [&keyed, &signed] {
            let name = installed.file_name().unwrap();
            let copy = elsewhere.join(name);
            std::fs::copy(installed, &copy).unwrap();
            let link = elsewhere.join("links").join(name);
            std::fs::create_dir_all(link.parent().unwrap()).unwrap();
            symlink(installed, &link).unwrap();
            let relative = installed
                .strip_prefix(directory.path().canonicalize().unwrap())
                .unwrap()
                .to_path_buf();
            let roundabout = installed
                .parent()
                .unwrap()
                .join("..")
                .join("bin")
                .join(name);
            // On macOS the temporary directory is reached through a symlink, so this path names
            // the installed program without being its canonical path.
            let uncanonical = directory.path().join(
                installed
                    .strip_prefix(directory.path().canonicalize().unwrap())
                    .unwrap(),
            );
            for program in [copy, link, relative, roundabout, uncanonical] {
                if program == *installed {
                    continue;
                }
                assert_eq!(
                    names(&launched(&program, &config_root)),
                    ["PATH"],
                    "{}",
                    program.display()
                );
            }
        }
    }
}
