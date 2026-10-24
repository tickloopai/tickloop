//! Shell commands run in the Tickhost. After a command's approval and final check the worker asks
//! its daemon for a one-time spawn grant, then submits the command on its own spawner connection,
//! passing the approved directory and the output file as descriptors; the command lives as long as
//! that connection, until a planned successor takes its ownership. A View of the Chat shows a
//! command through a one-time view grant from the same daemon; the worker keeps an ended command's
//! connection while the Chat's Monitor may list it, so
//! the Tickhost keeps showing it until then. An Agent without a daemon runs no command and shows
//! none, unless it is given a Tickhost of its own, served inside its process, as tests are.

use std::{
    os::unix::ffi::OsStrExt,
    sync::{Arc, OnceLock, PoisonError, RwLock},
    time::Duration,
};

use tick_protocol::InstanceRef;
use tickhost::{
    paths::HostPaths,
    shell::{
        ShellApproval, ShellIdentity, ShellOutcome, SpawnBinding, SpawnRequest, environment_digest,
    },
};
use tickhost_connect::host_error::HostError;
use tickloop_agent_protocol::{CommandState, HostedViewGrant};
use tickloop_agent_tools::{
    ShellExit, ShellHost, ShellLaunch, ShellProcess, ShellStartError, ToolError,
};
use tokio::sync::{Mutex, OnceCell, watch};
use uuid::Uuid;

use super::{
    controller::HostController,
    worker_control::{DaemonControl, IssuedSpawn, WorkerRequest},
};
use crate::WeakAgent;

/// How often a worker checks whether the Monitor still lists an ended command it keeps.
const MONITOR_LISTING_CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// Grants spawns of commands a Chat approved, and views of the commands it started.
#[async_trait::async_trait]
pub trait ShellGrants: Send + Sync {
    async fn allow_spawn(
        &self,
        chat: Uuid,
        operation: Uuid,
        approval: ShellApproval,
    ) -> Result<IssuedSpawn, HostError>;

    async fn allow_view(
        &self,
        chat: Uuid,
        instance: &InstanceRef,
    ) -> Result<HostedViewGrant, HostError>;
}

#[async_trait::async_trait]
impl ShellGrants for DaemonControl {
    async fn allow_spawn(
        &self,
        chat: Uuid,
        operation: Uuid,
        approval: ShellApproval,
    ) -> Result<IssuedSpawn, HostError> {
        let issued = self
            .request(WorkerRequest::AllowShellSpawn {
                chat,
                operation,
                approval,
            })
            .await?;
        serde_json::from_value(issued)
            .map_err(|error| HostError::after("ProtocolViolation", error.to_string()))
    }

    async fn allow_view(
        &self,
        chat: Uuid,
        instance: &InstanceRef,
    ) -> Result<HostedViewGrant, HostError> {
        let granted = self
            .request(WorkerRequest::AllowShellView {
                chat,
                instance: instance.clone(),
            })
            .await?;
        serde_json::from_value(granted)
            .map_err(|error| HostError::after("ProtocolViolation", error.to_string()))
    }
}

/// The grants of an Agent without a daemon: only a daemon grants spawns and views, so it grants
/// none.
pub(crate) struct NoDaemon;

impl NoDaemon {
    fn refusal() -> HostError {
        HostError::before(
            "Unsupported",
            "shell commands run in the Tickhost of the Agent daemon, and this Agent runs without one",
        )
    }
}

#[async_trait::async_trait]
impl ShellGrants for NoDaemon {
    async fn allow_spawn(
        &self,
        _: Uuid,
        _: Uuid,
        _: ShellApproval,
    ) -> Result<IssuedSpawn, HostError> {
        Err(Self::refusal())
    }

    async fn allow_view(&self, _: Uuid, _: &InstanceRef) -> Result<HostedViewGrant, HostError> {
        Err(Self::refusal())
    }
}

/// Grants that control a Tickhost of its own, served inside this process from a private runtime
/// directory, under an epoch and worker identity of its own; they let an Agent without a daemon
/// run and show commands in tests.
pub struct OwnTickhost {
    directory: tickloop_agent_base::private_fs::PrivateDirectory,
    worker: Uuid,
    idle_timeout: Duration,
    controller: OnceCell<HostController>,
}

impl OwnTickhost {
    pub fn new() -> Self {
        Self::with_idle_timeout(tickhost::host::DEFAULT_IDLE_TIMEOUT)
    }

