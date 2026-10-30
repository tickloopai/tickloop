use std::{
    collections::HashMap,
    fs::File,
    os::unix::fs::FileExt,
    path::Path,
    sync::{Arc, Weak},
};

use serde_json::json;
use tick_protocol::{InstanceRef, TickState};
use tickloop_harness::protocol::{ChatId, CommandState, ToolResult};
use tickloop_harness::tools::{
    ShellHost, ShellLaunch, ShellProcess, ShellStartError, ShellStopError, ToolError,
};
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::*;
use crate::{
    host_tools::HostToolPermission,
    hosted::{
        registry::{FileIdentity, PreparedProgram},
        shells::{ShellGrants, ShellTasks, launch_environment},
    },
};

type ShellEntries = HashMap<(ChatId, InstanceRef), Option<Arc<ShellEntry>>>;

pub(crate) struct LocalShells {
    tasks: Arc<ShellTasks>,
    sandbox: std::sync::RwLock<Option<tickloop_code_settings::UserSettings>>,
    epoch: Uuid,
    plans: std::sync::Mutex<HashMap<(Uuid, PreparationId), ShellPlan>>,
    stops: std::sync::Mutex<HashMap<(Uuid, PreparationId), PreparedStop>>,
    entries: Mutex<ShellEntries>,
}

struct ShellPlan {
    lifetime: Weak<()>,
    spec: ShellSpec,
    directory: File,
    output: PreparedOutput,
    environment: Vec<(Vec<u8>, Vec<u8>)>,
    executable: PreparedProgram,
    directory_identity: FileIdentity,
    sandbox: Option<tickhost::sandbox::SandboxPolicy>,
    warning: Option<String>,
    escape_guidance: bool,
}

enum PreparedOutput {
    Existing(File),
    New {
        directory: File,
        identity: FileIdentity,
    },
}

struct ShellEntry {
    output: Option<Arc<BoundOutput>>,
    handoff: Option<serde_json::Value>,
    stops: mpsc::Sender<StopRequest>,
    status: watch::Receiver<ShellStatus>,
    updates: watch::Sender<ShellStatus>,
}

struct BoundOutput {
    escape_guidance: bool,
    path: std::path::PathBuf,
    identity: FileIdentity,
    open: std::sync::Mutex<Option<Arc<File>>>,
}

impl BoundOutput {
    fn new(file: File, path: String, escape_guidance: bool) -> Result<Self, ExecutionError> {
        let identity = FileIdentity::opened(&file).map_err(preparation_error)?;
        Ok(Self {
            path: path.into(),
            escape_guidance,
            identity,
            open: std::sync::Mutex::new(Some(Arc::new(file))),
        })
    }
    fn file(&self) -> Result<Arc<File>, ExecutionError> {
        if let Some(file) = self.open.lock().unwrap().as_ref() {
            return Ok(file.clone());
        }
        let file = tickloop_harness::base::private_fs::open_without_following_links(
            &self.path,
            tickloop_harness::base::private_fs::OpenTarget::Entry,
        )
        .map_err(io_error)?;
        if !file.metadata().map_err(io_error)?.is_file()
            || FileIdentity::opened(&file).map_err(preparation_error)? != self.identity
        {
            return Err(ExecutionError::new(
                "OutputChanged",
                "Shell output file was replaced",
            ));
        }
        Ok(Arc::new(file))
    }
    fn length(&self) -> Result<u64, ExecutionError> {
        Ok(self.file()?.metadata().map_err(io_error)?.len())
    }
    fn release(&self) {
        self.open.lock().unwrap().take();
    }
}

struct PreparedStop {
    lifetime: Weak<()>,
    instance: InstanceRef,
    reason: String,
}
struct StopRequest {
    dispatch: ExecutionDispatch,
    reason: String,
    reply: oneshot::Sender<Result<(), ShellStopError>>,
}

