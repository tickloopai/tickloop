//! Shell commands run in the Tickhost. After a command's approval and final check the worker asks
//! its daemon for a one-time spawn grant, then submits the command on its own spawner connection,
//! passing the approved directory and the output file as descriptors; the command lives as long as
//! that connection. An Agent without a daemon runs no command, unless it is given a Tickhost of its
//! own, served inside its process, as tests are.

use std::{
    os::unix::ffi::OsStrExt,
    sync::{Arc, PoisonError, RwLock},
};

use tickhost::{
    paths::HostPaths,
    shell::{
        ShellApproval, ShellIdentity, ShellOutcome, SpawnBinding, SpawnRequest, environment_digest,
    },
};
use tickhost_connect::host_error::HostError;
use tickloop_agent_protocol::CommandState;
use tickloop_agent_tools::{
    ShellExit, ShellHost, ShellLaunch, ShellProcess, ShellStartError, ToolError,
};
use tokio::sync::{OnceCell, watch};
use uuid::Uuid;

use super::{
    controller::HostController,
    worker_control::{DaemonControl, IssuedSpawn, WorkerRequest},
};

/// Grants spawns of commands a Chat approved.
#[async_trait::async_trait]
pub trait SpawnIssuer: Send + Sync {
    async fn allow_spawn(
        &self,
        chat: Uuid,
        operation: Uuid,
        approval: ShellApproval,
    ) -> Result<IssuedSpawn, HostError>;
}

#[async_trait::async_trait]
impl SpawnIssuer for DaemonControl {
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
}

/// The issuer of an Agent without a daemon: only a daemon grants spawns, so it grants none.
pub(crate) struct NoDaemon;

#[async_trait::async_trait]
impl SpawnIssuer for NoDaemon {
    async fn allow_spawn(
        &self,
        _: Uuid,
        _: Uuid,
        _: ShellApproval,
    ) -> Result<IssuedSpawn, HostError> {
        Err(HostError::before(
            "Unsupported",
            "shell commands run in the Tickhost of the Agent daemon, and this Agent runs without one",
        ))
    }
}

/// An issuer that controls a Tickhost of its own, served inside this process from a private
/// runtime directory, under an epoch and worker identity of its own; it lets an Agent without a
/// daemon run commands in tests.
pub struct OwnTickhost {
    directory: tickloop_agent_base::private_fs::PrivateDirectory,
    worker: Uuid,
    controller: OnceCell<HostController>,
}

impl OwnTickhost {
    pub fn new() -> Self {
        Self {
            directory: crate::transcript::private_tickhost_directory(),
            worker: Uuid::new_v4(),
            controller: OnceCell::new(),
        }
    }
}

impl Default for OwnTickhost {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl SpawnIssuer for OwnTickhost {
    async fn allow_spawn(
        &self,
        chat: Uuid,
        operation: Uuid,
        approval: ShellApproval,
    ) -> Result<IssuedSpawn, HostError> {
        let controller = self
            .controller
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
                    Uuid::new_v4(),
                ))
            })
            .await?;
        let binding = SpawnBinding {
            operation,
            identity: ShellIdentity {
                daemon_epoch: controller.daemon_epoch(),
                worker: self.worker,
                chat,
            },
            approval,
        };
        let grant = controller.allow_spawn(&binding).await?;
        Ok(IssuedSpawn {
            grant,
            binding,
            endpoint: controller.endpoint(),
        })
    }
}

/// The Shell tasks of this Agent that the Tickhost runs, and who grants their spawns.
pub struct ShellTasks {
    issuer: RwLock<Arc<dyn SpawnIssuer>>,
}

impl ShellTasks {
    pub fn new(issuer: Arc<dyn SpawnIssuer>) -> Arc<Self> {
        Arc::new(Self {
            issuer: RwLock::new(issuer),
        })
    }

    /// Replaces who grants later spawns: a worker's daemon, or a test's own Tickhost.
    pub fn install_issuer(&self, issuer: Arc<dyn SpawnIssuer>) {
        *self.issuer.write().unwrap_or_else(PoisonError::into_inner) = issuer;
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
        let issuer = self
            .tasks
            .issuer
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let issued = issuer
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
        let tickhost::shell::SubmittedShell {
            spawned,
            mut exits,
            stops,
        } = submitted;
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
        Ok(Box::new(TickhostProcess {
            pid: spawned.pid,
            exit,
            stops,
        }))
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

/// A command the Tickhost runs on this process's spawner connection. Dropping it closes that
/// connection, which stops the command if it still runs and lets the host forget it.
struct TickhostProcess {
    pid: u32,
    exit: watch::Receiver<Option<ShellExit>>,
    stops: tickhost::shell::ShellStops,
}

#[async_trait::async_trait]
impl ShellProcess for TickhostProcess {
    fn pid(&self) -> u32 {
        self.pid
    }

    async fn exited(&mut self) -> ShellExit {
        let exit = self
            .exit
            .wait_for(Option::is_some)
            .await
            .map(|exit| exit.clone())
            .ok()
            .flatten();
        exit.unwrap_or_else(|| ShellExit {
            exit_code: None,
            signal: None,
            reason: Some("the command's exit was never reported".into()),
            state: CommandState::Unknown,
        })
    }

    async fn stop(&mut self, reason: &str) {
        // A connection that is gone already stopped the command.
        let _ = self.stops.stop(reason).await;
    }
}