    /// A Tickhost that forgets an ended command nobody holds or views after `idle_timeout`.
    pub fn with_idle_timeout(idle_timeout: Duration) -> Self {
        Self {
            directory: crate::transcript::private_tickhost_directory(),
            worker: tickloop_ids::generate(),
            idle_timeout,
            controller: OnceCell::new(),
        }
    }
}

impl Default for OwnTickhost {
    fn default() -> Self {
        Self::new()
    }
}

impl OwnTickhost {
    async fn controller(&self) -> Result<&HostController, HostError> {
        self.controller
            .get_or_try_init(|| async {
                self.directory
                    .create()
                    .map_err(|error| HostError::before("HostUnavailable", error.to_string()))?;
                Ok::<_, HostError>(HostController::in_process(
                    self.directory.path(),
                    HostPaths {
                        runtime: self
                            .directory
                            .path()
                            .join(crate::transcript::TICKHOST_RUNTIME_SUBDIRECTORY),
                    },
                    tickloop_ids::generate(),
                    self.idle_timeout,
                ))
            })
            .await
    }

    fn identity(&self, controller: &HostController, chat: Uuid) -> ShellIdentity {
        ShellIdentity {
            daemon_epoch: controller.daemon_epoch(),
            worker: self.worker,
            chat,
        }
    }
}

#[async_trait::async_trait]
impl ShellGrants for OwnTickhost {
    async fn allow_spawn(
        &self,
        chat: Uuid,
        operation: Uuid,
        approval: ShellApproval,
    ) -> Result<IssuedSpawn, HostError> {
        let controller = self.controller().await?;
        let binding = SpawnBinding {
            operation,
            identity: self.identity(controller, chat),
            approval,
        };
        let grant = controller.allow_spawn(&binding).await?;
        Ok(IssuedSpawn {
            grant,
            binding,
            endpoint: controller.endpoint(),
        })
    }

    async fn allow_view(
        &self,
        chat: Uuid,
        instance: &InstanceRef,
    ) -> Result<HostedViewGrant, HostError> {
        let controller = self.controller().await?;
        controller
            .allow_shell_view(&self.identity(controller, chat), instance)
            .await
    }
}

/// The Shell tasks of this Agent that the Tickhost runs, and who grants their spawns and views.
pub struct ShellTasks {
    grants: RwLock<Arc<dyn ShellGrants>>,
    /// The Agent whose Monitors decide how long an ended command stays viewable.
    agent: OnceLock<WeakAgent>,
    pending_handoffs: Mutex<Vec<Arc<Mutex<tickhost::shell::ShellStops>>>>,
    retained_handoffs:
        std::sync::Mutex<std::collections::HashMap<Uuid, (Uuid, tickhost::shell::ShellHandoff)>>,
}

impl ShellTasks {
    pub(crate) async fn commit_handoffs(&self) -> Result<(), HostError> {
        let handoffs = std::mem::take(&mut *self.pending_handoffs.lock().await);
        for owner in handoffs {
            owner.lock().await.commit_handoff().await?;
        }
        Ok(())
    }

    pub fn new(grants: Arc<dyn ShellGrants>) -> Arc<Self> {
        Arc::new(Self {
            grants: RwLock::new(grants),
            agent: OnceLock::new(),
            pending_handoffs: Mutex::new(Vec::new()),
            retained_handoffs: std::sync::Mutex::new(std::collections::HashMap::new()),
        })
    }

    pub(crate) fn follow_monitor(&self, agent: WeakAgent) {
        let _ = self.agent.set(agent);
    }

    /// Keeps `chat`'s ended command `task` on its spawner connection, which keeps the Tickhost
    /// from forgetting it, for as long as the Chat's Monitor may list it. Closing the connection
    /// leaves the command to the Tickhost, which forgets it once nobody viewed it for its idle
    /// timeout.
    fn keep_while_listed(
        self: &Arc<Self>,
        chat: Uuid,
        task: Uuid,
        connection: Arc<Mutex<tickhost::shell::ShellStops>>,
        handoff: tickhost::shell::ShellHandoff,
    ) {
        let Some(agent) = self.agent.get().cloned() else {
            return;
        };
        let Some(owner) = agent.upgrade() else {
            return;
        };
        self.retained_handoffs
            .lock()
            .unwrap()
            .insert(task, (chat, handoff));
        let tasks = self.clone();
        owner.inner.tasks.spawn(async move {
            loop {
                let Some(listing) = agent.upgrade() else {
                    tasks.retained_handoffs.lock().unwrap().remove(&task);
                    drop(connection);
                    return;
                };
                let guard = listing.inner.upgrade_gate.clone().read_owned().await;
                if listing.inner.shutdown.is_cancelled()
                    || !listing.monitor_may_list_shell(chat.into(), task).await
                {
                    tasks.retained_handoffs.lock().unwrap().remove(&task);
                    drop(connection);
                    return;
                }
                // Waiting holds no Agent, so an Agent dropped meanwhile closes the connection.
                let shutdown = listing.inner.shutdown.clone();
                drop(guard);
                drop(listing);
                tokio::select! {
                    _ = shutdown.cancelled() => {},
                    _ = tokio::time::sleep(MONITOR_LISTING_CHECK_INTERVAL) => {}
                }
            }
        });
    }

