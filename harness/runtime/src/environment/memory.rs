//! An execution environment whose programs and instances live in memory.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
};

use crate::{environment::*, host_tools::HostToolPermission};
use async_trait::async_trait;
use serde_json::{Value, json};
use tick_protocol::{InstanceRef, TickDescriptor, TickState};
use tickloop_harness_protocol::{ChatId, ToolResult};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Clone)]
pub struct MemoryEnvironment {
    state: Arc<Mutex<MemoryState>>,
}

#[derive(Default)]
struct MemoryState {
    epoch: Uuid,
    clock: std::time::Duration,
    shell_programs: HashMap<String, MemoryShellProgram>,
    shells: HashMap<InstanceRef, MemoryShell>,
    programs: HashMap<String, u64>,
    instances: HashMap<InstanceRef, MemoryInstance>,
    lost: HashMap<InstanceRef, MemoryInstance>,
    tick_programs: HashMap<String, MemoryTick>,
    terminal_programs: HashMap<String, MemoryTerminal>,
    receipts: HashMap<(ChatId, Uuid), ExecutionReceipt>,
    lose_reply: bool,
    call_reply_gate: Option<CancellationToken>,
}

#[derive(Clone)]
struct MemoryShellProgram {
    generation: u64,
    duration: std::time::Duration,
    output: Vec<u8>,
    exit_code: i32,
}

struct MemoryShell {
    owner: ChatId,
    status: ShellStatus,
    program: MemoryShellProgram,
    due: std::time::Duration,
    output: Vec<u8>,
    updates: tokio::sync::watch::Sender<ExecutionUpdate>,
}

type TerminalInput = dyn Fn(&mut String, &[u8]) + Send + Sync;
#[derive(Clone)]
struct MemoryTerminal {
    initial: String,
    input: Arc<TerminalInput>,
}

type TickCall = dyn Fn(&mut Value, &str, Value) -> Result<Value, String> + Send + Sync;

#[derive(Clone)]
struct MemoryTick {
    descriptor: TickDescriptor,
    data: Value,
    call: Arc<TickCall>,
}

struct MemoryInstance {
    owner: ChatId,
    open: OpenSpec,
    tick: Option<MemoryTick>,
    terminal: Option<MemoryTerminal>,
    sequence: u64,
    screen: String,
    history_floor: u64,
    updates: tokio::sync::watch::Sender<ExecutionUpdate>,
}

impl MemoryInstance {
    fn read(&self) -> ExecutionRead {
        ExecutionRead {
            kind: if self.tick.is_some() {
                ExecutionKind::Protocol
            } else {
                ExecutionKind::Terminal
            },
            state: TickState {
                seq: self.sequence,
                data: self.tick.as_ref().map_or_else(
                    || json!({"text":self.screen, "exit":null}),
                    |tick| tick.data.clone(),
                ),
                at: None,
            },
            truncated: false,
        }
    }
}

fn finish_due_shells(state: &mut MemoryState) {
    let now = state.clock;
    for shell in state.shells.values_mut() {
        if shell.status.state != tickloop_harness_protocol::CommandState::Running || shell.due > now
        {
            continue;
        }
        shell.output = shell.program.output.clone();
        shell.status.output_bytes = shell.output.len() as u64;
        shell.status.exit_code = Some(shell.program.exit_code);
        shell.status.state = if shell.program.exit_code == 0 {
            tickloop_harness_protocol::CommandState::Completed
        } else {
            tickloop_harness_protocol::CommandState::Failed
        };
        shell.updates.send_replace(ExecutionUpdate {
            update: Some(TickState {
                seq: 1,
                data: json!(shell.status),
                at: None,
            }),
            received: 1,
            state: ExecutionLinkState::Live,
            terminal: None,
        });
    }
}