impl LocalShells {
    pub(crate) fn new(grants: Arc<dyn ShellGrants>) -> Arc<Self> {
        Self::from_tasks(ShellTasks::new(grants))
    }

    fn from_tasks(tasks: Arc<ShellTasks>) -> Arc<Self> {
        let mut existing = tasks.execution_environment.lock().unwrap();
        if let Some(environment) = existing.upgrade() {
            return environment;
        }
        let environment = Arc::new(Self {
            tasks: tasks.clone(),
            sandbox: Default::default(),
            epoch: tickloop_ids::generate(),
            plans: Default::default(),
            stops: Default::default(),
            entries: Mutex::new(HashMap::new()),
        });
        *existing = Arc::downgrade(&environment);
        environment
    }

    pub(crate) fn set_sandbox(&self, settings: tickloop_code_settings::UserSettings) {
        *self.sandbox.write().unwrap() = Some(settings);
    }

    pub(crate) fn guidance(&self, folder: Option<&str>) -> ExecutionGuidance {
        let settings = self.sandbox.read().unwrap().clone();
        super::sandbox::guidance(settings.as_ref(), folder)
    }

    pub(crate) fn tasks(&self) -> Arc<ShellTasks> {
        self.tasks.clone()
    }

    pub async fn prepare(
        self: &Arc<Self>,
        port: Uuid,
        chat: ChatId,
        authority: &ExecutionAuthority,
        mut spec: ShellSpec,
        staged: Option<ShellLaunch>,
    ) -> Result<PreparedExecution, ExecutionError> {
        let settings = self.sandbox.read().unwrap().clone();
        let mut sandbox = super::sandbox::prepare(settings.as_ref(), &spec).await?;
        if let Some(policy) = &mut sandbox.policy
            && let tickhost::sandbox::SandboxNetwork::Allowlist { proxy_port, .. } =
                &mut policy.network
        {
            *proxy_port = self
                .tasks
                .sandbox_proxy_port(chat.into())
                .await
                .map_err(|error| ExecutionError::new(error.code, error.message))?;
        }
        // The shell runs by the path it was chosen by, links kept, so it sees the name it was
        // invoked as; the approval binds the file that path names.
        let shell = std::path::absolute(&spec.shell).map_err(io_error)?;
        shell.metadata().map_err(io_error)?;
        let cwd = Path::new(&spec.cwd).canonicalize().map_err(io_error)?;
        let output_path = if Path::new(&spec.output).is_absolute() {
            std::path::PathBuf::from(&spec.output)
        } else {
            cwd.join(&spec.output)
        };
        let parent = output_path
            .parent()
            .ok_or_else(|| preparation_error("Shell output has no parent"))?;
        output_path
            .file_name()
            .ok_or_else(|| preparation_error("Shell output has no file name"))?;
        let (directory, output) = match staged {
            Some(launch) => {
                if launch.operation_id != spec.operation
                    || launch.command != spec.command
                    || launch.shell != Path::new(&spec.shell)
                    || launch.cwd != Path::new(&spec.cwd)
                    || launch.output_path != output_path
                    || launch.description != spec.description
                {
                    return Err(preparation_error(
                        "Shell submission does not match the prepared facts",
                    ));
                }
                if !launch.output.metadata().map_err(io_error)?.is_file() {
                    return Err(preparation_error("Shell output is not a regular file"));
                }
                (launch.directory, PreparedOutput::Existing(launch.output))
            }
            None => {
                let output_directory =
                    tickloop_harness::base::private_fs::open_without_following_links(
                        &parent.canonicalize().map_err(io_error)?,
                        tickloop_harness::base::private_fs::OpenTarget::Directory {
                            create_mode: None,
                        },
                    )
                    .map_err(io_error)?;
                let identity =
                    FileIdentity::opened(&output_directory).map_err(preparation_error)?;
                if !output_directory.metadata().map_err(io_error)?.is_dir()
                    || FileIdentity::directory(parent).map_err(preparation_error)? != identity
                {
                    return Err(preparation_error(
                        "Shell output directory changed during preparation",
                    ));
                }
                (
                    tickloop_harness::base::private_fs::open_without_following_links(
                        &cwd,
                        tickloop_harness::base::private_fs::OpenTarget::Directory {
                            create_mode: None,
                        },
                    )
                    .map_err(io_error)?,
                    PreparedOutput::New {
                        directory: output_directory,
                        identity,
                    },
                )
            }
        };
        let directory_identity = FileIdentity::opened(&directory).map_err(preparation_error)?;
        if !directory.metadata().map_err(io_error)?.is_dir()
            || FileIdentity::directory(&cwd).map_err(preparation_error)? != directory_identity
        {
            return Err(preparation_error(
                "working directory changed during preparation",
            ));
        }
        let executable = PreparedProgram::of(&shell).map_err(preparation_error)?;
        spec.shell = shell.to_string_lossy().into_owned();
        spec.cwd = cwd.to_string_lossy().into_owned();
        spec.output = output_path.to_string_lossy().into_owned();
        let input = json!({"command":spec.command, "cwd":spec.cwd, "description":spec.description});
        let owner = Arc::downgrade(self);
        let operation = spec.operation;
        let prepared = PreparedExecution::for_operation(
            operation,
            port,
            authority,
            ExecutionFacts {
                intent: ExecutionIntent::SpawnShell(spec.clone()),
                normalized_input: input.clone(),
                permission: HostToolPermission::Reviewable,
                summary: format!("Run {} in {}", spec.command, spec.cwd),
                approval: tickloop_harness::protocol::PreparedApprovalFacts {
                    normalized_input: input,
                    canonical_path: Some(cwd),
                    ..Default::default()
                },
            },
        );
        let prepared = prepared
            .with_isolation(sandbox.isolation)
            .with_required_approval(sandbox.requires_approval)
            .with_approval_skipped_when_isolated(sandbox.skips_approval_when_isolated);
        let preparation = prepared.preparation();
        let prepared = prepared.with_cleanup(move || {
            if let Some(owner) = owner.upgrade() {
                owner
                    .plans
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&(port, preparation));
            }
        });
        let mut plans = self.plans.lock().unwrap();
        plans.retain(|_, plan| plan.lifetime.strong_count() > 0);
        plans.insert(
            (port, preparation),
            ShellPlan {
                lifetime: prepared.lifetime(),
                spec,
                directory,
                output,
                environment: launch_environment(),
                executable,
                directory_identity,
                sandbox: sandbox.policy,
                warning: sandbox.warning,
                escape_guidance: sandbox.escape_guidance,
            },
        );
        Ok(prepared)
    }

    pub async fn prepare_stop(
        &self,
        port: Uuid,
        chat: ChatId,
        authority: &ExecutionAuthority,
        instance: InstanceRef,
        reason: String,
    ) -> Result<PreparedExecution, ExecutionError> {
        self.entry(chat, &instance).await?;
        let input = json!({"task_id":instance});
        let prepared = PreparedExecution::new(
            port,
            authority,
            ExecutionFacts {
                intent: ExecutionIntent::StopShell {
                    instance: instance.clone(),
                    reason: reason.clone(),
                },
                normalized_input: input.clone(),
                permission: HostToolPermission::Reviewable,
                summary: format!("Stop Shell {}", instance.public()),
                approval: tickloop_harness::protocol::PreparedApprovalFacts {
                    normalized_input: input,
                    ..Default::default()
                },
            },
        );
        let mut stops = self.stops.lock().unwrap();
        stops.retain(|_, plan| plan.lifetime.strong_count() > 0);
        stops.insert(
            (port, prepared.preparation()),
            PreparedStop {
                lifetime: prepared.lifetime(),
                instance,
                reason,
            },
        );
        Ok(prepared)
    }

    async fn execute_stop(
        &self,
        port: Uuid,
        chat: ChatId,
        prepared: PreparedExecution,
        cancel: CancellationToken,
        dispatch: ExecutionDispatch,
    ) -> ExecutionOutcome {
        let operation = prepared.operation();
        let Some(stop) = self
            .stops
            .lock()
            .unwrap()
            .remove(&(port, prepared.preparation()))
        else {
            return refused(
                "UnknownPreparation",
                "Shell stop preparation was consumed or expired",
            );
        };
        if cancel.is_cancelled() {
            return refused("Cancelled", "Shell stop cancelled before dispatch");
        }
        let entry = match self.entry(chat, &stop.instance).await {
            Ok(entry) => entry,
            Err(error) => return ExecutionOutcome::NotDispatched(error),
        };
        let (reply, answer) = oneshot::channel();
        if entry
            .stops
            .send(StopRequest {
                reason: stop.reason,
                reply,
                dispatch,
            })
            .await
            .is_err()
        {
            return refused("TargetGone", "Shell already exited");
        }
        let result = tokio::select! {
            _ = cancel.cancelled() => return ExecutionOutcome::OutcomeUnknown { operation, reason: "Shell stop was cancelled after dispatch".into() },
            answer = answer => answer,
        };
        match result {
            Ok(Err(ShellStopError::NotDispatched(reason))) => {
                return refused("NotDispatched", reason);
            }
            Ok(Err(ShellStopError::OutcomeUnknown(reason))) => {
                return ExecutionOutcome::OutcomeUnknown { operation, reason };
            }
            Err(_)
                if !matches!(
                    entry.status.borrow().state,
                    CommandState::Completed | CommandState::Failed | CommandState::Unknown
                ) =>
            {
                return ExecutionOutcome::OutcomeUnknown {
                    operation,
                    reason: "Shell stop reply was lost".into(),
                };
            }
            _ => {}
        }
        let mut status = entry.updates.subscribe();
        let ended = tokio::time::timeout(
            tickhost_connect::hosted::CONTROL_TIMEOUT,
            status.wait_for(|status| {
                matches!(
                    status.state,
                    CommandState::Completed | CommandState::Failed | CommandState::Unknown
                )
            }),
        )
        .await;
        match ended {
            Ok(Ok(status)) if status.state != CommandState::Unknown => {
                ExecutionOutcome::Completed(ExecutionResult {
                    result: ToolResult {
                        content: json!({"status":"stopped", "tick_instance_id":stop.instance})
                            .to_string(),
                        is_error: false,
                        output_ref: None,
                    },
                    opened: None,
                    shell: None,
                    command: None,
                    handoff: None,
                })
            }
            _ => ExecutionOutcome::OutcomeUnknown {
                operation,
                reason: "Shell stop was dispatched but its exit was not confirmed".into(),
            },
        }
    }

    pub async fn execute(
        &self,
        port: Uuid,
        chat: ChatId,
        prepared: PreparedExecution,
        cancel: CancellationToken,
        dispatch: ExecutionDispatch,
    ) -> ExecutionOutcome {
        if matches!(prepared.facts().intent, ExecutionIntent::StopShell { .. }) {
            return self
                .execute_stop(port, chat, prepared, cancel, dispatch)
                .await;
        }
        let operation = prepared.operation();
        let plan = self
            .plans
            .lock()
            .unwrap()
            .remove(&(port, prepared.preparation()));
        let Some(plan) = plan else {
            return refused(
                "UnknownPreparation",
                "Shell preparation was consumed or expired",
            );
        };
        if cancel.is_cancelled() {
            return refused("Cancelled", "Shell cancelled before dispatch");
        }
        if PreparedProgram::of(Path::new(&plan.spec.shell)).ok() != Some(plan.executable)
            || FileIdentity::directory(Path::new(&plan.spec.cwd)).ok()
                != Some(plan.directory_identity)
            || FileIdentity::opened(&plan.directory).ok() != Some(plan.directory_identity)
        {
            return refused(
                "TargetChanged",
                "the executable or working directory changed before dispatch; prepare and approve again",
            );
        }
        let path = plan.spec.shell.clone();
        let approved = plan.executable;
        dispatch.revalidate_before_send(move || {
            if PreparedProgram::of(Path::new(&path)).ok() != Some(approved) {
                return Err(ExecutionError::new(
                    "TargetChanged",
                    "the executable changed before dispatch; prepare and approve again",
                ));
            }
            Ok(())
        });
        let ExecutionIntent::SpawnShell(spec) = &prepared.facts().intent else {
            return refused("InvalidPreparation", "not a Shell preparation");
        };
        let instance = InstanceRef::shell(operation);
        let key = (chat, instance.clone());
        {
            let mut entries = self.entries.lock().await;
            if entries.contains_key(&key) {
                return refused(
                    "DuplicateOperation",
                    "Shell operation was already dispatched",
                );
            }
            entries.insert(key.clone(), None);
        }
        let output_file = match plan.output {
            PreparedOutput::Existing(file) => Ok(file),
            PreparedOutput::New {
                directory,
                identity,
            } => {
                let path = Path::new(&spec.output);
                if path
                    .parent()
                    .and_then(|parent| FileIdentity::directory(parent).ok())
                    != Some(identity)
                    || FileIdentity::opened(&directory).ok() != Some(identity)
                {
                    Err(ExecutionError::new(
                        "TargetChanged",
                        "Shell output directory changed before dispatch",
                    ))
                } else {
                    create_output(&directory, path)
                }
            }
        };
        let output_file = match output_file {
            Ok(output) => output,
            Err(error) => {
                self.entries.lock().await.remove(&key);
                return ExecutionOutcome::NotDispatched(error);
            }
        };
        let output = match output_file
            .try_clone()
            .map_err(io_error)
            .and_then(|file| BoundOutput::new(file, spec.output.clone(), plan.escape_guidance))
        {
            Ok(output) => Arc::new(output),
            Err(error) => {
                self.entries.lock().await.remove(&key);
                return ExecutionOutcome::NotDispatched(error);
            }
        };
        if let Some(warning) = &plan.warning {
            use std::io::Write;
            let written = writeln!(&output_file, "Warning: {warning}");
            if let Err(error) = written {
                self.entries.lock().await.remove(&key);
                return ExecutionOutcome::NotDispatched(io_error(error));
            }
        }
        let launch = ShellLaunch {
            operation_id: operation,
            shell: spec.shell.clone().into(),
            command: spec.command.clone(),
            description: spec.description.clone(),
            cwd: spec.cwd.clone().into(),
            directory: plan.directory,
            output: output_file,
            output_path: spec.output.clone().into(),
            cancel,
        };
        let process = match self
            .tasks
            .start_prepared(
                chat.into(),
                launch,
                plan.environment,
                plan.sandbox,
                Some(&dispatch),
            )
            .await
        {
            Ok(process) => process,
            Err(ShellStartError::NotStarted(error)) => {
                self.entries.lock().await.remove(&key);
                let code = if matches!(error, ToolError::Cancelled) {
                    "Cancelled"
                } else {
                    "NotDispatched"
                };
                return refused(code, error.to_string());
            }
            Err(ShellStartError::OutcomeUnknown(reason)) => {
                return ExecutionOutcome::OutcomeUnknown { operation, reason };
            }
        };
        let handoff = process.handoff().map(ExecutionHandoff::new);
        let status = self
            .register(
                chat,
                instance.clone(),
                process,
                Some(output),
                spec.output.clone(),
            )
            .await;
        ExecutionOutcome::Completed(ExecutionResult {
            result: ToolResult {
                content: json!({"status":"started", "tick_instance_id":instance}).to_string(),
                is_error: false,
                output_ref: None,
            },
            opened: None,
            shell: Some(Box::new(status)),
            command: None,
            handoff,
        })
    }

    async fn register(
        &self,
        chat: ChatId,
        instance: InstanceRef,
        mut process: Box<dyn ShellProcess>,
        output: Option<Arc<BoundOutput>>,
        output_path: String,
    ) -> ShellStatus {
        let key = (chat, instance.clone());
        let handoff = process.handoff();
        let status = ShellStatus {
            instance,
            pid: Some(process.pid()),
            state: CommandState::Running,
            exit_code: None,
            signal: None,
            reason: None,
            output: output_path,
            output_bytes: 0,
            output_delivery: ShellOutputDelivery::CallerFile,
        };
        let (updates, receiver) = watch::channel(status.clone());
        let (stops, mut stop_requests) = mpsc::channel::<StopRequest>(16);
        self.entries.lock().await.insert(
            key,
            Some(Arc::new(ShellEntry {
                output: output.clone(),
                handoff,
                stops,
                status: receiver,
                updates: updates.clone(),
            })),
        );
        tokio::spawn(async move {
            let exit = loop {
                tokio::select! {
                    _ = updates.closed() => return,
                    exit = process.exited() => break exit,
                    stop = stop_requests.recv() => {
                        let Some(stop) = stop else { return; };
                        let dispatch = stop.dispatch.clone();
                        let authorize = move || dispatch.begin().map_err(|error| ShellStopError::NotDispatched(error.to_string()));
                        let result = tokio::select! {
                            biased;
                            _ = stop.dispatch.interrupted() => Err(if stop.dispatch.was_sent() { ShellStopError::OutcomeUnknown("Shell stop cancelled after dispatch".into()) } else { ShellStopError::NotDispatched("Shell stop cancelled before dispatch".into()) }),
                            result = process.stop_checked(&stop.reason, Some(&authorize)) => result,
                        };
                        let _ = stop.reply.send(result);
                    }
                }
            };
            if exit.state == CommandState::Failed
                && let Some(output) = &output
                && output.escape_guidance
                && let Ok(file) = output.file()
            {
                super::sandbox::annotate_escape_guidance(&file);
            }
            updates.send_modify(|status| {
                status.state = exit.state;
                status.exit_code = exit.exit_code;
                status.signal = exit.signal;
                status.reason = exit.reason;
                status.output_bytes = output
                    .as_ref()
                    .and_then(|output| output.length().ok())
                    .unwrap_or(0);
            });
            if let Some(output) = output {
                output.release();
            }
        });
        status
    }

    async fn entry(
        &self,
        chat: ChatId,
        instance: &InstanceRef,
    ) -> Result<Arc<ShellEntry>, ExecutionError> {
        self.entries
            .lock()
            .await
            .get(&(chat, instance.clone()))
            .and_then(Clone::clone)
            .ok_or_else(|| ExecutionError::new("TargetGone", "Shell is not available to this Chat"))
    }

    pub async fn inspect(
        &self,
        chat: ChatId,
        query: ExecutionQuery,
    ) -> Result<ExecutionInspection, ExecutionError> {
        match query {
            ExecutionQuery::ShellStatus(instance) => {
                let entry = self.entry(chat, &instance).await?;
                let mut status = entry.status.borrow().clone();
                if status.state == CommandState::Running
                    && let Some(output) = &entry.output
                {
                    status.output_bytes = output.length()?;
                }
                Ok(ExecutionInspection::Shell(status))
            }
            ExecutionQuery::ShellOutput {
                instance,
                offset,
                limit,
            } => {
                let entry = self.entry(chat, &instance).await?;
                let output = entry.output.as_ref().ok_or_else(|| {
                    ExecutionError::new(
                        "Unsupported",
                        "the adopted Shell output is held by its task registry",
                    )
                })?;
                let file = output.file()?;
                let length = file.metadata().map_err(io_error)?.len();
                let begin = offset.min(length);
                let mut data = vec![0; limit.min(SHELL_OUTPUT_BYTES)];
                let read = file.read_at(&mut data, begin).map_err(io_error)?;
                data.truncate(read);
                Ok(ExecutionInspection::Output(OutputPage {
                    data,
                    next_offset: begin + read as u64,
                    eof: begin + read as u64 >= length,
                }))
            }
            _ => Err(ExecutionError::new("Unsupported", "not a Shell query")),
        }
    }

    pub async fn observe(
        &self,
        chat: ChatId,
        authority: ExecutionAuthority,
        request: ObserveRequest,
    ) -> Result<ExecutionObservation, ExecutionError> {
        if request
            .after
            .as_ref()
            .is_some_and(|cursor| cursor.instance != request.instance || cursor.epoch != self.epoch)
        {
            return Err(ExecutionError::new(
                "HostRestarted",
                "Shell cursor belongs to another execution",
            ));
        }
        let entry = self.entry(chat, &request.instance).await?;
        let mut source = entry.updates.subscribe();
        let baseline = source.borrow_and_update().clone();
        let position = u64::from(baseline.state != CommandState::Running);
        let (sender, updates) = watch::channel(ExecutionUpdate {
            update: None,
            received: position,
            state: ExecutionLinkState::Live,
            terminal: None,
        });
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = authority.revoked() => return,
                    _ = sender.closed() => return,
                    changed = source.changed() => {
                        if changed.is_err() || authority.check(chat).is_err() { return; }
                        let status = source.borrow_and_update().clone();
                        sender.send_replace(ExecutionUpdate { update: Some(TickState { seq: 1, data: json!(status), at: None }), received: 1, state: ExecutionLinkState::Live, terminal: None });
                    }
                }
            }
        });
        Ok(ExecutionObservation {
            baseline: ExecutionRead {
                kind: ExecutionKind::Shell,
                state: TickState {
                    seq: position,
                    data: json!(baseline),
                    at: None,
                },
                truncated: false,
            },
            cursor: ObservationCursor {
                instance: request.instance,
                epoch: self.epoch,
                watch: tickloop_ids::generate(),
                position,
            },
            gap: request
                .after
                .is_some_and(|cursor| cursor.position > position),
            updates,
        })
    }
}