    /// Replaces who grants later spawns and views: a worker's daemon, or a test's own Tickhost.
    pub fn install_grants(&self, grants: Arc<dyn ShellGrants>) {
        *self.grants.write().unwrap_or_else(PoisonError::into_inner) = grants;
    }

    /// Lets one View of `chat` show its Shell task `instance` once, running or ended.
    pub async fn allow_view(
        &self,
        chat: Uuid,
        instance: &InstanceRef,
    ) -> Result<HostedViewGrant, HostError> {
        self.grants().allow_view(chat, instance).await
    }

    fn grants(&self) -> Arc<dyn ShellGrants> {
        self.grants
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The host of `chat`'s commands.
    pub fn for_chat(self: &Arc<Self>, chat: Uuid) -> Arc<dyn ShellHost> {
        Arc::new(ChatShells {
            tasks: self.clone(),
            chat,
        })
    }
}

struct ChatShells {
    tasks: Arc<ShellTasks>,
    chat: Uuid,
}

/// The environment commands start with: the worker's own, which is the View environment it was
/// started with.
fn launch_environment() -> Vec<(Vec<u8>, Vec<u8>)> {
    std::env::vars_os()
        .map(|(name, value)| (name.as_bytes().to_vec(), value.as_bytes().to_vec()))
        .collect()
}

#[async_trait::async_trait]
impl ShellHost for ChatShells {
    async fn retained_handoff(
        &self,
        operation: Uuid,
    ) -> Result<Option<serde_json::Value>, ToolError> {
        let retained = self.tasks.retained_handoffs.lock().unwrap();
        let Some((_, handoff)) = retained
            .get(&operation)
            .filter(|(chat, _)| *chat == self.chat)
        else {
            return Ok(None);
        };
        if handoff.owner_token.is_empty() {
            return Err(ToolError::Rejected(
                "the existing Tickhost cannot transfer this retained Shell".into(),
            ));
        }
        Ok(Some(
            serde_json::to_value(handoff).expect("Shell handoff serializes"),
        ))
    }
    async fn adopt(&self, value: serde_json::Value) -> Result<Box<dyn ShellProcess>, ToolError> {
        let handoff: tickhost::shell::ShellHandoff = serde_json::from_value(value)
            .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
        let submitted = tickhost::shell::adopt(&handoff).await.map_err(|error| {
            ToolError::Rejected(format!("Shell ownership transfer failed: {error}"))
        })?;
        let stops = Arc::new(Mutex::new(submitted.stops));
        self.tasks.pending_handoffs.lock().await.push(stops.clone());
        Ok(self.process(submitted.spawned, submitted.exits, stops, handoff.endpoint))
    }