impl Default for MemoryEnvironment {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryEnvironment {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(MemoryState {
                epoch: tickloop_ids::generate(),
                ..Default::default()
            })),
        }
    }

    pub fn define_shell(
        &self,
        command: &str,
        duration: std::time::Duration,
        output: Vec<u8>,
        exit_code: i32,
    ) {
        let mut state = self.state.lock().unwrap();
        let generation = state
            .shell_programs
            .get(command)
            .map_or(1, |program| program.generation + 1);
        state.shell_programs.insert(
            command.into(),
            MemoryShellProgram {
                generation,
                duration,
                output,
                exit_code,
            },
        );
    }

    pub fn advance(&self, duration: std::time::Duration) {
        let mut state = self.state.lock().unwrap();
        state.clock += duration;
        finish_due_shells(&mut state);
    }

    pub fn install_program(&self, path: &str) {
        let mut state = self.state.lock().unwrap();
        *state.programs.entry(path.into()).or_default() += 1;
    }

    pub fn install_tick_program(
        &self,
        path: &str,
        descriptor: TickDescriptor,
        data: Value,
        call: impl Fn(&mut Value, &str, Value) -> Result<Value, String> + Send + Sync + 'static,
    ) {
        let mut state = self.state.lock().unwrap();
        *state.programs.entry(path.into()).or_default() += 1;
        state.terminal_programs.remove(path);
        state.tick_programs.insert(
            path.into(),
            MemoryTick {
                descriptor,
                data,
                call: Arc::new(call),
            },
        );
    }

    pub fn install_terminal_program(
        &self,
        path: &str,
        initial: &str,
        input: impl Fn(&mut String, &[u8]) + Send + Sync + 'static,
    ) {
        let mut state = self.state.lock().unwrap();
        *state.programs.entry(path.into()).or_default() += 1;
        state.tick_programs.remove(path);
        state.terminal_programs.insert(
            path.into(),
            MemoryTerminal {
                initial: initial.into(),
                input: Arc::new(input),
            },
        );
    }

    pub fn change_revision(&self, instance: &InstanceRef, revision: &str) {
        self.state
            .lock()
            .unwrap()
            .instances
            .get_mut(instance)
            .unwrap()
            .tick
            .as_mut()
            .unwrap()
            .descriptor
            .revision = revision.into();
    }

    pub fn set_terminal_screen(&self, instance: &InstanceRef, text: &str) {
        let mut state = self.state.lock().unwrap();
        let instance = state.instances.get_mut(instance).unwrap();
        assert!(instance.tick.is_none());
        if instance.screen != text {
            instance.screen = text.into();
            instance.sequence += 1;
            instance.updates.send_replace(ExecutionUpdate {
                update: None,
                received: instance.sequence,
                state: ExecutionLinkState::Live,
                terminal: Some(TerminalChange {
                    dirty: true,
                    exited: false,
                }),
            });
        }
    }

    pub fn publish(&self, instance: &InstanceRef, data: Value) {
        let mut state = self.state.lock().unwrap();
        let instance = state.instances.get_mut(instance).unwrap();
        instance.sequence += 1;
        instance.tick.as_mut().unwrap().data = data;
        instance.updates.send_replace(ExecutionUpdate {
            update: Some(instance.read().state),
            received: instance.sequence,
            state: ExecutionLinkState::Live,
            terminal: None,
        });
    }

    pub fn expire_observation_history(&self) {
        for instance in self.state.lock().unwrap().instances.values_mut() {
            instance.history_floor = instance.sequence;
        }
    }

    pub fn restart(&self) {
        let mut state = self.state.lock().unwrap();
        let instances = std::mem::take(&mut state.instances);
        for (instance, target) in instances {
            target
                .updates
                .send_modify(|update| update.state = ExecutionLinkState::Gone);
            state.lost.insert(instance, target);
        }
        state.epoch = tickloop_ids::generate();
        state.receipts.clear();
    }

    pub fn hold_call_replies(&self) -> CancellationToken {
        let release = CancellationToken::new();
        self.state.lock().unwrap().call_reply_gate = Some(release.clone());
        release
    }

    pub fn lose_next_reply(&self) {
        self.state.lock().unwrap().lose_reply = true;
    }
}

impl ExecutionEnvironment for MemoryEnvironment {
    fn bind(
        &self,
        scope: ExecutionScope,
        authority: ExecutionAuthority,
    ) -> Result<Arc<dyn ExecutionPort>, ExecutionError> {
        authority.check(scope.chat)?;
        Ok(Arc::new(MemoryPort {
            identity: tickloop_ids::generate(),
            scope,
            authority,
            state: self.state.clone(),
            plans: Mutex::new(HashMap::new()),
        }))
    }
}