fn io_error(error: std::io::Error) -> ExecutionError {
    ExecutionError::new("Unavailable", error.to_string())
}
fn preparation_error(error: impl Into<String>) -> ExecutionError {
    ExecutionError::new("PreparationFailed", error)
}
fn refused(code: &str, message: impl Into<String>) -> ExecutionOutcome {
    ExecutionOutcome::NotDispatched(ExecutionError::new(code, message))
}

pub(super) fn host(tasks: Arc<ShellTasks>, chat: ChatId) -> Arc<dyn ShellHost> {
    Arc::new(LocalShellHost {
        native: tasks.native_for_chat(chat.into()),
        backend: LocalShells::from_tasks(tasks),
        chat,
    })
}

struct LocalShellHost {
    backend: Arc<LocalShells>,
    native: Arc<dyn ShellHost>,
    chat: ChatId,
}

#[async_trait::async_trait]
impl ShellHost for LocalShellHost {
    async fn prepare_stop(
        &self,
        task: Uuid,
        reason: &str,
    ) -> Result<Option<tickloop_harness::tools::ShellStopPreparation>, ToolError> {
        let authority = ExecutionAuthority::new(self.chat);
        let port = super::shell_port(self.backend.clone(), self.chat, authority.clone(), None);
        super::shell_host::prepare_stop(port, authority, task, reason)
            .await
            .map(Some)
    }
    async fn prepare(
        &self,
        request: tickloop_harness::tools::ShellPreparationRequest,
    ) -> Result<Option<tickloop_harness::tools::ShellPreparation>, ToolError> {
        if !self.native.is_available() {
            return Ok(None);
        }
        let authority = ExecutionAuthority::new(self.chat);
        let port = super::shell_port(self.backend.clone(), self.chat, authority.clone(), None);
        super::shell_host::prepare_shell(port, authority, request, ShellOutputDelivery::CallerFile)
            .await
            .map(Some)
    }
    fn is_available(&self) -> bool {
        self.native.is_available()
    }

