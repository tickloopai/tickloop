//! Shell commands run in the Tickhost. After a command's approval and final check the worker asks
//! its daemon for a one-time spawn grant, then submits the command on its own spawner connection,
//! passing the approved directory and the output file as descriptors; the command lives as long as
//! that connection. A View of the Chat shows a command through a one-time view grant from the same
//! daemon. An Agent without a daemon grants its spawns and views from a Tickhost of its own,
//! served inside its process.

use std::{
    collections::HashMap,
    os::unix::ffi::OsStrExt,
    path::PathBuf,
    sync::{Arc, Mutex, PoisonError, RwLock},
};

use tick_protocol::InstanceRef;
use tickhost::{
    paths::HostPaths,
    shell::{ShellApproval, ShellIdentity, ShellOutcome, SpawnBinding, SpawnRequest},
};
use tickhost_connect::host_error::HostError;
use tickloop_agent_protocol::{CommandState, HostedViewGrant};
use tickloop_agent_tools::{ShellExit, ShellHost, ShellLaunch, ShellProcess, ToolError};
use tokio::sync::{OnceCell, watch};
use uuid::Uuid;

use super::{
    controller::HostController,
    worker_control::{DaemonControl, IssuedSpawn, WorkerRequest},
};

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

/// The grants of an Agent without a daemon: it controls a Tickhost of its own, served inside its
/// process from a private runtime directory, under an epoch and worker identity of its own.
pub struct OwnTickhost {
    directory: tickloop_agent_base::private_fs::PrivateDirectory,
    worker: Uuid,
    controller: OnceCell<HostController>,
}

impl OwnTickhost {
    pub(crate) fn new(directory: tickloop_agent_base::private_fs::PrivateDirectory) -> Self {
        Self {
            directory,
            worker: Uuid::new_v4(),
            controller: OnceCell::new(),
        }
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
                    Uuid::new_v4(),
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

/// Where the Tickhost serves the `shell:` instance of a task, and the token that reads it.
#[derive(Clone, Debug)]
pub struct ShellReader {
    pub endpoint: PathBuf,
    pub instance: InstanceRef,
    pub data_token: String,
}

/// The Shell tasks of this Agent that the Tickhost runs: who grants their spawns and views, and
/// the reader of each running task, which goes once its exit is recorded and its spawner left.
pub struct ShellTasks {
    grants: RwLock<Arc<dyn ShellGrants>>,
    readers: Mutex<HashMap<Uuid, ShellReader>>,
    /// Ticks whenever a task starts or is forgotten.
    changes: watch::Sender<()>,
}

impl ShellTasks {
    pub fn new(grants: Arc<dyn ShellGrants>) -> Arc<Self> {
        Arc::new(Self {
            grants: RwLock::new(grants),
            readers: Mutex::default(),
            changes: watch::channel(()).0,
        })
    }

    /// Replaces who grants later spawns and views: a worker's daemon instead of an own Tickhost.
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

    /// The reader of task `operation` while the Tickhost runs it.
    pub fn reader(&self, operation: Uuid) -> Option<ShellReader> {
        self.readers().get(&operation).cloned()
    }

    /// Tells when a task started or was forgotten.
    pub fn changes(&self) -> watch::Receiver<()> {
        self.changes.subscribe()
    }

    fn set_reader(&self, operation: Uuid, reader: Option<ShellReader>) {
        let mut readers = self.readers();
        match reader {
            Some(reader) => readers.insert(operation, reader),
            None => readers.remove(&operation),
        };
        drop(readers);
        self.changes.send_replace(());
    }

    fn readers(&self) -> std::sync::MutexGuard<'_, HashMap<Uuid, ShellReader>> {
        self.readers.lock().unwrap_or_else(PoisonError::into_inner)
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
    async fn start(&self, launch: ShellLaunch) -> Result<Box<dyn ShellProcess>, ToolError> {
        let unavailable = |error: HostError| {
            ToolError::Rejected(format!("the Tickhost cannot run the command: {error}"))
        };
        let ShellLaunch {
            operation_id,
            shell,
            command,
            description,
            cwd,
            directory,
            output,
        } = launch;
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
                },
            )
            .await
            .map_err(unavailable)?;
        let request = SpawnRequest {
            binding: issued.binding,
            description,
            environment: launch_environment(),
        };
        let submitted = tickhost::shell::submit(
            &issued.endpoint,
            &issued.grant,
            &request,
            &directory,
            &output,
        )
        .await
        .map_err(unavailable)?;
        let tickhost::shell::SubmittedShell {
            spawned,
            mut exits,
            stops,
        } = submitted;
        self.tasks.set_reader(
            operation_id,
            Some(ShellReader {
                endpoint: issued.endpoint,
                instance: spawned.instance,
                data_token: spawned.data_token,
            }),
        );
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
            tasks: self.tasks.clone(),
            operation: operation_id,
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
    tasks: Arc<ShellTasks>,
    operation: Uuid,
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

impl Drop for TickhostProcess {
    fn drop(&mut self) {
        self.tasks.set_reader(self.operation, None);
    }
}