struct MemoryPlan {
    lifetime: Weak<()>,
    program_generation: u64,
    open: Option<OpenSpec>,
    shell_program: Option<MemoryShellProgram>,
}

struct MemoryPort {
    identity: Uuid,
    scope: ExecutionScope,
    authority: ExecutionAuthority,
    state: Arc<Mutex<MemoryState>>,
    plans: Mutex<HashMap<PreparationId, MemoryPlan>>,
}

#[async_trait]
impl ExecutionPort for MemoryPort {
    async fn observe(
        &self,
        request: ObserveRequest,
    ) -> Result<ExecutionObservation, ExecutionError> {
        self.authority.check(self.scope.chat)?;
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
        let state = self.state.lock().unwrap();
        if request
            .after
            .as_ref()
            .is_some_and(|cursor| cursor.epoch != state.epoch)
        {
            return Err(ExecutionError::new(
                "HostRestarted",
                "the Tickhost restarted",
            ));
        }
        let (baseline, position, history_floor, mut source) = if let Some(shell) = state
            .shells
            .get(&request.instance)
            .filter(|shell| shell.owner == self.scope.chat)
        {
            let position = shell.updates.borrow().received;
            (
                ExecutionRead {
                    kind: ExecutionKind::Shell,
                    state: TickState {
                        seq: position,
                        data: json!(shell.status),
                        at: None,
                    },
                    truncated: false,
                },
                position,
                0,
                shell.updates.subscribe(),
            )
        } else {
            let instance = state
                .instances
                .get(&request.instance)
                .filter(|instance| instance.owner == self.scope.chat)
                .ok_or_else(|| {
                    ExecutionError::new("TargetGone", "Tick is no longer available to this Chat")
                })?;
            (
                instance.read(),
                instance.sequence,
                instance.history_floor,
                instance.updates.subscribe(),
            )
        };
        let cursor = ObservationCursor {
            instance: request.instance,
            epoch: state.epoch,
            watch: tickloop_ids::generate(),
            position,
        };
        let gap = request
            .after
            .is_some_and(|after| after.position < history_floor || after.position > position);
        let (sender, updates) = tokio::sync::watch::channel(source.borrow().clone());
        let authority = self.authority.clone();
        let chat = self.scope.chat;
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = authority.revoked() => return,
                    _ = sender.closed() => return,
                    changed = source.changed() => {
                        if changed.is_err() || authority.check(chat).is_err() { return; }
                        sender.send_replace(source.borrow_and_update().clone());
                    }
                }
            }
        });
        Ok(ExecutionObservation {
            baseline,
            cursor,
            gap,
            updates,
        })
    }

    async fn inspect(&self, query: ExecutionQuery) -> Result<ExecutionInspection, ExecutionError> {
        self.authority.check(self.scope.chat)?;
        let describes = matches!(&query, ExecutionQuery::Describe(_));
        match query {
            ExecutionQuery::Capabilities => {
                Ok(ExecutionInspection::Capabilities(ExecutionCapabilities {
                    ticks: true,
                    shells: true,
                    shell_output: ShellOutputDelivery::Environment,
                }))
            }
            ExecutionQuery::Instances => Ok(ExecutionInspection::Instances(
                self.state
                    .lock()
                    .unwrap()
                    .instances
                    .iter()
                    .filter(|(_, owner)| owner.owner == self.scope.chat)
                    .map(|(instance, _)| instance.clone())
                    .collect(),
            )),
            ExecutionQuery::Receipt(operation) => Ok(ExecutionInspection::Receipt(
                self.state
                    .lock()
                    .unwrap()
                    .receipts
                    .get(&(self.scope.chat, operation))
                    .cloned()
                    .unwrap_or(ExecutionReceipt::Unknown),
            )),
            ExecutionQuery::ShellStatus(instance) => {
                let state = self.state.lock().unwrap();
                let shell = state
                    .shells
                    .get(&instance)
                    .filter(|shell| shell.owner == self.scope.chat)
                    .ok_or_else(|| {
                        ExecutionError::new("Unauthorized", "Shell does not belong to this Chat")
                    })?;
                Ok(ExecutionInspection::Shell(shell.status.clone()))
            }
            ExecutionQuery::ShellOutput {
                instance,
                offset,
                limit,
            } => {
                let state = self.state.lock().unwrap();
                let shell = state
                    .shells
                    .get(&instance)
                    .filter(|shell| shell.owner == self.scope.chat)
                    .ok_or_else(|| {
                        ExecutionError::new("Unauthorized", "Shell does not belong to this Chat")
                    })?;
                let begin = usize::try_from(offset)
                    .unwrap_or(usize::MAX)
                    .min(shell.output.len());
                let end = begin
                    .saturating_add(limit.min(SHELL_OUTPUT_BYTES))
                    .min(shell.output.len());
                Ok(ExecutionInspection::Output(OutputPage {
                    data: shell.output[begin..end].to_vec(),
                    next_offset: end as u64,
                    eof: end == shell.output.len(),
                }))
            }
            ExecutionQuery::Kind(instance) | ExecutionQuery::Describe(instance) => {
                let state = self.state.lock().unwrap();
                let instance = state
                    .instances
                    .get(&instance)
                    .filter(|instance| instance.owner == self.scope.chat)
                    .ok_or_else(|| {
                        ExecutionError::new(
                            "TargetGone",
                            "Tick is no longer available to this Chat",
                        )
                    })?;
                if describes {
                    Ok(ExecutionInspection::Description(instance.tick.as_ref().map_or_else(|| json!({"title":instance.open.command.join(" "), "command":instance.open.command, "cwd":instance.open.cwd}), |tick| json!(tick.descriptor))))
                } else {
                    Ok(ExecutionInspection::Kind(instance.read().kind))
                }
            }
            ExecutionQuery::Read(instance)
            | ExecutionQuery::Sample(ObservationCursor { instance, .. }) => {
                let state = self.state.lock().unwrap();
                let instance = state
                    .instances
                    .get(&instance)
                    .filter(|instance| instance.owner == self.scope.chat)
                    .ok_or_else(|| {
                        ExecutionError::new("Unauthorized", "Tick does not belong to this Chat")
                    })?;
                Ok(ExecutionInspection::Read(instance.read()))
            }
            ExecutionQuery::Preparation(operation) => {
                if !self.plans.lock().unwrap().contains_key(&operation) {
                    return Err(ExecutionError::new(
                        "UnknownPreparation",
                        "preparation was consumed or expired",
                    ));
                }
                Ok(ExecutionInspection::Validated)
            }
        }
    }

    async fn prepare(&self, intent: ExecutionIntent) -> Result<PreparedExecution, ExecutionError> {
        self.authority.check(self.scope.chat)?;
        let state = self.state.lock().unwrap();
        let (generation, normalized_input, summary, permission, catalog) = match &intent {
            ExecutionIntent::StopShell { instance, .. } => {
                if state
                    .shells
                    .get(instance)
                    .is_none_or(|target| target.owner != self.scope.chat)
                {
                    return Err(ExecutionError::new(
                        "Unauthorized",
                        "Shell does not belong to this Chat",
                    ));
                }
                (
                    0,
                    json!({"task_id":instance}),
                    format!("Stop Shell {}", instance.public()),
                    HostToolPermission::Reviewable,
                    None,
                )
            }
            ExecutionIntent::SpawnShell(shell) => {
                if !state.shell_programs.contains_key(&shell.command) {
                    return Err(ExecutionError::new(
                        "ProgramNotFound",
                        "Shell command has no definition in this environment",
                    ));
                }
                (
                    0,
                    json!({"command":shell.command, "cwd":shell.cwd}),
                    format!("Run {}", shell.command),
                    HostToolPermission::Reviewable,
                    None,
                )
            }
            ExecutionIntent::Open(open) => {
                let program = open
                    .command
                    .first()
                    .ok_or_else(|| ExecutionError::new("InvalidInput", "command is empty"))?;
                let generation = *state
                    .programs
                    .get(program)
                    .ok_or_else(|| ExecutionError::new("ProgramNotFound", program))?;
                (
                    generation,
                    json!({"action":"open", "command":open.command, "cwd":open.cwd, "protocol":open.protocol}),
                    format!("Open {}", open.command.join(" ")),
                    HostToolPermission::Reviewable,
                    None,
                )
            }
            ExecutionIntent::Reopen { instance } => {
                let target = state
                    .lost
                    .get(instance)
                    .filter(|target| target.owner == self.scope.chat)
                    .ok_or_else(|| {
                        ExecutionError::new("TargetGone", "Tick was not lost in this Chat")
                    })?;
                let generation = *state.programs.get(&target.open.command[0]).ok_or_else(|| {
                    ExecutionError::new(
                        "ProgramNotFound",
                        "the original program is no longer installed",
                    )
                })?;
                (
                    generation,
                    json!({"action":"reopen", "tick_instance_id":instance}),
                    format!("Reopen {instance} as a new hidden instance"),
                    HostToolPermission::Reviewable,
                    None,
                )
            }
            ExecutionIntent::SendInput { instance, data } => {
                let target = state
                    .instances
                    .get(instance)
                    .filter(|target| target.owner == self.scope.chat)
                    .ok_or_else(|| {
                        ExecutionError::new("Unauthorized", "Tick does not belong to this Chat")
                    })?;
                if target.tick.is_some() || data.is_empty() || data.len() > TERMINAL_INPUT_BYTES {
                    return Err(ExecutionError::new("Unsupported", "invalid terminal input"));
                }
                (
                    0,
                    json!({"action":"send_input", "tick_instance_id":instance, "data":data}),
                    format!(
                        "Type {} bytes into hosted terminal Tick {instance}",
                        data.len()
                    ),
                    HostToolPermission::Reviewable,
                    None,
                )
            }
            ExecutionIntent::Close { instance } => {
                if state
                    .instances
                    .get(instance)
                    .is_none_or(|target| target.owner != self.scope.chat)
                {
                    return Err(ExecutionError::new(
                        "Unauthorized",
                        "Tick does not belong to this Chat",
                    ));
                }
                (
                    0,
                    json!({"action":"close", "tick_instance_id":instance}),
                    format!("Close hosted Tick {}", instance.public()),
                    HostToolPermission::Reviewable,
                    None,
                )
            }
            ExecutionIntent::Call {
                instance,
                tool,
                args,
                revision,
            } => {
                let tick = state
                    .instances
                    .get(instance)
                    .filter(|instance| instance.owner == self.scope.chat)
                    .and_then(|instance| instance.tick.as_ref())
                    .ok_or_else(|| {
                        ExecutionError::new("Unauthorized", "Tick does not belong to this Chat")
                    })?;
                if &tick.descriptor.revision != revision {
                    return Err(ExecutionError::new(
                        "RevisionMismatch",
                        "the Tick catalog changed; prepare and approve again",
                    ));
                }
                let declaration = tick
                    .descriptor
                    .tools
                    .iter()
                    .find(|declared| &declared.name == tool)
                    .ok_or_else(|| ExecutionError::new("UnknownTool", tool))?;
                (
                    0,
                    json!({"tick_instance_id":instance, "tool":tool, "args":args}),
                    format!("Call {tool} on {instance}"),
                    if declaration.mutates {
                        HostToolPermission::Reviewable
                    } else {
                        HostToolPermission::ReadOnly
                    },
                    Some(json!(tick.descriptor)),
                )
            }
        };
        let open = match &intent {
            ExecutionIntent::Open(open) => Some(open.clone()),
            ExecutionIntent::Reopen { instance } => {
                let mut open = state.lost[instance].open.clone();
                open.view = None;
                Some(open)
            }
            _ => None,
        };
        let shell_program = match &intent {
            ExecutionIntent::SpawnShell(shell) => state.shell_programs.get(&shell.command).cloned(),
            _ => None,
        };
        let operation = match &intent {
            ExecutionIntent::SpawnShell(shell) => shell.operation,
            _ => tickloop_ids::generate(),
        };
        let prepared = PreparedExecution::for_operation(
            operation,
            self.identity,
            &self.authority,
            ExecutionFacts {
                intent: intent.clone(),
                summary,
                normalized_input: normalized_input.clone(),
                permission,
                approval: tickloop_harness_protocol::PreparedApprovalFacts {
                    normalized_input,
                    external_declaration: catalog,
                    ..Default::default()
                },
            },
        );
        let mut plans = self.plans.lock().unwrap();
        plans.retain(|_, plan| plan.lifetime.strong_count() > 0);
        plans.insert(
            prepared.preparation(),
            MemoryPlan {
                lifetime: prepared.lifetime(),
                program_generation: generation,
                open,
                shell_program,
            },
        );
        Ok(prepared)
    }

    async fn execute(
        &self,
        prepared: PreparedExecution,
        admission: ExecutionAdmission,
        cancel: CancellationToken,
    ) -> ExecutionOutcome {
        if let Err(error) = prepared.validate(self.identity, &self.authority, &admission) {
            return ExecutionOutcome::NotDispatched(error);
        }
        if cancel.is_cancelled() {
            return ExecutionOutcome::NotDispatched(ExecutionError::new(
                "Cancelled",
                "execution cancelled before dispatch",
            ));
        }
        let Some(plan) = self.plans.lock().unwrap().remove(&prepared.preparation()) else {
            return ExecutionOutcome::NotDispatched(ExecutionError::new(
                "UnknownPreparation",
                "preparation was consumed or expired",
            ));
        };
        let (outcome, gate) = {
            let mut state = self.state.lock().unwrap();
            if let Err(error) = self.authority.check(self.scope.chat) {
                return ExecutionOutcome::NotDispatched(error);
            }
            if cancel.is_cancelled() {
                return ExecutionOutcome::NotDispatched(ExecutionError::new(
                    "Cancelled",
                    "execution cancelled before dispatch",
                ));
            }

            let result = match &prepared.facts().intent {
                ExecutionIntent::StopShell { instance, reason } => {
                    let Some(shell) = state
                        .shells
                        .get_mut(instance)
                        .filter(|shell| shell.owner == self.scope.chat)
                    else {
                        return ExecutionOutcome::NotDispatched(ExecutionError::new(
                            "Unauthorized",
                            "Shell does not belong to this Chat",
                        ));
                    };
                    if shell.status.state == tickloop_harness_protocol::CommandState::Running {
                        shell.status.state = tickloop_harness_protocol::CommandState::Failed;
                        shell.status.reason = Some(reason.clone());
                        shell.status.signal = Some(15);
                        shell.updates.send_replace(ExecutionUpdate {
                            update: Some(TickState {
                                seq: 1,
                                data: json!(shell.status),
                                at: None,
                            }),
                            received: 1,
                            state: ExecutionLinkState::Live,
                            terminal: None,
                        });
                    }
                    ExecutionResult {
                        result: ToolResult {
                            content: json!({"status":"stopped", "tick_instance_id":instance})
                                .to_string(),
                            is_error: false,
                            output_ref: None,
                        },
                        opened: None,
                        shell: None,
                        handoff: None,
                    }
                }
                ExecutionIntent::SpawnShell(spec) => {
                    let instance = InstanceRef::shell(spec.operation);
                    if state.shells.contains_key(&instance) {
                        return ExecutionOutcome::NotDispatched(ExecutionError::new(
                            "DuplicateOperation",
                            "Shell operation was already dispatched",
                        ));
                    }
                    let program = plan.shell_program.expect("prepared Shell definition");
                    if state
                        .shell_programs
                        .get(&spec.command)
                        .is_none_or(|current| current.generation != program.generation)
                    {
                        return ExecutionOutcome::NotDispatched(ExecutionError::new(
                            "TargetChanged",
                            "the Shell program changed before dispatch; prepare and approve again",
                        ));
                    }
                    let status = ShellStatus {
                        instance: instance.clone(),
                        pid: None,
                        state: tickloop_harness_protocol::CommandState::Running,
                        exit_code: None,
                        signal: None,
                        reason: None,
                        output: spec.output.clone(),
                        output_bytes: 0,
                        output_delivery: ShellOutputDelivery::Environment,
                    };
                    let due = state.clock + program.duration;
                    state.shells.insert(
                        instance,
                        MemoryShell {
                            owner: self.scope.chat,
                            status: status.clone(),
                            program,
                            due,
                            output: Vec::new(),
                            updates: tokio::sync::watch::channel(ExecutionUpdate {
                                update: None,
                                received: 0,
                                state: ExecutionLinkState::Live,
                                terminal: None,
                            })
                            .0,
                        },
                    );
                    finish_due_shells(&mut state);
                    ExecutionResult {
                        result: ToolResult {
                            content:
                                json!({"status":"started", "tick_instance_id":status.instance})
                                    .to_string(),
                            is_error: false,
                            output_ref: None,
                        },
                        opened: None,
                        shell: Some(Box::new(status)),
                        handoff: None,
                    }
                }
                ExecutionIntent::Open(_) | ExecutionIntent::Reopen { .. } => {
                    let open = plan.open.as_ref().expect("prepared open");
                    if state.programs.get(&open.command[0]) != Some(&plan.program_generation) {
                        return ExecutionOutcome::NotDispatched(ExecutionError::new(
                            "TargetChanged",
                            "the executable changed before dispatch; prepare and approve again",
                        ));
                    }
                    let instance = InstanceRef::hosted(tickloop_ids::Id::new());
                    let tick = state.tick_programs.get(&open.command[0]).cloned();
                    let terminal = state.terminal_programs.get(&open.command[0]).cloned();
                    let screen = terminal
                        .as_ref()
                        .map(|terminal| terminal.initial.clone())
                        .unwrap_or_default();
                    state.instances.insert(
                        instance.clone(),
                        MemoryInstance {
                            owner: self.scope.chat,
                            open: open.clone(),
                            tick,
                            terminal,
                            sequence: 0,
                            screen,
                            history_floor: 0,
                            updates: tokio::sync::watch::channel(ExecutionUpdate {
                                update: None,
                                received: 0,
                                state: ExecutionLinkState::Live,
                                terminal: None,
                            })
                            .0,
                        },
                    );
                    let mut result = json!({"status":"opened", "host":"tickhost", "visibility":"hidden", "tick_instance_id":instance});
                    if open.view.is_some() {
                        result["view"] = json!({"status":"unavailable"});
                    }
                    if let ExecutionIntent::Reopen { instance } = &prepared.facts().intent {
                        state.lost.remove(instance);
                        result["replaces"] = json!(instance);
                    }
                    ExecutionResult {
                        result: ToolResult {
                            content: result.to_string(),
                            is_error: false,
                            output_ref: None,
                        },
                        opened: Some(OpenedExecution {
                            instance,
                            title: Some(open.command.join(" ")),
                            is_terminal: !open.protocol,
                        }),
                        shell: None,
                        handoff: None,
                    }
                }
                ExecutionIntent::SendInput { instance, data } => {
                    let Some(target) = state
                        .instances
                        .get_mut(instance)
                        .filter(|target| target.owner == self.scope.chat && target.tick.is_none())
                    else {
                        return ExecutionOutcome::NotDispatched(ExecutionError::new(
                            "Unauthorized",
                            "terminal Tick does not belong to this Chat",
                        ));
                    };
                    if let Some(terminal) = &target.terminal {
                        (terminal.input)(&mut target.screen, data);
                    } else {
                        target.screen.push_str(&String::from_utf8_lossy(data));
                    }
                    target.sequence += 1;
                    target.updates.send_replace(ExecutionUpdate {
                        update: None,
                        received: target.sequence,
                        state: ExecutionLinkState::Live,
                        terminal: Some(TerminalChange {
                            dirty: true,
                            exited: false,
                        }),
                    });
                    ExecutionResult { result: ToolResult { content: json!({"status":"completed", "tick_instance_id":instance, "written":data.len()}).to_string(), is_error: false, output_ref: None }, opened: None, shell: None, handoff: None }
                }
                ExecutionIntent::Close { instance } => {
                    if state
                        .instances
                        .get(instance)
                        .is_none_or(|target| target.owner != self.scope.chat)
                    {
                        return ExecutionOutcome::NotDispatched(ExecutionError::new(
                            "Unauthorized",
                            "Tick does not belong to this Chat",
                        ));
                    }
                    let removed = state.instances.remove(instance).unwrap();
                    removed
                        .updates
                        .send_modify(|update| update.state = ExecutionLinkState::Gone);
                    ExecutionResult { result: ToolResult { content: json!({"status":"closed", "host":"tickhost", "tick_instance_id":instance}).to_string(), is_error: false, output_ref: None }, opened: None, shell: None, handoff: None }
                }
                ExecutionIntent::Call {
                    instance,
                    tool,
                    args,
                    revision,
                } => {
                    let Some(target) = state
                        .instances
                        .get_mut(instance)
                        .filter(|instance| instance.owner == self.scope.chat)
                    else {
                        return ExecutionOutcome::NotDispatched(ExecutionError::new(
                            "Unauthorized",
                            "Tick does not belong to this Chat",
                        ));
                    };
                    let Some(tick) = &mut target.tick else {
                        return ExecutionOutcome::NotDispatched(ExecutionError::new(
                            "Unsupported",
                            "terminal Ticks declare no tools",
                        ));
                    };
                    if &tick.descriptor.revision != revision {
                        return ExecutionOutcome::NotDispatched(ExecutionError::new(
                            "RevisionMismatch",
                            "the Tick catalog changed; prepare and approve again",
                        ));
                    }
                    let (result, is_error) = match (tick.call)(&mut tick.data, tool, args.clone()) {
                        Ok(result) => {
                            target.sequence += 1;
                            (
                                json!({"status":"completed", "tick_instance_id":instance, "result":result}),
                                false,
                            )
                        }
                        Err(error) => (
                            json!({"status":"failed", "tick_instance_id":instance, "error":error}),
                            true,
                        ),
                    };
                    target.updates.send_replace(ExecutionUpdate {
                        update: Some(target.read().state),
                        received: target.sequence,
                        state: ExecutionLinkState::Live,
                        terminal: None,
                    });
                    ExecutionResult {
                        result: ToolResult {
                            content: result.to_string(),
                            is_error,
                            output_ref: None,
                        },
                        opened: None,
                        shell: None,
                        handoff: None,
                    }
                }
            };
            state.receipts.insert(
                (self.scope.chat, prepared.operation()),
                ExecutionReceipt::Completed(result.clone()),
            );
            let gate = if matches!(prepared.facts().intent, ExecutionIntent::Call { .. }) {
                state.call_reply_gate.clone()
            } else {
                None
            };
            let outcome = if std::mem::take(&mut state.lose_reply) {
                ExecutionOutcome::OutcomeUnknown {
                    operation: prepared.operation(),
                    reason: "the operation ran but its response was lost".into(),
                }
            } else {
                ExecutionOutcome::Completed(result)
            };
            (outcome, gate)
        };
        if let Some(release) = gate {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => ExecutionOutcome::OutcomeUnknown { operation: prepared.operation(), reason: "operation cancelled after dispatch".into() },
                _ = self.authority.revoked() => ExecutionOutcome::OutcomeUnknown { operation: prepared.operation(), reason: "authority revoked after dispatch".into() },
                _ = release.cancelled() => outcome,
            }
        } else {
            outcome
        }
    }
}

/// A stateful counter program for shared execution contracts.
pub fn counter_environment() -> MemoryEnvironment {
    let environment = MemoryEnvironment::new();
    environment.install_tick_program("/counter", serde_json::from_value(json!({
        "protocol":3,"revision":"1","kind":"example.counter","title":"Counter","description":"Counter",
        "tools":[{"name":"increment","description":"Increment","input_schema":{"type":"object","properties":{"amount":{"type":"integer"}}},"mutates":true}],
    })).unwrap(), json!({"value":0}), |data, _, args| {
        data["value"] = json!(data["value"].as_i64().unwrap() + args["amount"].as_i64().unwrap_or(1));
        Ok(data.clone())
    });
    environment
}