    async fn start(&self, launch: ShellLaunch) -> Result<Box<dyn ShellProcess>, ShellStartError> {
        if !self.native.is_available() {
            return self.native.start(launch).await;
        }
        let spec = super::shell_host::shell_spec(&launch);
        let cancel = launch.cancel.clone();
        let authority = ExecutionAuthority::new(self.chat);
        let port = super::shell_port(
            self.backend.clone(),
            self.chat,
            authority.clone(),
            Some(launch),
        );
        let prepared = port
            .prepare(ExecutionIntent::SpawnShell(spec))
            .await
            .map_err(|error| ShellStartError::NotStarted(ToolError::Rejected(error.message)))?;
        let admission = authority
            .admit(&prepared)
            .map_err(|error| ShellStartError::NotStarted(ToolError::Rejected(error.message)))?;
        match port.execute(prepared, admission, cancel).await {
            ExecutionOutcome::Completed(result) => {
                let status = result.shell.ok_or_else(|| {
                    ShellStartError::OutcomeUnknown("Shell start returned no instance".into())
                })?;
                let entry = self
                    .backend
                    .entry(self.chat, &status.instance)
                    .await
                    .map_err(|error| ShellStartError::OutcomeUnknown(error.to_string()))?;
                Ok(Box::new(super::shell_host::PortShellProcess::new(
                    port,
                    authority,
                    *status,
                    None,
                    entry.handoff.clone(),
                )))
            }
            ExecutionOutcome::NotDispatched(error) => Err(super::shell_host::start_refusal(error)),
            ExecutionOutcome::OutcomeUnknown { reason, .. } => {
                Err(ShellStartError::OutcomeUnknown(reason))
            }
        }
    }