    async fn start(&self, launch: ShellLaunch) -> Result<Box<dyn ShellProcess>, ShellStartError> {
        let refused = |error: HostError| {
            ShellStartError::NotStarted(ToolError::Rejected(format!(
                "the Tickhost cannot run the command: {error}"
            )))
        };
        let ShellLaunch {
            operation_id,
            shell,
            command,
            description,
            cwd,
            directory,
            output,
            cancel,
        } = launch;
        let environment = launch_environment();
        let issued = self
            .tasks
            .grants()
            .allow_spawn(
                self.chat,
                operation_id,
                ShellApproval {
                    shell,
                    command,
                    cwd,
                    environment: environment_digest(&environment),
                },
            )
            .await
            .map_err(refused)?;
        // A stop or cancel that came while the grant was asked for leaves the command unstarted;
        // the grant expires unclaimed.
        if cancel.is_cancelled() {
            return Err(ShellStartError::NotStarted(ToolError::Cancelled));
        }
        let request = SpawnRequest {
            binding: issued.binding,
            description,
            environment,
        };
        let submitted = tickhost::shell::submit(
            &issued.endpoint,
            &issued.grant,
            &request,
            &directory,
            &output,
        )
        .await
        .map_err(|error| {
            if error.dispatched {
                ShellStartError::OutcomeUnknown(format!(
                    "the Tickhost may have started the command before its answer was lost \
                     ({error}); the outcome is unknown, so do not run it again before checking \
                     what it did"
                ))
            } else {
                refused(error)
            }
        })?;
        Ok(self.process(
            submitted.spawned,
            submitted.exits,
            Arc::new(Mutex::new(submitted.stops)),
            issued.endpoint,
        ))
    }
}

impl ChatShells {
    fn process(
        &self,
        spawned: tickhost::shell::SpawnedShell,
        mut exits: tickhost::shell::ShellExits,
        stops: Arc<Mutex<tickhost::shell::ShellStops>>,
        endpoint: std::path::PathBuf,
    ) -> Box<dyn ShellProcess> {
        // The exit arrives on its own task, so waiting for it can be given up and resumed.
        let (reported, exit) = watch::channel(None);
        tokio::spawn(async move {
            let exit = match exits.exited().await {
                Ok(exit) => command_exit(exit),
                Err(error) => ShellExit {
                    exit_code: None,
                    signal: None,
                    reason: Some(format!(
                        "the Tickhost connection was lost before the command ended: {error}"
                    )),
                    state: CommandState::Unknown,
                },
            };
            reported.send_replace(Some(exit));
        });
        let task = spawned
            .instance
            .as_str()
            .strip_prefix("shell:")
            .and_then(|value| Uuid::parse_str(value).ok())
            .expect("a submitted Shell has an operation UUID");
        Box::new(TickhostProcess {
            pid: spawned.pid,
            handoff: tickhost::shell::ShellHandoff {
                endpoint,
                instance: spawned.instance,
                owner_token: spawned.owner_token,
                handoff: tickloop_ids::generate(),
            },
            exit,
            stops: Some(stops),
            tasks: self.tasks.clone(),
            chat: self.chat,
            task,
        })
    }
}

fn command_exit(exit: tickhost::shell::ShellExit) -> ShellExit {
    ShellExit {
        exit_code: exit.code,
        signal: exit.signal,
        reason: exit.reason,
        state: match exit.outcome {
            ShellOutcome::Completed => CommandState::Completed,
            ShellOutcome::Failed => CommandState::Failed,
            ShellOutcome::Unknown => CommandState::Unknown,
        },
    }
}

/// A command the Tickhost runs on this process's spawner connection. Dropping it before the
/// command ended closes that connection, which stops the command; once it ended, the connection
/// passes to the Shell tasks, which keep it while Monitor may list the command.
struct TickhostProcess {
    pid: u32,
    handoff: tickhost::shell::ShellHandoff,
    exit: watch::Receiver<Option<ShellExit>>,
    stops: Option<Arc<Mutex<tickhost::shell::ShellStops>>>,
    tasks: Arc<ShellTasks>,
    chat: Uuid,
    task: Uuid,
}

#[async_trait::async_trait]
impl ShellProcess for TickhostProcess {
    fn pid(&self) -> u32 {
        self.pid
    }

    fn handoff(&self) -> Option<serde_json::Value> {
        (!self.handoff.owner_token.is_empty())
            .then(|| serde_json::to_value(&self.handoff).expect("Shell handoff serializes"))
    }

    async fn exited(&mut self) -> ShellExit {
        let exit = self
            .exit
            .wait_for(Option::is_some)
            .await
            .map(|exit| exit.clone())
            .ok()
            .flatten();
        if let Some(stops) = self.stops.take() {
            self.tasks
                .keep_while_listed(self.chat, self.task, stops, self.handoff.clone());
        }
        exit.unwrap_or_else(|| ShellExit {
            exit_code: None,
            signal: None,
            reason: Some("the command's exit was never reported".into()),
            state: CommandState::Unknown,
        })
    }

    async fn stop(&mut self, reason: &str) {
        // A connection that is gone already stopped the command, and one passed on belongs to a
        // command that ended.
        if let Some(stops) = &mut self.stops {
            let _ = stops.lock().await.stop(reason).await;
        }
    }
}