    async fn adopt(&self, value: serde_json::Value) -> Result<Box<dyn ShellProcess>, ToolError> {
        let instance: InstanceRef = serde_json::from_value(value["instance"].clone())
            .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
        let process = self.native.adopt(value).await?;
        let status = self
            .backend
            .register(self.chat, instance.clone(), process, None, String::new())
            .await;
        let entry = self
            .backend
            .entry(self.chat, &instance)
            .await
            .map_err(|error| ToolError::Rejected(error.to_string()))?;
        let authority = ExecutionAuthority::new(self.chat);
        let port = super::shell_port(self.backend.clone(), self.chat, authority.clone(), None);
        Ok(Box::new(super::shell_host::PortShellProcess::new(
            port,
            authority,
            status,
            None,
            entry.handoff.clone(),
        )))
    }

    async fn retained_handoff(
        &self,
        operation: Uuid,
    ) -> Result<Option<serde_json::Value>, ToolError> {
        self.native.retained_handoff(operation).await
    }
}

fn create_output(directory: &File, path: &Path) -> Result<File, ExecutionError> {
    use std::os::{
        fd::{AsRawFd, FromRawFd},
        unix::ffi::OsStrExt,
    };
    let name = path
        .file_name()
        .ok_or_else(|| preparation_error("Shell output has no file name"))?;
    let name = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| preparation_error("Shell output name contains NUL"))?;
    // The approved directory descriptor and a single basename prevent path redirection.
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDWR
                | libc::O_APPEND
                | libc::O_CREAT
                | libc::O_EXCL
                | libc::O_NOFOLLOW
                | libc::O_CLOEXEC,
            0o600,
        )
    };
    if descriptor < 0 {
        return Err(io_error(std::io::Error::last_os_error()));
    }
    Ok(unsafe { File::from_raw_fd(descriptor) })
}
