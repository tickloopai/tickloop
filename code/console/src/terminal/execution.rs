use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
};
#[cfg(test)]
use tickloop_code_local_environment::AgentLocalExt;
#[cfg(test)]
use tickloop_code_settings::local_agent::LocalAgent;
use tickloop_code_settings::skills::SkillDiscovery;

use tickhost_connect::herdr::{PaneIdentityError, confirm_pane};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::new_agent_draft::{
    AgentConfiguration, ConfigTarget, FirstCreate, NewAgentDraft, ReplayStep, unavailable_change,
};
use super::permissions_surface::{
    DraftPolicyRevision, PermissionsContent, PermissionsPage, PermissionsSubject,
};
use tickloop_code_access::{
    AccessError, AccessErrorCode, AfterTurnAction, AgentAccess, AgentCommand, AgentDetailQuery,
    AgentFilter, AgentSummary, CommandEnvelope, CommandReceipt, CommandResult, ConfigurationChange,
    DisplayUpdate, ExecutionAvailability, ExecutionRef, HistoryAction, HostLocation,
    InteractionSource, LaunchSpec, LiveChatSnapshot, LiveChatStatus, LoopAction, MAX_PAGE_LIMIT,
    Observation, ObservationChange, ObservationRequest, ObservationUpdate, ObservationValue, Page,
    PendingRequestKind, REPLAY_CAPACITY, ReceiptState, RequestRef, TaskAction, TaskOrigin, TaskRef,
    TranscriptRef, UserInput, ViewRef,
};
use tickloop_code_client::{LocalAgentClient, RecoveringAgentClient};
#[cfg(test)]
use tickloop_code_daemon::EmbeddedAgentAccess;
use tickloop_code_settings::new_chat::new_chat_settings;
use tickloop_harness::model::judgment::jev_key_status_in;
use tickloop_harness::protocol::{
    ApprovalHistoryView, BranchOutcome, BranchRequest, ChatSnapshot, ChatStatus, CommandSnapshot,
    CompactionOutcome, ContextBreakdown, DirectedSummaryOutcome, DirectedSummaryRequest, Event,
    ExecutionMode, HostedMember, InputContext, LoopState, LoopTriggerNotice, ModelChange,
    ModelConfig, MonitorMembership, OperationRecord, PermissionChange, PermissionRule,
    PermissionScope, PermissionsRevision, PermissionsView, ProviderCredentialScope, QuestionAnswer,
    QueuedInput, ReasoningEffort, Receipt, RewindMode, RewindOutcome, SavedChat, ShellTask,
    ShellTaskOrigin, ShellTaskOutputChunk, TakenBackPrompt, TaskNotification,
    TickSubscriptionChange, TickSubscriptionStatus, ToolResult, UserShellReceipt, UserShellRecord,
    recover_shell_update_time,
};
use tickloop_harness::tools::permissions::{
    canonical_directory, launch_rule_views, rule_views, validate_rule,
};

/// Why a New Agent Permissions change was not applied. Nothing was written or staged.
pub(super) enum DraftPermissionsError {
    /// Saved policy changed since the view was read; carries the current view.
    Conflict(Box<PermissionsPage>),
    Rejected(AccessError),
}

/// A New Agent Permissions change that was applied, with the view after it.
pub(super) struct DraftPermissionsChange {
    pub(super) page: PermissionsPage,
    /// Kept for the first input (mode, session rule, directory) rather than saved now.
    pub(super) kept: bool,
}

#[derive(Clone)]
pub(super) struct ConsoleExecution {
    inner: Arc<Inner>,
}

pub(super) enum CreationAcceptance {
    /// This View follows the created Agent; carries its id and the receipt of its first
    /// submitted run.
    Attached(Uuid, Option<Receipt>),
    /// The daemon created the Agent, but this View could not follow it.
    AttachUnavailable(AccessError),
    Rejected(AccessError),
}

/// The first input's Create, from sending it until its Agent is attached and configured.
#[allow(clippy::large_enum_variant)]
enum FirstCreateState {
    /// The reply was lost; the receipt decides whether an Agent exists.
    Unconfirmed {
        epoch: Uuid,
        command_id: Uuid,
        draft: NewAgentDraft,
    },
    /// The Create failed after the daemon registered `execution` for it. That Agent may be
    /// incomplete, so nothing is sent for it and no other Create follows until the user
    /// acknowledges `command_id`.
    RegisteredFailed {
        command_id: Uuid,
        execution: ExecutionRef,
        error: String,
        draft: NewAgentDraft,
    },
    /// The Agent exists; attaching or some staged choices remain.
    Created {
        chat: Uuid,
        execution: ExecutionRef,
        replay: FirstCreate,
        blocked: Option<BlockedChoice>,
    },
}

/// A replayed New Agent choice the first input waits on.
enum BlockedChoice {
    /// Its reply was lost; only its receipt decides, and it is never resent.
    Unknown { epoch: Uuid, command_id: Uuid },
    /// The daemon refused it; the remaining choices wait until the user cancels them.
    Refused { command_id: Uuid, error: String },
}

/// What a command's reply or receipt proves about it.
enum CommandOutcome {
    Completed(CommandResult),
    /// The daemon refused it. For a Create, `registered` names the execution the daemon had
    /// already registered before the failure.
    Refused {
        error: AccessError,
        registered: Option<ExecutionRef>,
    },
    /// It never reached the daemon, so nothing was applied.
    Undelivered(AccessError),
    /// Accepted, running or with a lost result: only its receipt may decide later.
    Unknown,
}

impl CommandOutcome {
    fn of(receipt: CommandReceipt) -> Self {
        match receipt.state {
            ReceiptState::Completed | ReceiptState::Failed => {
                let registered = receipt.target;
                match receipt.into_result() {
                    Ok(result) => Self::Completed(result),
                    Err(error) => Self::Refused { error, registered },
                }
            }
            ReceiptState::Accepted | ReceiptState::Running | ReceiptState::OutcomeUnknown => {
                Self::Unknown
            }
        }
    }
}

/// What the receipt of an input whose acceptance was unknown proves.
pub(super) enum InputResolution {
    /// The daemon accepted the original input; this is its result, so nothing is resent.
    Accepted(Box<CommandResult>),
    Refused,
    Unknown,
    /// No unknown input with that id remains for the Agent.
    Released,
}

/// What saving a persistent rule change for the New Agent did.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum RuleSave {
    Saved,
    /// Not a saved-rule change, or this View has no settings to save into.
    NotSaved,
    /// The saved rules changed after the revision the change was made against; nothing was
    /// written.
    Conflict,
}

/// What acknowledging a New Agent command left behind.
pub(super) enum CreateAcknowledgement {
    /// The unknown or failed Create is no longer tracked; its draft choices return to the New
    /// Agent.
    DraftRestored,
    /// The Agent keeps the choices already applied; the rest are dropped, and input reaches it.
    ChoicesCancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ConfigurationOutcome {
    Applied,
    /// The visible Agent is working; the model applies after its current turn.
    Pending,
    /// Kept in this View until the first input creates the Agent.
    Staged,
}

impl std::fmt::Display for ConfigurationOutcome {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Applied => "Applied",
            Self::Pending => "Pending",
            Self::Staged => "Staged for the new Agent",
        })
    }
}

impl ConfigurationOutcome {
    /// Tells the user when a New Agent choice takes effect; empty for a visible Agent.
    pub(super) fn staged_note(self) -> &'static str {
        if self == Self::Staged {
            " Applies when you send the first input."
        } else {
            ""
        }
    }
}

pub(super) enum DraftCreationError {
    /// No Agent was created; the draft and its staged settings are unchanged.
    NotCreated(AccessError),
    /// The Create failed after the daemon registered an Agent for it; nothing more is sent for it
    /// until the user acknowledges `command_id`.
    RegisteredFailed {
        command_id: Uuid,
        agent_id: Uuid,
        error: String,
    },
    /// The Agent exists but a staged choice could not be sent yet; the remaining choices and the
    /// input wait for the next attempt.
    Unapplied { chat: Uuid, error: AccessError },
    /// A staged choice's result is unknown; its receipt decides before anything else is sent.
    ChoiceUnknown { chat: Uuid, command_id: Uuid },
    /// The daemon refused a staged choice; no input reaches the Agent until the user cancels the
    /// remaining choices.
    ChoiceRefused {
        chat: Uuid,
        command_id: Uuid,
        error: String,
    },
    /// The daemon created the Agent, but this View cannot attach to it yet; staged settings and the
    /// input wait for the next attempt, which never sends another Create.
    AttachUnavailable { chat: Uuid, error: AccessError },
    /// The first Create's acceptance is unknown; its receipt decides before anything is resent.
    Unconfirmed(Uuid),
}

#[derive(Clone, Debug)]
pub(super) struct ExecutionObservation {
    pub(super) execution: ExecutionRef,
    pub(super) snapshot: ChatSnapshot,
    pub(super) events: Vec<Event>,
    pub(super) reset_required: bool,
}

pub(super) struct EventRecovery {
    covered: tokio::sync::OwnedMutexGuard<HashMap<Uuid, (ExecutionRef, u64)>>,
    inner: Arc<Inner>,
}

impl EventRecovery {
    pub(super) fn cover(&mut self, execution: ExecutionRef, snapshot: &ChatSnapshot) {
        let covered = self
            .covered
            .entry(snapshot.id.into())
            .or_insert((execution, 0));
        if covered.0 != execution {
            *covered = (execution, 0);
        }
        covered.1 = covered.1.max(snapshot.last_sequence);
    }

    fn covers(&self, event: &Event, references: &HashMap<Uuid, ExecutionRef>) -> bool {
        self.covered
            .get(&event.chat_id.uuid())
            .is_some_and(|(execution, sequence)| {
                references.get(&event.chat_id.uuid()) == Some(execution)
                    && event.sequence <= *sequence
            })
    }

    pub(super) fn retain_uncovered(
        &self,
        receiver: &mut broadcast::Receiver<Event>,
        deferred: &mut VecDeque<Event>,
    ) {
        let references = self.inner.references.read().unwrap();
        deferred.retain(|event| !self.covers(event, &references));
        loop {
            match receiver.try_recv() {
                Ok(event) if !self.covers(&event, &references) => deferred.push_back(event),
                Ok(_) | Err(broadcast::error::TryRecvError::Lagged(_)) => {}
                Err(
                    broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed,
                ) => break,
            }
        }
    }
}

struct Inner {
    access: Arc<dyn AgentAccess>,
    upgrades_supported: bool,
    epoch: Uuid,
    launch: LaunchSpec,
    recovery: Option<RecoveringAgentClient>,
    references: RwLock<HashMap<Uuid, ExecutionRef>>,
    /// The latest full snapshot of each Chat this View read, for history and for an unreachable
    /// daemon.
    snapshots: RwLock<HashMap<Uuid, LiveChatSnapshot>>,
    /// The latest status of each Chat, from a status read or a full snapshot, whichever is newer.
    statuses: RwLock<HashMap<Uuid, LiveChatStatus>>,
    watching: Mutex<HashSet<ExecutionRef>>,
    events: broadcast::Sender<Event>,
    /// Chats whose daemon feed skipped events: a Reset or a Resync stands where they would be.
    feed_gaps: broadcast::Sender<Uuid>,
    event_delivery: Arc<tokio::sync::Mutex<HashMap<Uuid, (ExecutionRef, u64)>>>,
    shells: broadcast::Sender<UserShellRecord>,
    tasks: broadcast::Sender<TaskNotification>,
    /// Signals that a pushed snapshot reached the cache, so recovery tests can wait for it.
    #[cfg(test)]
    facts: broadcast::Sender<()>,
    cancel: CancellationToken,
    display: Mutex<DisplayState>,
    disconnected: Mutex<Option<String>>,
    draft: Mutex<NewAgentDraft>,
    launch_rules: Mutex<Vec<PermissionRule>>,
    first_create: Mutex<Option<FirstCreateState>>,
    /// The View's settings, sharing its transient Providers; New Agent defaults save through it.
    user_settings: Mutex<Option<tickloop_code_settings::UserSettings>>,
    default_model: Mutex<Option<ModelConfig>>,
    new_agent_directory: Mutex<Option<PathBuf>>,
    /// Inputs whose acceptance is unknown, by execution; no further input goes there until the
    /// receipt resolves.
    unconfirmed_inputs: Mutex<HashMap<ExecutionRef, (Uuid, Uuid)>>,
    /// The in-process access a test View was built on, so the test can stop its runtime.
    #[cfg(test)]
    in_process_access: Option<EmbeddedAgentAccess>,
    #[cfg(test)]
    recovery_probe:
        Mutex<Option<tokio::sync::mpsc::UnboundedSender<super::recovery_tests::RecoveryFact>>>,
    #[cfg(test)]
    recovery_checkpoint: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
    #[cfg(test)]
    recovery_refresh: std::sync::atomic::AtomicBool,
}

struct DisplayState {
    view: ViewRef,
    version: u64,
    execution: Option<ExecutionRef>,
    host: Option<HostLocation>,
}

impl Inner {
    fn cache_snapshot(
        &self,
        snapshot: Box<LiveChatSnapshot>,
        observed_execution: Option<ExecutionRef>,
    ) -> Box<LiveChatSnapshot> {
        let chat = snapshot.conversation.id;
        let mut snapshots = self.snapshots.write().unwrap();
        let mut references = self.references.write().unwrap();
        if observed_execution
            .is_some_and(|expected| references.get(&chat.uuid()) != Some(&expected))
        {
            return snapshots
                .get(&chat.uuid())
                .cloned()
                .map(Box::new)
                .unwrap_or(snapshot);
        }
        if let Some(current) = snapshots.get(&chat.uuid())
            && current.execution == snapshot.execution
            && current.last_sequence > snapshot.last_sequence
        {
            return Box::new(current.clone());
        }
        references.insert(chat.into(), snapshot.execution);
        self.remember_status(snapshot.status());
        snapshots.insert(chat.into(), (*snapshot).clone());
        snapshot
    }

    /// Caches a status the daemon just reported unless the View already holds a newer one; a
    /// status for an execution this View no longer follows yields the cached one instead.
    fn cache_status(
        &self,
        status: Box<LiveChatStatus>,
        observed_execution: Option<ExecutionRef>,
    ) -> Box<LiveChatStatus> {
        let chat = status.chat.id;
        let mut references = self.references.write().unwrap();
        if observed_execution
            .is_some_and(|expected| references.get(&chat.uuid()) != Some(&expected))
        {
            return self
                .statuses
                .read()
                .unwrap()
                .get(&chat.uuid())
                .cloned()
                .map(Box::new)
                .unwrap_or(status);
        }
        references.insert(chat.into(), status.execution);
        Box::new(self.remember_status(*status))
    }

    /// Keeps `status` unless a newer status of the same execution is cached, and returns the one
    /// kept.
    fn remember_status(&self, status: LiveChatStatus) -> LiveChatStatus {
        let mut statuses = self.statuses.write().unwrap();
        if let Some(current) = statuses.get(&status.chat.id.uuid())
            && current.execution == status.execution
            && current.chat.last_sequence > status.chat.last_sequence
        {
            return current.clone();
        }
        statuses.insert(status.chat.id.into(), status.clone());
        status
    }

    /// Reads the execution's status again after the daemon reported a change.
    async fn refresh_status(&self, execution: ExecutionRef) {
        match self
            .access
            .observe(ObservationRequest::Agent {
                execution,
                detail: AgentDetailQuery::Status,
                after: None,
            })
            .await
        {
            Ok(next) => {
                if let ObservationValue::Status(status) = next.initial {
                    self.cache_status(status, Some(execution));
                    #[cfg(test)]
                    let _ = self.facts.send(());
                }
            }
            Err(error) => *self.disconnected.lock().unwrap() = Some(error.to_string()),
        }
    }

    /// The execution's events after sequence `after`, when the daemon's replay window still
    /// holds every one of them; `None` when some are gone.
    async fn missed_events(
        &self,
        execution: ExecutionRef,
        after: u64,
    ) -> Result<Option<Vec<Event>>, AccessError> {
        let observation = self
            .access
            .observe(ObservationRequest::Agent {
                execution,
                detail: AgentDetailQuery::Events { after },
                after: None,
            })
            .await?;
        let ObservationValue::Events(events) = observation.initial else {
            return Err(unexpected());
        };
        Ok(events
            .first()
            .is_none_or(|first| first.sequence <= after.saturating_add(1))
            .then_some(events))
    }
}

/// Forwards one execution's feed events to the View in sequence order, each at most once,
/// whether the feed carried it or it was read after a Resync.
struct EventForwarder {
    execution: ExecutionRef,
    events: broadcast::Sender<Event>,
    event_delivery: Arc<tokio::sync::Mutex<HashMap<Uuid, (ExecutionRef, u64)>>>,
    /// Reports to recovery tests what happened to each event.
    #[cfg(test)]
    weak: std::sync::Weak<Inner>,
    /// The latest event the feed accounted for: forwarded, or reflected in its first answer.
    forwarded_through: u64,
}

impl EventForwarder {
    /// Forwards `event` unless it was already accounted for or a recovery covers it; `false` once
    /// the View is closing.
    async fn forward(&mut self, event: Event, cancel: &CancellationToken) -> bool {
        if event.sequence <= self.forwarded_through {
            return true;
        }
        self.forwarded_through = event.sequence;
        let covered = tokio::select! {
            _ = cancel.cancelled() => return false,
            covered = self.event_delivery.lock() => covered,
        };
        if covered
            .get(&event.chat_id.uuid())
            .is_some_and(|(scope, sequence)| {
                *scope == self.execution && event.sequence <= *sequence
            })
        {
            #[cfg(test)]
            if let Some(inner) = self.weak.upgrade()
                && let Some(sender) = &*inner.recovery_probe.lock().unwrap()
            {
                let _ = sender.send(super::recovery_tests::RecoveryFact::Skipped {
                    chat: event.chat_id.into(),
                    run: event.run_id.into(),
                    sequence: event.sequence,
                    completed: matches!(
                        event.kind,
                        tickloop_harness::protocol::EventKind::Completed { .. }
                    ),
                });
            }
            return true;
        }
        #[cfg(test)]
        let fact = super::recovery_tests::RecoveryFact::Forwarded {
            chat: event.chat_id.into(),
            sequence: event.sequence,
        };
        let _ = self.events.send(event);
        #[cfg(test)]
        if let Some(inner) = self.weak.upgrade()
            && let Some(sender) = &*inner.recovery_probe.lock().unwrap()
        {
            let _ = sender.send(fact);
        }
        true
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// The Agent an input-bearing command sends work to: a message or Skill, a queued action or a user
/// Shell. A lost reply for one of these must never be followed by the same input under a new id.
fn input_execution(command: &AgentCommand) -> Option<ExecutionRef> {
    match command {
        AgentCommand::SendFamilyInput {
            source_execution, ..
        } => Some(*source_execution),
        AgentCommand::Submit { execution, .. }
        | AgentCommand::ContinueLoop { execution, .. }
        | AgentCommand::EnqueueAfterTurn { execution, .. }
        | AgentCommand::ManageTask {
            execution,
            action: TaskAction::SubmitShell { .. },
        } => Some(*execution),
        _ => None,
    }
}

/// This View's `host` with the Workspace, Tab and Pane Herdr reports now, so the daemon records
/// where the View is. The endpoint and the terminal must still be the ones recorded.
async fn refresh_view_host(host: &HostLocation) -> Result<HostLocation, AccessError> {
    let (Some(pane), Some(terminal)) = (&host.pane, &host.terminal) else {
        return Err(AccessError::invalid("View host has no Pane or terminal"));
    };
    let pane = confirm_pane(&host.socket, &host.host_instance, pane, terminal)
        .await
        .map_err(|error| {
            let code = match error {
                PaneIdentityError::Denied(_) => AccessErrorCode::AccessDenied,
                PaneIdentityError::Replaced(_) => AccessErrorCode::VersionConflict,
                PaneIdentityError::Unresponsive => AccessErrorCode::Unavailable,
            };
            AccessError::new(code, format!("View host: {error}"))
        })?;
    Ok(HostLocation {
        workspace: Some(pane.workspace_id),
        tab: Some(pane.tab_id),
        pane: Some(pane.pane_id),
        ..host.clone()
    })
}

/// The project key persistent rules are saved under: the canonical directory, as the runtime
/// resolves an Agent's working directory.
fn rule_project(directory: &Path) -> Result<PathBuf, AccessError> {
    directory
        .canonicalize()
        .map_err(|_| AccessError::invalid("directory is unavailable"))
}

impl ConsoleExecution {
    pub(super) fn connected(
        client: LocalAgentClient,
        launch: LaunchSpec,
        host: Option<HostLocation>,
    ) -> Self {
        let epoch = client.daemon_epoch();
        let upgrades_supported = client.protocol().upgrade_protocol
            == Some(tickloop_code_access::RUNTIME_HANDOFF_VERSION);
        let view = ViewRef {
            session_id: client.session_id(),
            view_id: tickloop_ids::generate(),
        };
        let recovery = RecoveringAgentClient::new(
            client,
            Arc::new(|| {
                Box::pin(async {
                    let executable = std::env::current_exe()
                        .map_err(|error| AccessError::unavailable(error.to_string()))?;
                    tickloop_code_client::connect_to_running_daemon(executable)
                        .await
                        .map_err(AccessError::unavailable)
                })
            }),
        );
        let mut execution =
            Self::from_access(Arc::new(recovery.clone()), epoch, view, launch, host);
        let inner = Arc::get_mut(&mut execution.inner).expect("new connection is unique");
        inner.recovery = Some(recovery);
        inner.upgrades_supported = upgrades_supported;
        execution
    }

    fn from_access(
        access: Arc<dyn AgentAccess>,
        epoch: Uuid,
        view: ViewRef,
        launch: LaunchSpec,
        host: Option<HostLocation>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                access,
                upgrades_supported: false,
                epoch,
                launch,
                recovery: None,
                references: RwLock::new(HashMap::new()),
                snapshots: RwLock::new(HashMap::new()),
                statuses: RwLock::new(HashMap::new()),
                watching: Mutex::new(HashSet::new()),
                events: broadcast::channel(REPLAY_CAPACITY).0,
                feed_gaps: broadcast::channel(REPLAY_CAPACITY).0,
                event_delivery: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
                shells: broadcast::channel(REPLAY_CAPACITY).0,
                tasks: broadcast::channel(REPLAY_CAPACITY).0,
                #[cfg(test)]
                facts: broadcast::channel(REPLAY_CAPACITY).0,
                cancel: CancellationToken::new(),
                display: Mutex::new(DisplayState {
                    view,
                    version: 0,
                    execution: None,
                    host,
                }),
                disconnected: Mutex::new(None),
                draft: Mutex::new(NewAgentDraft::default()),
                launch_rules: Mutex::new(Vec::new()),
                first_create: Mutex::new(None),
                user_settings: Mutex::new(None),
                default_model: Mutex::new(None),
                new_agent_directory: Mutex::new(None),
                unconfirmed_inputs: Mutex::new(HashMap::new()),
                #[cfg(test)]
                in_process_access: None,
                #[cfg(test)]
                recovery_probe: Mutex::new(None),
                #[cfg(test)]
                recovery_checkpoint: Mutex::new(None),
                #[cfg(test)]
                recovery_refresh: std::sync::atomic::AtomicBool::new(true),
            }),
        }
    }

    #[cfg(test)]
    pub(super) fn set_recovery_probe(
        &self,
        sender: tokio::sync::mpsc::UnboundedSender<super::recovery_tests::RecoveryFact>,
    ) {
        *self.inner.recovery_probe.lock().unwrap() = Some(sender);
    }

    #[cfg(test)]
    pub(super) fn record_recovery_fact(&self, fact: super::recovery_tests::RecoveryFact) {
        if let Some(sender) = &*self.inner.recovery_probe.lock().unwrap() {
            let _ = sender.send(fact);
        }
    }

    #[cfg(test)]
    pub(super) fn hold_recovery_checkpoint(&self, gate: Arc<tokio::sync::Semaphore>) {
        *self.inner.recovery_checkpoint.lock().unwrap() = Some(gate);
        self.stop_global_refresh();
    }

    /// Stops the View's periodic global refresh, which otherwise repaints and reads the visible
    /// Chat on its own schedule.
    #[cfg(test)]
    pub(super) fn stop_global_refresh(&self) {
        self.inner
            .recovery_refresh
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(super) async fn recovery_checkpoint(&self) {
        let gate = self.inner.recovery_checkpoint.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.acquire().await.unwrap().forget();
        }
    }

    #[cfg(test)]
    pub(super) fn recovery_refresh_enabled(&self) -> bool {
        self.inner
            .recovery_refresh
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(super) fn from_access_for_test(
        access: Arc<dyn AgentAccess>,
        epoch: Uuid,
        view: ViewRef,
        launch: LaunchSpec,
        host: Option<HostLocation>,
    ) -> Self {
        Self::from_access(access, epoch, view, launch, host)
    }

    #[cfg(test)]
    pub(super) fn new_embedded(
        model: ModelConfig,
        client: Arc<dyn tickloop_harness::model::ModelClient>,
    ) -> Self {
        let core = tickloop_harness::runtime::Agent::new_embedded(model.clone(), client);
        core.install_own_tickhost();
        Self::from_embedded(core, model)
    }

    #[cfg(test)]
    pub(super) fn open_embedded(
        root: PathBuf,
        model: ModelConfig,
        client: Arc<dyn tickloop_harness::model::ModelClient>,
    ) -> Result<Self, tickloop_harness::runtime::AgentError> {
        let core =
            tickloop_harness::runtime::Agent::open_embedded(root.clone(), model.clone(), client)?;
        core.install_own_tickhost();
        let mut execution = Self::from_embedded(core, model);
        Arc::get_mut(&mut execution.inner)
            .expect("new execution is unique")
            .launch
            .config_root = Some(root);
        Ok(execution)
    }

    #[cfg(test)]
    pub(super) fn from_embedded_launch(
        core: tickloop_harness::runtime::Agent,
        launch: LaunchSpec,
    ) -> Self {
        let model = launch
            .model
            .clone()
            .expect("Console launch has a selected model");
        let mut execution = Self::from_embedded(core, model);
        Arc::get_mut(&mut execution.inner)
            .expect("new execution is unique")
            .launch = launch;
        execution
    }

    #[cfg(test)]
    pub(super) fn from_embedded(
        core: tickloop_harness::runtime::Agent,
        model: ModelConfig,
    ) -> Self {
        let adapter = EmbeddedAgentAccess::new(core);
        let epoch = adapter.daemon_epoch();
        let view = ViewRef {
            session_id: tickloop_ids::generate(),
            view_id: tickloop_ids::generate(),
        };
        let mut launch =
            LaunchSpec::embedded(std::env::current_dir().expect("test current directory"));
        launch.model = Some(model);
        let mut execution = Self::from_access(Arc::new(adapter.clone()), epoch, view, launch, None);
        Arc::get_mut(&mut execution.inner)
            .expect("new execution is unique")
            .in_process_access = Some(adapter);
        execution
    }

    pub(super) fn launch_spec(&self) -> LaunchSpec {
        self.effective_launch()
    }
    pub(super) fn access(&self) -> Arc<dyn AgentAccess> {
        self.inner.access.clone()
    }
    pub(super) fn daemon_epoch(&self) -> Uuid {
        self.inner
            .recovery
            .as_ref()
            .map(RecoveringAgentClient::daemon_epoch)
            .unwrap_or(self.inner.epoch)
    }
    pub(super) fn host_location(&self) -> Option<HostLocation> {
        self.inner.display.lock().unwrap().host.clone()
    }
    pub(super) fn disconnected(&self) -> Option<String> {
        self.inner.disconnected.lock().unwrap().clone()
    }

    pub(super) async fn catalog(&self) -> Result<Vec<AgentSummary>, AccessError> {
        let known = self.inner.references.read().unwrap().clone();
        let mut offset = 0;
        let mut rows = Vec::new();
        loop {
            let observation = self
                .inner
                .access
                .observe(ObservationRequest::Overview {
                    filter: AgentFilter::default(),
                    page: Page {
                        offset,
                        limit: MAX_PAGE_LIMIT,
                    },
                    after: None,
                })
                .await?;
            let ObservationValue::Overview(page) = observation.initial else {
                return Err(unexpected());
            };
            let done = page.len() < MAX_PAGE_LIMIT;
            offset += page.len();
            for row in &page {
                let mut references = self.inner.references.write().unwrap();
                if references.get(&row.chat_id) == known.get(&row.chat_id)
                    || references.get(&row.chat_id) == Some(&row.execution)
                {
                    references.insert(row.chat_id, row.execution);
                }
            }
            rows.extend(page);
            if done {
                return Ok(rows);
            }
        }
    }

    /// The Agent and title `chat` had in the latest status this View received; `None` before any.
    pub(super) fn cached_title(&self, chat: Uuid) -> Option<(Uuid, String)> {
        let statuses = self.inner.statuses.read().unwrap();
        let status = statuses.get(&chat)?;
        Some((
            status.execution.agent_id,
            status
                .subagent
                .as_ref()
                .map(|registration| registration.handle.clone())
                .unwrap_or_else(|| status.chat.display_title()),
        ))
    }

    pub(super) fn cached_is_delegated(&self, chat: Uuid) -> bool {
        self.cached_delegation(chat).unwrap_or(false)
    }

    /// Whether `chat` is a delegated child, from the latest status this View received; `None`
    /// before any. A Chat stays the root or child it was created as, so any status answers.
    fn cached_delegation(&self, chat: Uuid) -> Option<bool> {
        self.inner
            .statuses
            .read()
            .unwrap()
            .get(&chat)
            .map(|status| status.chat.delegation.is_some())
    }

    pub(super) fn cached_pending_plan_approvals(
        &self,
        chat: Uuid,
    ) -> Option<(ExecutionRef, Vec<u64>)> {
        let statuses = self.inner.statuses.read().unwrap();
        let status = statuses.get(&chat)?;
        Some((
            status.execution,
            status
                .chat
                .plans
                .iter()
                .filter(|plan| {
                    plan.approval_pending
                        && status.chat.chat_loop.id == plan.loop_id
                        && matches!(
                            status.chat.chat_loop.state,
                            LoopState::WaitingPlan | LoopState::Interrupted
                        )
                        && !status.chat.permissions.iter().any(|request| {
                            request.name == tickloop_harness::tools::EXIT_PLAN_MODE_TOOL_NAME
                                && request.loop_id == plan.loop_id
                        })
                })
                .map(|plan| plan.version)
                .collect(),
        ))
    }

    pub(super) fn cached_plan_is_pending(&self, chat: Uuid, version: u64) -> Option<bool> {
        let statuses = self.inner.statuses.read().unwrap();
        let status = statuses.get(&chat)?;
        Some(
            status
                .chat
                .plans
                .iter()
                .any(|plan| plan.version == version && plan.approval_pending),
        )
    }

    pub(super) fn cached_reference(&self, chat: Uuid) -> Option<ExecutionRef> {
        self.inner.references.read().unwrap().get(&chat).copied()
    }

    pub(super) async fn reference(&self, chat: Uuid) -> Result<ExecutionRef, AccessError> {
        if let Some(reference) = self.cached_reference(chat) {
            return Ok(reference);
        }
        self.catalog().await?;
        self.inner
            .references
            .read()
            .unwrap()
            .get(&chat)
            .copied()
            .ok_or_else(|| AccessError::not_found("Chat is not known to this View"))
    }

    pub(super) async fn chat_for_agent(&self, agent_id: Uuid) -> Result<Uuid, AccessError> {
        self.catalog()
            .await?
            .into_iter()
            .find(|row| row.execution.agent_id == agent_id)
            .map(|row| row.chat_id)
            .ok_or_else(|| AccessError::not_found("Agent does not exist"))
    }

    async fn value(
        &self,
        chat: Uuid,
        detail: AgentDetailQuery,
    ) -> Result<ObservationValue, AccessError> {
        self.observed_value(chat, detail)
            .await
            .map(|(_, value)| value)
    }

    async fn observed_value(
        &self,
        chat: Uuid,
        detail: AgentDetailQuery,
    ) -> Result<(ExecutionRef, ObservationValue), AccessError> {
        let execution = self.reference(chat).await?;
        self.inner
            .access
            .observe(ObservationRequest::Agent {
                execution,
                detail,
                after: None,
            })
            .await
            .map(|observation| (execution, observation.initial))
    }

    pub(super) async fn command(
        &self,
        mut command: AgentCommand,
        command_id: Option<Uuid>,
    ) -> Result<CommandResult, AccessError> {
        if matches!(
            &command,
            AgentCommand::SendFamilyInput { .. }
                | AgentCommand::Create {
                    origin: Some(_),
                    ..
                }
                | AgentCommand::Submit {
                    origin: Some(_),
                    ..
                }
                | AgentCommand::ContinueLoop {
                    origin: Some(_),
                    ..
                }
                | AgentCommand::EnqueueAfterTurn {
                    action: AfterTurnAction::Submit {
                        origin: Some(_),
                        ..
                    },
                    ..
                }
        ) {
            self.refresh_interaction_source().await?;
            match &mut command {
                AgentCommand::SendFamilyInput { origin, .. } => {
                    *origin = self.origin().ok_or_else(|| {
                        AccessError::invalid("directed input needs an authenticated displayed View")
                    })?;
                }
                AgentCommand::Create { origin, .. }
                | AgentCommand::Submit { origin, .. }
                | AgentCommand::ContinueLoop { origin, .. }
                | AgentCommand::EnqueueAfterTurn {
                    action: AfterTurnAction::Submit { origin, .. },
                    ..
                } => *origin = self.origin(),
                _ => {}
            }
        }
        let command_id = command_id.unwrap_or_else(tickloop_ids::generate);
        let epoch = self.daemon_epoch();
        let input_target = input_execution(&command);
        if let Some(execution) = input_target.or(match &command {
            AgentCommand::Configure { execution, .. } => Some(*execution),
            _ => None,
        }) {
            self.ensure_first_create_settled(execution)?;
        }
        if let Some(execution) = input_target {
            self.release_settled_input(execution).await?;
        }
        match self.send(epoch, command_id, command).await {
            CommandOutcome::Completed(result) => Ok(result),
            CommandOutcome::Refused { error, .. } | CommandOutcome::Undelivered(error) => {
                Err(error)
            }
            CommandOutcome::Unknown => {
                if let Some(execution) = input_target {
                    self.inner
                        .unconfirmed_inputs
                        .lock()
                        .unwrap()
                        .insert(execution, (epoch, command_id));
                    return Err(AccessError::invalid(format!(
                        "Input {command_id} has an unknown acceptance result; nothing was resent. /acknowledge-input {command_id} releases it without resending"
                    )));
                }
                Err(AccessError::invalid(format!(
                    "Command {command_id} has an unknown result; nothing was resent"
                )))
            }
        }
    }
    /// Sends `command` once under `command_id` and reports what its reply proves; a lost reply
    /// reads the receipt and never resends.
    async fn send(&self, epoch: Uuid, command_id: Uuid, command: AgentCommand) -> CommandOutcome {
        match self
            .inner
            .access
            .execute(CommandEnvelope {
                daemon_epoch: epoch,
                command_id,
                command,
            })
            .await
        {
            Ok(receipt) => CommandOutcome::of(receipt),
            Err(error) if error.code == AccessErrorCode::OutcomeUnknown => {
                self.read_receipt(epoch, command_id).await
            }
            Err(error) => CommandOutcome::Undelivered(error),
        }
    }
    /// Reads a command's receipt without sending anything.
    async fn read_receipt(&self, epoch: Uuid, command_id: Uuid) -> CommandOutcome {
        match self
            .inner
            .access
            .observe(ObservationRequest::Receipt { epoch, command_id })
            .await
        {
            Ok(Observation {
                initial: ObservationValue::Receipt(Some(receipt)),
                ..
            }) => CommandOutcome::of(*receipt),
            _ => CommandOutcome::Unknown,
        }
    }
    /// Refuses input and setting changes to an Agent whose first-input Create or staged choices
    /// are not proven yet.
    fn ensure_first_create_settled(&self, execution: ExecutionRef) -> Result<(), AccessError> {
        match &*self.inner.first_create.lock().unwrap() {
            Some(FirstCreateState::Created {
                execution: pending,
                blocked,
                ..
            }) if *pending == execution => Err(AccessError::invalid(match blocked {
                Some(
                    BlockedChoice::Unknown { command_id, .. }
                    | BlockedChoice::Refused { command_id, .. },
                ) => format!(
                    "The New Agent's staged choices are not all applied; nothing was sent. /acknowledge-create {command_id} cancels the remaining choices"
                ),
                None => "The New Agent's staged choices are not all applied; nothing was sent. Send the input again to apply them first".into(),
            })),
            Some(FirstCreateState::RegisteredFailed {
                command_id,
                execution: pending,
                ..
            }) if *pending == execution => Err(AccessError::invalid(format!(
                "Creation {command_id} failed after this Agent was registered; nothing was sent"
            ))),
            _ => Ok(()),
        }
    }
    fn ensure_chat_settled(&self, chat: Uuid) -> Result<(), AccessError> {
        let execution = self.inner.references.read().unwrap().get(&chat).copied();
        execution.map_or(Ok(()), |execution| {
            self.ensure_first_create_settled(execution)
        })
    }
    /// Refuses more input to `execution` while an earlier input's acceptance is unknown; reading
    /// the receipt is the only action, and a final receipt releases the Agent for new input.
    async fn release_settled_input(&self, execution: ExecutionRef) -> Result<(), AccessError> {
        let Some((epoch, command_id)) = self
            .inner
            .unconfirmed_inputs
            .lock()
            .unwrap()
            .get(&execution)
            .copied()
        else {
            return Ok(());
        };
        if matches!(
            self.read_receipt(epoch, command_id).await,
            CommandOutcome::Unknown
        ) {
            return Err(AccessError::invalid(format!(
                "Input {command_id} still has an unknown acceptance result; nothing was resent. /acknowledge-input {command_id} releases it without resending"
            )));
        }
        self.inner
            .unconfirmed_inputs
            .lock()
            .unwrap()
            .remove(&execution);
        Ok(())
    }
    /// The input to `chat` whose acceptance is unknown, if any.
    pub(super) fn unknown_input(&self, chat: Uuid) -> Option<Uuid> {
        let execution = self.inner.references.read().unwrap().get(&chat).copied()?;
        self.inner
            .unconfirmed_inputs
            .lock()
            .unwrap()
            .get(&execution)
            .map(|(_, command_id)| *command_id)
    }
    /// Reads the receipt of the unknown input `command_id` to `chat`. An accepted input is adopted
    /// rather than sent again, and any final receipt releases the Agent for new input.
    pub(super) async fn resolve_input(&self, chat: Uuid, command_id: Uuid) -> InputResolution {
        let Some(execution) = self.inner.references.read().unwrap().get(&chat).copied() else {
            return InputResolution::Released;
        };
        let pending = self
            .inner
            .unconfirmed_inputs
            .lock()
            .unwrap()
            .get(&execution)
            .copied()
            .filter(|(_, pending)| *pending == command_id);
        let Some((epoch, command_id)) = pending else {
            return InputResolution::Released;
        };
        let resolution = match self.read_receipt(epoch, command_id).await {
            CommandOutcome::Unknown => return InputResolution::Unknown,
            CommandOutcome::Completed(result) => InputResolution::Accepted(Box::new(result)),
            CommandOutcome::Refused { .. } | CommandOutcome::Undelivered(_) => {
                InputResolution::Refused
            }
        };
        self.inner
            .unconfirmed_inputs
            .lock()
            .unwrap()
            .remove(&execution);
        resolution
    }
    /// Stops tracking the unknown input `command_id` without resending it.
    pub(super) fn acknowledge_input(&self, command_id: Uuid) -> bool {
        let mut inputs = self.inner.unconfirmed_inputs.lock().unwrap();
        let before = inputs.len();
        inputs.retain(|_, (_, pending)| *pending != command_id);
        inputs.len() != before
    }

    pub(super) async fn display_chat(&self, chat: Option<Uuid>) -> Result<(), AccessError> {
        self.refresh_display_session();
        let execution = match chat {
            Some(chat) => Some(self.reference(chat).await?),
            None => None,
        };
        self.display_execution(execution, false).await
    }

    pub(super) async fn refresh_interaction_source(&self) -> Result<(), AccessError> {
        self.refresh_display_session();
        self.renew_display().await?;
        let execution = self.inner.display.lock().unwrap().execution;
        self.display_execution(execution, false).await
    }

    async fn display_execution(
        &self,
        execution: Option<ExecutionRef>,
        force: bool,
    ) -> Result<(), AccessError> {
        let (view, version, host, old) = {
            let display = self.inner.display.lock().unwrap();
            (
                display.view,
                display.version,
                display.host.clone(),
                display.execution,
            )
        };
        let refreshed_host = match &host {
            Some(host) => Some(refresh_view_host(host).await?),
            None => None,
        };
        if !force && execution == old && refreshed_host == host && version > 0 {
            return Ok(());
        }
        let update = match execution {
            Some(execution) => DisplayUpdate::Displayed {
                view,
                expected_version: version,
                execution,
                host: refreshed_host.clone(),
            },
            None => DisplayUpdate::Scene {
                view,
                expected_version: version,
                host: refreshed_host.clone(),
            },
        };
        let receipt = self.inner.access.show_view(update).await?;
        let mut display = self.inner.display.lock().unwrap();
        if display.view != receipt.view {
            return Err(AccessError::invalid(
                "View connection changed during display registration",
            ));
        }
        display.version = receipt.version;
        display.execution = execution;
        display.host = refreshed_host;
        Ok(())
    }

    pub(super) async fn renew_display(&self) -> Result<(), AccessError> {
        self.refresh_display_session();
        let (view, version, shown) = {
            let display = self.inner.display.lock().unwrap();
            (display.view, display.version, display.version > 0)
        };
        if shown {
            let renewed = self
                .inner
                .access
                .show_view(DisplayUpdate::Renew { view, version })
                .await;
            match renewed {
                Ok(_) => {}
                Err(error) if error.code == AccessErrorCode::VersionConflict => {
                    let execution = self.inner.display.lock().unwrap().execution;
                    self.display_execution(execution, true).await?;
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn refresh_display_session(&self) {
        if let Some(recovery) = &self.inner.recovery {
            let mut display = self.inner.display.lock().unwrap();
            if display.view.session_id != recovery.session_id() {
                display.view = ViewRef {
                    session_id: recovery.session_id(),
                    view_id: tickloop_ids::generate(),
                };
                display.version = 0;
                display.execution = None;
            }
        }
    }

    pub(super) async fn leave_view(&self) -> Result<(), AccessError> {
        let (view, version) = {
            let display = self.inner.display.lock().unwrap();
            (display.view, display.version)
        };
        self.inner.cancel.cancel();
        if version == 0 {
            return Ok(());
        }
        match self
            .inner
            .access
            .show_view(DisplayUpdate::Left { view, version })
            .await
        {
            Ok(_) => Ok(()),
            Err(error)
                if matches!(
                    error.code,
                    AccessErrorCode::Unavailable
                        | AccessErrorCode::AccessDenied
                        | AccessErrorCode::VersionConflict
                        | AccessErrorCode::EpochChanged
                        | AccessErrorCode::StaleExecution
                        | AccessErrorCode::NotFound
                ) =>
            {
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    pub(super) fn origin(&self) -> Option<InteractionSource> {
        let display = self.inner.display.lock().unwrap();
        (display.version > 0).then(|| InteractionSource {
            view: display.view,
            display_version: display.version,
            host: display.host.clone(),
        })
    }

    async fn watch(&self, execution: ExecutionRef) -> Result<(), AccessError> {
        if !self.inner.watching.lock().unwrap().insert(execution) {
            return Ok(());
        }
        let mut observation = match self
            .inner
            .access
            .observe(ObservationRequest::Agent {
                execution,
                detail: AgentDetailQuery::Status,
                after: None,
            })
            .await
        {
            Ok(observation) => observation,
            Err(error) => {
                self.inner.watching.lock().unwrap().remove(&execution);
                return Err(error);
            }
        };
        let ObservationValue::Status(status) = observation.initial.clone() else {
            self.inner.watching.lock().unwrap().remove(&execution);
            return Err(unexpected());
        };
        let chat = status.chat.id;
        let mut forwarder = EventForwarder {
            execution,
            events: self.inner.events.clone(),
            event_delivery: self.inner.event_delivery.clone(),
            #[cfg(test)]
            weak: Arc::downgrade(&self.inner),
            forwarded_through: status.chat.last_sequence,
        };
        self.inner.cache_status(status, Some(execution));
        let feed_gaps = self.inner.feed_gaps.clone();
        let shells = self.inner.shells.clone();
        let tasks = self.inner.tasks.clone();
        let cancel = self.inner.cancel.clone();
        let weak = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            loop {
                let update = tokio::select! { _=cancel.cancelled()=>break, update=observation.recv()=>update };
                match update {
                    Ok(ObservationUpdate {
                        change: ObservationChange::Event(event),
                        ..
                    }) => {
                        if !forwarder.forward(event, &cancel).await {
                            break;
                        }
                    }
                    Ok(ObservationUpdate {
                        change: ObservationChange::UserShell(shell),
                        ..
                    }) => {
                        let _ = shells.send(shell);
                    }
                    Ok(ObservationUpdate {
                        change: ObservationChange::TaskNotification(task),
                        ..
                    }) => {
                        let _ = tasks.send(task);
                    }
                    // The events the Reset stands for never arrive; the gap is announced before
                    // anything the feed forwards after it.
                    Ok(ObservationUpdate {
                        change: ObservationChange::Reset { snapshot, .. },
                        ..
                    }) => {
                        if let Some(inner) = weak.upgrade() {
                            forwarder.forwarded_through =
                                forwarder.forwarded_through.max(snapshot.last_sequence);
                            inner.cache_snapshot(snapshot, Some(execution));
                            let _ = feed_gaps.send(chat.into());
                            #[cfg(test)]
                            let _ = inner.facts.send(());
                        }
                    }
                    Ok(ObservationUpdate {
                        change:
                            ObservationChange::AccountModels(_) | ObservationChange::SearchTest(_),
                        ..
                    }) => {}
                    // A Resync stands for a change too large for the feed, such as an event with
                    // a long tool result or a Reset with its snapshot. The events it skipped are
                    // read by sequence; only events gone from the daemon's replay window leave a
                    // gap for the View to restore from a snapshot.
                    Ok(ObservationUpdate {
                        change: ObservationChange::Resync { .. },
                        ..
                    }) => {
                        let Some(inner) = weak.upgrade() else {
                            break;
                        };
                        match inner
                            .missed_events(execution, forwarder.forwarded_through)
                            .await
                        {
                            Ok(Some(missed)) => {
                                drop(inner);
                                let mut forwarded = true;
                                for event in missed {
                                    forwarded = forwarder.forward(event, &cancel).await;
                                    if !forwarded {
                                        break;
                                    }
                                }
                                if !forwarded {
                                    break;
                                }
                            }
                            Ok(None) => {
                                let _ = feed_gaps.send(chat.into());
                            }
                            Err(error) => {
                                *inner.disconnected.lock().unwrap() = Some(error.to_string());
                                let _ = feed_gaps.send(chat.into());
                            }
                        }
                        let Some(inner) = weak.upgrade() else {
                            break;
                        };
                        inner.refresh_status(execution).await;
                    }
                    Ok(ObservationUpdate {
                        change: ObservationChange::Changed,
                        ..
                    }) => {
                        let Some(inner) = weak.upgrade() else {
                            break;
                        };
                        inner.refresh_status(execution).await;
                    }
                    Err(error) => {
                        if let Some(inner) = weak.upgrade() {
                            *inner.disconnected.lock().unwrap() = Some(error.to_string());
                            inner.watching.lock().unwrap().remove(&execution);
                        }
                        break;
                    }
                }
            }
        });
        Ok(())
    }

    /// Whether this View follows `execution`'s feed, so its events arrive as they happen.
    pub(super) fn follows(&self, execution: ExecutionRef) -> bool {
        self.inner.watching.lock().unwrap().contains(&execution)
    }

    pub(super) async fn pause_event_delivery(&self) -> EventRecovery {
        EventRecovery {
            covered: self.inner.event_delivery.clone().lock_owned().await,
            inner: self.inner.clone(),
        }
    }

    #[cfg(test)]
    pub(super) fn subscribe_facts(&self) -> broadcast::Receiver<()> {
        self.inner.facts.subscribe()
    }

    pub(super) fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.inner.events.subscribe()
    }
    /// Chats whose daemon feed skipped events. Each gap arrives before any event the feed
    /// forwards after it.
    pub(super) fn subscribe_feed_gaps(&self) -> broadcast::Receiver<Uuid> {
        self.inner.feed_gaps.subscribe()
    }
    pub(super) fn subscribe_user_shells(&self) -> broadcast::Receiver<UserShellRecord> {
        self.inner.shells.subscribe()
    }
    pub(super) fn subscribe_task_notifications(&self) -> broadcast::Receiver<TaskNotification> {
        self.inner.tasks.subscribe()
    }

    /// The Chat with its whole history: for opening, restoring or rewinding it and for results
    /// drawn from the conversation. State alone comes from `status`.
    pub(super) async fn snapshot(&self, chat: Uuid) -> Result<ChatSnapshot, AccessError> {
        self.live_snapshot(chat)
            .await
            .map(|snapshot| snapshot.into_snapshot())
    }

    /// What the Chat is doing now, without its history. An unreachable daemon answers with the
    /// cached status, as `snapshot` does.
    pub(super) async fn status(&self, chat: Uuid) -> Result<ChatStatus, AccessError> {
        match self.observed_value(chat, AgentDetailQuery::Status).await {
            Ok((execution, value)) => self.adopt_status(execution, value).await,
            Err(error) => {
                *self.inner.disconnected.lock().unwrap() = Some(error.to_string());
                self.inner
                    .statuses
                    .read()
                    .unwrap()
                    .get(&chat)
                    .map(|status| status.chat.clone())
                    .ok_or(error)
            }
        }
    }

    /// The status the daemon reports now. Unlike `status`, an unreachable daemon is an error
    /// rather than the cached copy, so the answer can prove that a run has ended.
    pub(super) async fn current_status(&self, chat: Uuid) -> Result<ChatStatus, AccessError> {
        let (execution, value) = self
            .observed_value(chat, AgentDetailQuery::Status)
            .await
            .inspect_err(|error| {
                *self.inner.disconnected.lock().unwrap() = Some(error.to_string());
            })?;
        self.adopt_status(execution, value).await
    }

    /// Caches a status the daemon just reported and keeps following its execution.
    async fn adopt_status(
        &self,
        execution: ExecutionRef,
        value: ObservationValue,
    ) -> Result<ChatStatus, AccessError> {
        let ObservationValue::Status(status) = value else {
            return Err(unexpected());
        };
        *self.inner.disconnected.lock().unwrap() = None;
        let status = self.inner.cache_status(status, Some(execution));
        self.watch(status.execution).await?;
        Ok(status.chat)
    }

    /// The tool operation `run` recorded last for `call_id`.
    pub(super) async fn operation(
        &self,
        chat: Uuid,
        run_id: Uuid,
        call_id: &str,
    ) -> Result<Option<OperationRecord>, AccessError> {
        match self
            .value(
                chat,
                AgentDetailQuery::Operation {
                    run_id,
                    call_id: call_id.to_owned(),
                },
            )
            .await?
        {
            ObservationValue::Operation(operation) => Ok(operation.map(|operation| *operation)),
            _ => Err(unexpected()),
        }
    }

    pub(super) async fn live_snapshot(
        &self,
        chat: Uuid,
    ) -> Result<Box<LiveChatSnapshot>, AccessError> {
        match self.observed_value(chat, AgentDetailQuery::Snapshot).await {
            Ok((execution, value)) => self.adopt_snapshot(execution, value).await,
            Err(error) => {
                *self.inner.disconnected.lock().unwrap() = Some(error.to_string());
                self.inner
                    .snapshots
                    .read()
                    .unwrap()
                    .get(&chat)
                    .cloned()
                    .map(Box::new)
                    .ok_or(error)
            }
        }
    }

    async fn fill_goal_history(&self, snapshot: &mut LiveChatSnapshot) {
        if !snapshot.conversation.goal_history.is_empty() {
            return;
        }
        let cached = self
            .inner
            .snapshots
            .read()
            .unwrap()
            .get(&snapshot.conversation.id.uuid())
            .map(|cached| cached.conversation.goal_history.clone());
        if let Some(history) = cached {
            snapshot.conversation.goal_history = history;
            return;
        }
        let Some(root) = &self.inner.launch.config_root else {
            return;
        };
        match self
            .inner
            .access
            .observe(ObservationRequest::Transcript {
                record: TranscriptRef {
                    store_root: root.clone(),
                    chat_id: snapshot.conversation.id.into(),
                },
                page: Page {
                    offset: 0,
                    limit: 1,
                },
            })
            .await
        {
            Ok(observation) => {
                if let ObservationValue::Transcript(record) = observation.initial {
                    snapshot.conversation.goal_history = record.goal_history;
                }
            }
            Err(error) if error.code == AccessErrorCode::NotFound => {}
            Err(error) => snapshot
                .warnings
                .push(format!("Goal history unavailable: {error}")),
        }
    }

    /// Caches an Agent snapshot the daemon just reported and keeps following its execution.
    async fn adopt_snapshot(
        &self,
        execution: ExecutionRef,
        value: ObservationValue,
    ) -> Result<Box<LiveChatSnapshot>, AccessError> {
        let ObservationValue::Agent(mut snapshot) = value else {
            return Err(unexpected());
        };
        *self.inner.disconnected.lock().unwrap() = None;
        self.fill_goal_history(&mut snapshot).await;
        let snapshot = self.inner.cache_snapshot(snapshot, Some(execution));
        self.watch(snapshot.execution).await?;
        Ok(snapshot)
    }
    pub(super) async fn observe(
        &self,
        chat: Uuid,
        after: u64,
    ) -> Result<ExecutionObservation, AccessError> {
        let live = self.live_snapshot(chat).await?;
        let execution = live.execution;
        let snapshot = live.into_snapshot();
        let reset_required = after > snapshot.last_sequence
            || snapshot
                .events
                .first()
                .is_some_and(|event| after.saturating_add(1) < event.sequence);
        let events = if reset_required {
            Vec::new()
        } else {
            snapshot
                .events
                .iter()
                .filter(|event| event.sequence > after)
                .cloned()
                .collect()
        };
        Ok(ExecutionObservation {
            execution,
            snapshot,
            events,
            reset_required,
        })
    }

    #[cfg(test)]
    pub(super) async fn create_chat(&self, workdir: PathBuf) -> Result<Uuid, AccessError> {
        let mut launch = self.inner.launch.clone();
        launch.folder = workdir;
        launch.exact_chat = None;
        self.create(launch, None).await.map(|(chat, _)| chat)
    }
    /// Creates the New Agent for its first input: one Create from the View's effective launch, then
    /// the staged process-local choices through the same Configure commands a visible Agent uses.
    /// Every command is proven by its reply or receipt before the next one is sent. A Create or a
    /// choice whose reply is lost, a choice the daemon refuses, and an Agent this View cannot attach
    /// to yet are all remembered with what remains: the next attempt reads receipts or attaches
    /// again, never sends a second Create or an unknown choice again, and no input reaches the
    /// Agent until every choice is applied or the user cancels the rest.
    pub(super) async fn create_from_draft(
        &self,
        workdir: PathBuf,
    ) -> Result<Uuid, DraftCreationError> {
        let pending = self.inner.first_create.lock().unwrap().take();
        let (chat, execution, mut replay, blocked) = match pending {
            Some(FirstCreateState::Created {
                chat,
                execution,
                replay,
                blocked,
            }) => (chat, execution, replay, blocked),
            Some(FirstCreateState::RegisteredFailed {
                command_id,
                execution,
                error,
                draft,
            }) => {
                let reported = DraftCreationError::RegisteredFailed {
                    command_id,
                    agent_id: execution.agent_id,
                    error: error.clone(),
                };
                *self.inner.first_create.lock().unwrap() =
                    Some(FirstCreateState::RegisteredFailed {
                        command_id,
                        execution,
                        error,
                        draft,
                    });
                return Err(reported);
            }
            Some(FirstCreateState::Unconfirmed {
                epoch,
                command_id,
                draft,
            }) => {
                let outcome = self.read_receipt(epoch, command_id).await;
                if let CommandOutcome::Refused {
                    registered: None, ..
                }
                | CommandOutcome::Undelivered(_) = outcome
                {
                    // The earlier Create provably made nothing, so this input may create the Agent.
                    *self.inner.draft.lock().unwrap() = draft;
                    return Box::pin(self.create_from_draft(workdir)).await;
                }
                let replay = draft
                    .clone()
                    .into_first_create(self.effective_launch(), workdir);
                let (chat, execution) = self.settle_create(epoch, command_id, outcome, draft)?;
                (chat, execution, replay, None)
            }
            None => {
                let draft = std::mem::take(&mut *self.inner.draft.lock().unwrap());
                let replay = draft
                    .clone()
                    .into_first_create(self.effective_launch(), workdir);
                let epoch = self.daemon_epoch();
                let command_id = tickloop_ids::generate();
                let outcome = self
                    .send_create(epoch, command_id, replay.launch.clone())
                    .await;
                let (chat, execution) = self.settle_create(epoch, command_id, outcome, draft)?;
                (chat, execution, replay, None)
            }
        };
        let retain = |replay: FirstCreate, blocked: Option<BlockedChoice>| {
            *self.inner.first_create.lock().unwrap() = Some(FirstCreateState::Created {
                chat,
                execution,
                replay,
                blocked,
            });
        };
        if let Err(error) = self.watch(execution).await {
            retain(replay, blocked);
            return Err(DraftCreationError::AttachUnavailable { chat, error });
        }
        match blocked {
            Some(BlockedChoice::Unknown { epoch, command_id }) => {
                match self.read_receipt(epoch, command_id).await {
                    CommandOutcome::Completed(_) => replay.complete_step(),
                    CommandOutcome::Refused { error, .. } | CommandOutcome::Undelivered(error) => {
                        let error = error.message;
                        retain(
                            replay,
                            Some(BlockedChoice::Refused {
                                command_id,
                                error: error.clone(),
                            }),
                        );
                        return Err(DraftCreationError::ChoiceRefused {
                            chat,
                            command_id,
                            error,
                        });
                    }
                    CommandOutcome::Unknown => {
                        retain(replay, Some(BlockedChoice::Unknown { epoch, command_id }));
                        return Err(DraftCreationError::ChoiceUnknown { chat, command_id });
                    }
                }
            }
            Some(BlockedChoice::Refused { command_id, error }) => {
                retain(
                    replay,
                    Some(BlockedChoice::Refused {
                        command_id,
                        error: error.clone(),
                    }),
                );
                return Err(DraftCreationError::ChoiceRefused {
                    chat,
                    command_id,
                    error,
                });
            }
            None => {}
        }
        while let Some(step) = replay.next_step() {
            let change = match self.replay_change(chat, step).await {
                Ok(change) => change,
                Err(error) => {
                    retain(replay, None);
                    return Err(DraftCreationError::Unapplied { chat, error });
                }
            };
            let epoch = self.daemon_epoch();
            let command_id = tickloop_ids::generate();
            match self
                .send(
                    epoch,
                    command_id,
                    AgentCommand::Configure {
                        execution,
                        change,
                        provider: None,
                    },
                )
                .await
            {
                CommandOutcome::Completed(_) => replay.complete_step(),
                CommandOutcome::Undelivered(error) => {
                    retain(replay, None);
                    return Err(DraftCreationError::Unapplied { chat, error });
                }
                CommandOutcome::Refused { error, .. } => {
                    let error = error.message;
                    retain(
                        replay,
                        Some(BlockedChoice::Refused {
                            command_id,
                            error: error.clone(),
                        }),
                    );
                    return Err(DraftCreationError::ChoiceRefused {
                        chat,
                        command_id,
                        error,
                    });
                }
                CommandOutcome::Unknown => {
                    retain(replay, Some(BlockedChoice::Unknown { epoch, command_id }));
                    return Err(DraftCreationError::ChoiceUnknown { chat, command_id });
                }
            }
        }
        Ok(chat)
    }
    /// What the first Create's reply or receipt proves; anything short of a created Agent is kept
    /// so that nothing is created twice.
    fn settle_create(
        &self,
        epoch: Uuid,
        command_id: Uuid,
        outcome: CommandOutcome,
        draft: NewAgentDraft,
    ) -> Result<(Uuid, ExecutionRef), DraftCreationError> {
        match outcome {
            CommandOutcome::Completed(CommandResult::Created { snapshot, .. }) => {
                self.inner.launch_rules.lock().unwrap().clear();
                let chat = snapshot.conversation.id;
                let execution = snapshot.execution;
                self.inner.cache_snapshot(snapshot, None);
                Ok((chat.into(), execution))
            }
            CommandOutcome::Refused {
                error,
                registered: Some(execution),
            } => {
                *self.inner.first_create.lock().unwrap() =
                    Some(FirstCreateState::RegisteredFailed {
                        command_id,
                        execution,
                        error: error.message.clone(),
                        draft,
                    });
                Err(DraftCreationError::RegisteredFailed {
                    command_id,
                    agent_id: execution.agent_id,
                    error: error.message,
                })
            }
            CommandOutcome::Unknown => {
                *self.inner.first_create.lock().unwrap() = Some(FirstCreateState::Unconfirmed {
                    epoch,
                    command_id,
                    draft,
                });
                Err(DraftCreationError::Unconfirmed(command_id))
            }
            CommandOutcome::Refused { error, .. } | CommandOutcome::Undelivered(error) => {
                *self.inner.draft.lock().unwrap() = draft;
                Err(DraftCreationError::NotCreated(error))
            }
            CommandOutcome::Completed(_) => {
                *self.inner.draft.lock().unwrap() = draft;
                Err(DraftCreationError::NotCreated(unexpected()))
            }
        }
    }
    /// The Configure change for a staged choice, read against the created Agent's current state.
    async fn replay_change(
        &self,
        chat: Uuid,
        step: ReplayStep,
    ) -> Result<ConfigurationChange, AccessError> {
        Ok(match step {
            ReplayStep::ExecutionMode(mode) => ConfigurationChange::ExecutionMode { mode },
            ReplayStep::Directories(additional) => ConfigurationChange::Directories {
                folder: self.status(chat).await?.folder.ok_or_else(|| {
                    AccessError::new(
                        AccessErrorCode::Unsupported,
                        "this Agent has no working directory",
                    )
                })?,
                additional,
            },
            ReplayStep::Rule(change) => ConfigurationChange::PermissionChange {
                change,
                expected: self.permissions_view(chat).await?.revision,
            },
            ReplayStep::Change(change) => change,
        })
    }
    /// Whether the Agent `chat` was created for this View's first input but still waits for the
    /// View to attach and apply its staged choices.
    pub(super) fn first_create_pending(&self, chat: Uuid) -> bool {
        matches!(
            &*self.inner.first_create.lock().unwrap(),
            Some(FirstCreateState::Created { chat: pending, .. }) if *pending == chat
        )
    }
    /// Stops tracking the New Agent command `command_id` without resending anything: an unknown or
    /// failed Create gives its draft back, and an unknown or refused choice cancels the choices not
    /// yet applied.
    pub(super) fn acknowledge_first_create(
        &self,
        command_id: Uuid,
    ) -> Option<CreateAcknowledgement> {
        let mut state = self.inner.first_create.lock().unwrap();
        match state.take() {
            Some(
                FirstCreateState::Unconfirmed {
                    command_id: pending,
                    draft,
                    ..
                }
                | FirstCreateState::RegisteredFailed {
                    command_id: pending,
                    draft,
                    ..
                },
            ) if pending == command_id => {
                let mut current = self.inner.draft.lock().unwrap();
                if current.is_empty() {
                    *current = draft;
                }
                Some(CreateAcknowledgement::DraftRestored)
            }
            Some(FirstCreateState::Created {
                blocked:
                    Some(
                        BlockedChoice::Unknown {
                            command_id: pending,
                            ..
                        }
                        | BlockedChoice::Refused {
                            command_id: pending,
                            ..
                        },
                    ),
                ..
            }) if pending == command_id => Some(CreateAcknowledgement::ChoicesCancelled),
            other => {
                *state = other;
                None
            }
        }
    }
    async fn send_create(
        &self,
        epoch: Uuid,
        command_id: Uuid,
        launch: LaunchSpec,
    ) -> CommandOutcome {
        let origin = if self.origin().is_some() {
            if let Err(error) = self.refresh_interaction_source().await {
                return CommandOutcome::Undelivered(AccessError::invalid(error.to_string()));
            }
            self.origin()
        } else {
            None
        };
        let launch_rules = self.inner.launch_rules.lock().unwrap().clone();
        self.send(
            epoch,
            command_id,
            AgentCommand::Create {
                launch_rules,
                launch,
                initial: None,
                origin,
            },
        )
        .await
    }
    /// The configuration settings menus show for `target`.
    pub(super) async fn configuration(
        &self,
        target: ConfigTarget,
    ) -> Result<AgentConfiguration, AccessError> {
        match target {
            ConfigTarget::Agent(chat) => {
                let mut configuration = AgentConfiguration::of(&self.status(chat).await?);
                let ObservationValue::TaskToolsSetting(enabled) =
                    self.value(chat, AgentDetailQuery::TaskToolsSetting).await?
                else {
                    return Err(unexpected());
                };
                configuration.task_tools_enabled = enabled;
                Ok(configuration)
            }
            ConfigTarget::NewDraft => Ok(self
                .new_agent_configuration(&self.effective_launch().folder)
                .0),
        }
    }
    pub(super) async fn provider_credentials(
        &self,
        target: ConfigTarget,
    ) -> Result<ProviderCredentialScope, AccessError> {
        let ConfigTarget::Agent(chat) = target else {
            return Ok(
                tickloop_harness::model::auth::current_provider_credential_scope(
                    self.inner.launch.config_root.clone(),
                ),
            );
        };
        match self
            .value(chat, AgentDetailQuery::ProviderCredentials)
            .await?
        {
            ObservationValue::ProviderCredentials(scope) => Ok(*scope),
            _ => Err(unexpected()),
        }
    }

    pub(super) async fn test_search(
        &self,
        target: ConfigTarget,
        config: tickloop_harness::protocol::SearchConfig,
    ) -> Result<tickloop_harness::protocol::SearchTestResult, String> {
        let ConfigTarget::Agent(chat) = target else {
            return Ok(tickloop_harness::tools::web_search::test_connection(
                &config,
                self.inner.launch.config_root.as_deref(),
                tokio_util::sync::CancellationToken::new(),
            )
            .await);
        };
        let execution = self
            .reference(chat)
            .await
            .map_err(|error| error.to_string())?;
        let mut observation = self
            .inner
            .access
            .observe(ObservationRequest::Agent {
                execution,
                detail: AgentDetailQuery::TestSearch { config },
                after: None,
            })
            .await
            .map_err(|error| error.to_string())?;
        if !matches!(observation.initial, ObservationValue::SearchTestPending) {
            return Err("Agent returned an invalid search test snapshot".into());
        }
        match observation
            .recv()
            .await
            .map_err(|error| error.to_string())?
            .change
        {
            ObservationChange::SearchTest(result) => Ok(result),
            _ => Err("Agent returned an invalid search test update".into()),
        }
    }

    pub(super) async fn account_models(
        &self,
        target: ConfigTarget,
        config: ModelConfig,
        refresh: bool,
    ) -> Result<tickloop_harness::protocol::AccountModelDirectory, String> {
        let ConfigTarget::Agent(chat) = target else {
            return Err("New Agent has no execution".into());
        };
        let execution = self
            .reference(chat)
            .await
            .map_err(|error| error.to_string())?;
        let mut refresh = refresh;
        loop {
            let mut observation = self
                .inner
                .access
                .observe(ObservationRequest::Agent {
                    execution,
                    detail: AgentDetailQuery::AccountModels {
                        config: Box::new(config.clone()),
                        refresh,
                    },
                    after: None,
                })
                .await
                .map_err(|error| error.to_string())?;
            match observation.initial {
                ObservationValue::AccountModels(result) => return result,
                ObservationValue::AccountModelsPending => {}
                _ => return Err("Agent returned an invalid account model snapshot".into()),
            }
            match observation
                .recv()
                .await
                .map_err(|error| error.to_string())?
                .change
            {
                ObservationChange::AccountModels(result) => return result,
                ObservationChange::Changed | ObservationChange::Resync { .. } => {
                    refresh = false;
                }
                _ => return Err("Agent returned an invalid account model update".into()),
            }
        }
    }

    /// Applies `change` to the visible Agent. For the New Agent, model, effort, Thinking and Task
    /// tools are user defaults saved at once; process-local choices wait for the first input.
    pub(super) async fn change_configuration(
        &self,
        target: ConfigTarget,
        change: ConfigurationChange,
    ) -> Result<ConfigurationOutcome, AccessError> {
        let ConfigTarget::Agent(chat) = target else {
            let current = self.configuration(ConfigTarget::NewDraft).await?;
            if self.save_new_agent_default(&current, &change)? {
                return Ok(ConfigurationOutcome::Applied);
            }
            self.inner.draft.lock().unwrap().stage(&current, change)?;
            return Ok(ConfigurationOutcome::Staged);
        };
        self.ensure_chat_settled(chat)?;
        let selected = match &change {
            ConfigurationChange::Model { config, .. } => Some(config.clone()),
            _ => None,
        };
        let outcome = self.apply_configuration(chat, change).await?;
        if let Some(config) = selected {
            self.select_default_model(config);
        }
        Ok(outcome)
    }
    /// Saves a New Agent choice through the shared user settings, with the runtime's validation
    /// and context-window rule. Task tools require durable settings; other choices may be staged
    /// when this View has no settings to save into.
    fn save_new_agent_default(
        &self,
        current: &AgentConfiguration,
        change: &ConfigurationChange,
    ) -> Result<bool, AccessError> {
        let invalid = |error: &dyn std::fmt::Display| AccessError::invalid(error.to_string());
        // Without settings nothing is saved; the choices then wait for the first input like the
        // process-local ones.
        let Some(settings) = self.user_settings() else {
            if matches!(change, ConfigurationChange::TaskToolsEnabled { .. }) {
                return Err(AccessError::invalid(
                    "cannot save Task tools because the user settings directory is unavailable",
                ));
            }
            return Ok(false);
        };
        let save = |write: &dyn Fn(
            &tickloop_code_settings::UserSettings,
        )
            -> Result<(), tickloop_code_settings::SettingsError>| {
            write(&settings).map_err(|error| invalid(&error))
        };
        match change {
            ConfigurationChange::Model { config, thinking } => {
                let config =
                    tickloop_harness::model::cards::model_for_persistent_selection(config.clone());
                tickloop_harness::model::validate_model_config(&config)
                    .map_err(|error| invalid(&error))?;
                if let Some(effort) = thinking {
                    tickloop_harness::model::validate_reasoning_effort(&config, *effort)
                        .map_err(|error| invalid(&error))?;
                }
                let clear_context_window =
                    !tickloop_harness::model::compatible_private_history(&current.model, &config);
                save(&|settings| match thinking {
                    Some(effort) => {
                        settings.save_model_with_thinking(&config, clear_context_window, *effort)
                    }
                    None => settings.save_model(&config, clear_context_window),
                })?;
                self.select_default_model(config);
            }
            ConfigurationChange::TaskToolsEnabled { enabled } => {
                save(&|settings| settings.save_task_tools_enabled(*enabled))?;
            }
            ConfigurationChange::Search { config } => {
                save(&|settings| settings.save_search_config(config.as_ref()))?;
            }
            ConfigurationChange::ReasoningEffort { effort } => {
                tickloop_harness::model::validate_reasoning_effort(&current.model, *effort)
                    .map_err(|error| invalid(&error))?;
                save(&|settings| settings.save_reasoning_effort(*effort))?;
            }
            ConfigurationChange::ThinkingEnabled { enabled } => {
                tickloop_harness::model::validate_reasoning_effort(
                    &current.model,
                    (!enabled).then_some(ReasoningEffort::Off),
                )
                .map_err(|error| invalid(&error))?;
                let enable_effort = (*enabled
                    && tickloop_harness::model::validate_reasoning_effort(
                        &current.model,
                        Some(ReasoningEffort::On),
                    )
                    .is_ok())
                .then_some(ReasoningEffort::On);
                save(&|settings| {
                    settings.save_thinking_enabled_with_effort(*enabled, enable_effort)
                })?;
            }
            _ => return Ok(false),
        }
        Ok(true)
    }
    /// The saved persistent rules of `project` that a New Agent created there will load.
    pub(super) fn saved_project_rules(
        &self,
        project: &Path,
    ) -> Result<Vec<PermissionRule>, AccessError> {
        let Some(settings) = self.user_settings() else {
            return Ok(Vec::new());
        };
        settings
            .project_rules_with_revision(&rule_project(project)?)
            .map(|(rules, ..)| rules)
            .map_err(|error| AccessError::invalid(error.to_string()))
    }
    /// Saves a persistent rule change for `project` through the shared settings, as changing a
    /// persistent rule on an Agent saves it; no Agent is created. The write succeeds only while the
    /// saved rules are still at `expected`, the revision the user acted on; without one, the
    /// revision read here is the one the single write is checked against.
    pub(super) fn save_project_rule_change(
        &self,
        project: &Path,
        change: &PermissionChange,
        expected: Option<&str>,
    ) -> Result<RuleSave, AccessError> {
        let Some(settings) = self.user_settings() else {
            return Ok(RuleSave::NotSaved);
        };
        let invalid = |error: &dyn std::fmt::Display| AccessError::invalid(error.to_string());
        let project = rule_project(project)?;
        let project = project.as_path();
        let (rules, _, revision) = settings
            .project_rules_with_revision(project)
            .map_err(|error| invalid(&error))?;
        let expected = expected.unwrap_or(&revision);
        let changes = match change {
            PermissionChange::UpsertRule { rule } if rule.persistent => {
                tickloop_code_settings::ProjectRuleChanges {
                    removed: rules
                        .iter()
                        .filter(|saved| saved.id == rule.id)
                        .map(|saved| saved.id)
                        .collect(),
                    added: vec![rule.clone()],
                }
            }
            PermissionChange::RemoveRule { id } if rules.iter().any(|rule| rule.id == *id) => {
                tickloop_code_settings::ProjectRuleChanges {
                    removed: std::collections::HashSet::from([*id]),
                    added: Vec::new(),
                }
            }
            _ => return Ok(RuleSave::NotSaved),
        };
        match settings.update_permission_rules_cas(project, expected, [(project, &changes)]) {
            Ok(()) => Ok(RuleSave::Saved),
            Err(tickloop_code_settings::SettingsError::Conflict) => Ok(RuleSave::Conflict),
            Err(error) => Err(invalid(&error)),
        }
    }
    pub(super) fn use_launch_rules(&self, rules: Vec<PermissionRule>) {
        *self.inner.launch_rules.lock().unwrap() = rules;
    }

    /// Shares the View's settings, so New Agent defaults use its transient Providers and saves.
    pub(super) fn use_user_settings(&self, settings: Option<tickloop_code_settings::UserSettings>) {
        *self.inner.user_settings.lock().unwrap() = settings;
    }
    fn user_settings(&self) -> Option<tickloop_code_settings::UserSettings> {
        self.inner.user_settings.lock().unwrap().clone()
    }
    /// Later new, dispatched and resumed Agents of this View start with the model last chosen
    /// here, replacing the startup choice, as the runtime's own default selection does.
    fn select_default_model(&self, config: ModelConfig) {
        *self.inner.default_model.lock().unwrap() =
            Some(tickloop_harness::model::cards::model_for_persistent_selection(config));
    }
    pub(super) fn set_new_agent_directory(&self, folder: PathBuf) {
        let previous = self.effective_launch().folder;
        if previous == folder {
            return;
        }
        let mut saved = self.user_settings().map(|settings| settings.load());
        let previous_defaults = self.initial_directory_sources(&previous, saved.as_mut());
        let next_defaults = self.initial_directory_sources(&folder, saved.as_mut());
        self.inner
            .draft
            .lock()
            .unwrap()
            .rebase_directories(&previous_defaults, &next_defaults);
        *self.inner.new_agent_directory.lock().unwrap() = Some(folder);
    }

    /// The View's launch with its current New Agent directory and confirmed model selection.
    fn effective_launch(&self) -> LaunchSpec {
        let mut launch = self.inner.launch.clone();
        if let Some(folder) = self.inner.new_agent_directory.lock().unwrap().clone() {
            launch.folder = folder;
        }
        if let Some(model) = self.inner.default_model.lock().unwrap().clone() {
            launch.model = Some(model);
        }
        launch
    }
    pub(super) async fn directory_sources(
        &self,
        target: ConfigTarget,
        folder: &Path,
    ) -> Result<tickloop_harness::protocol::AdditionalDirectorySources, AccessError> {
        if let ConfigTarget::Agent(chat) = target {
            return Ok(self
                .permissions_view(chat)
                .await?
                .additional_directory_sources);
        }
        Ok(self.new_agent_configuration(folder).1)
    }
    /// The New Agent's Permissions view for `project`: saved project rules and Auto policy read
    /// without an Agent, with the staged mode, session rules and directories. A settings file
    /// the Permissions view cannot read fails here the same way.
    pub(super) async fn draft_permissions(
        &self,
        project: &Path,
    ) -> Result<PermissionsPage, AccessError> {
        let project = rule_project(project)?;
        let (configuration, additional_directory_sources) = self.new_agent_configuration(&project);
        let storage =
            |error: tickloop_code_settings::SettingsError| AccessError::storage(error.to_string());
        let settings = self.user_settings();
        let (saved_rules, warnings, project_revision) = match &settings {
            Some(settings) => settings
                .project_rules_with_revision(&project)
                .map_err(storage)?,
            None => (Vec::new(), Vec::new(), String::new()),
        };
        let (auto, auto_revision) = match &settings {
            Some(settings) => settings.auto_settings_with_revision().map_err(storage)?,
            None => (
                tickloop_code_settings::AutoSettings::default(),
                tickloop_code_settings::empty_auto_settings_revision(),
            ),
        };
        let jev_key_status = jev_key_status_in(
            settings
                .as_ref()
                .and_then(tickloop_code_settings::UserSettings::root),
        )
        .map_err(|error| AccessError::storage(error.to_string()))?;
        let rules = saved_rules
            .into_iter()
            .chain(
                self.staged_rules()
                    .into_iter()
                    .filter(|rule| !rule.persistent),
            )
            .collect::<Vec<_>>();
        let content = PermissionsContent {
            execution_mode: configuration.execution_mode,
            chat_model_label: configuration.model.model.clone(),
            preferred_reviewer: auto.reviewer,
            auto_policy: auto.policy_for_directory(&project),
            auto_rule_settings: auto.rules.clone(),
            jev_key_status,
            auto_reviewer_health: None,
            selected_loop_id: None,
            rules: rule_views(
                &rules,
                |_| project.clone(),
                &project,
                configuration.execution_mode,
                &[],
                &[],
                &[],
            )
            .into_iter()
            .chain(launch_rule_views(
                &self.inner.launch_rules.lock().unwrap(),
                configuration.execution_mode,
            ))
            .collect(),
            working_directory: project.clone(),
            additional_directories: configuration.additional_directories,
            additional_directory_sources,
            warnings,
        };
        Ok(PermissionsPage {
            subject: PermissionsSubject::NewDraft {
                project,
                revision: DraftPolicyRevision {
                    project_rules: project_revision,
                    auto_settings: auto_revision,
                },
            },
            content,
        })
    }
    /// A Permissions change on the New Agent screen, read at `seen`. Saved rules and Auto policy
    /// are user settings, written now with compare-and-swap against `seen`; the mode, session
    /// rules and directories only an Agent holds are kept for the first input. No Agent is
    /// created, and a refused change writes and stages nothing.
    pub(super) async fn change_draft_permissions(
        &self,
        project: &Path,
        seen: &DraftPolicyRevision,
        change: PermissionChange,
    ) -> Result<DraftPermissionsChange, DraftPermissionsError> {
        let rejected = DraftPermissionsError::Rejected;
        let invalid = |reason: &str| rejected(AccessError::invalid(reason));
        let current = self.draft_permissions(project).await.map_err(rejected)?;
        let PermissionsSubject::NewDraft { revision, .. } = &current.subject else {
            return Err(invalid("the New Agent view has no Agent"));
        };
        if revision != seen {
            return Err(DraftPermissionsError::Conflict(Box::new(current)));
        }
        let view = &current.content;
        let settings = self.user_settings();
        let written = |result: Result<_, tickloop_code_settings::SettingsError>| match result {
            Ok(_) => Ok(()),
            Err(tickloop_code_settings::SettingsError::Conflict) => Err(None),
            Err(error) => Err(Some(AccessError::storage(error.to_string()))),
        };
        let kept = match change {
            PermissionChange::UpsertRule { rule } if rule.persistent => {
                if rule.loop_id.is_some() {
                    return Err(invalid(
                        "persistent rules apply to every Chat in the project and cannot name a Loop",
                    ));
                }
                validate_rule(&rule).map_err(invalid)?;
                match self
                    .save_project_rule_change(
                        project,
                        &PermissionChange::UpsertRule { rule },
                        Some(&seen.project_rules),
                    )
                    .map_err(rejected)?
                {
                    RuleSave::Saved => false,
                    RuleSave::Conflict => return Err(self.draft_conflict(project).await),
                    RuleSave::NotSaved => {
                        return Err(invalid("saved permission settings are unavailable"));
                    }
                }
            }
            PermissionChange::RemoveRule { id }
                if view
                    .rules
                    .iter()
                    .any(|entry| entry.rule.id == id && entry.rule.persistent) =>
            {
                match self
                    .save_project_rule_change(
                        project,
                        &PermissionChange::RemoveRule { id },
                        Some(&seen.project_rules),
                    )
                    .map_err(rejected)?
                {
                    RuleSave::Saved => false,
                    RuleSave::Conflict => return Err(self.draft_conflict(project).await),
                    RuleSave::NotSaved => {
                        return Err(invalid("saved permission settings are unavailable"));
                    }
                }
            }
            PermissionChange::UpsertRule { rule } => {
                validate_rule(&rule).map_err(invalid)?;
                if rule.loop_id.is_some() {
                    return Err(invalid("The New Agent has no Loop for a scoped rule"));
                }
                self.stage_rule_change(PermissionChange::UpsertRule { rule })
                    .map_err(rejected)?;
                true
            }
            PermissionChange::RemoveRule { id } => {
                self.stage_rule_change(PermissionChange::RemoveRule { id })
                    .map_err(rejected)?;
                true
            }
            PermissionChange::AddDirectory { path } => {
                let path = canonical_directory(if path.is_absolute() {
                    path
                } else {
                    view.working_directory.join(path)
                })
                .map_err(invalid)?;
                if view.additional_directories.contains(&path) {
                    return Err(invalid("directory is already configured"));
                }
                let mut additional = view.additional_directories.clone();
                additional.push(path);
                self.stage_directories(project, additional)
                    .await
                    .map_err(rejected)?;
                true
            }
            PermissionChange::RemoveDirectory { path } => {
                let path = if path.is_absolute() {
                    path
                } else {
                    view.working_directory.join(path)
                };
                let path = path.canonicalize().unwrap_or(path);
                if !view.additional_directories.contains(&path) {
                    return Err(invalid("directory is not configured"));
                }
                let mut additional = view.additional_directories.clone();
                additional.retain(|existing| existing != &path);
                self.stage_directories(project, additional)
                    .await
                    .map_err(rejected)?;
                true
            }
            PermissionChange::SelectReviewer { reviewer } => {
                if view.preferred_reviewer == reviewer {
                    return Err(invalid("reviewer is unchanged"));
                }
                if reviewer == tickloop_harness::protocol::AutoReviewerPreference::Jev
                    && view.jev_key_status == tickloop_harness::protocol::ApiKeyStatus::Missing
                {
                    return Err(invalid("Jev key is missing"));
                }
                let settings =
                    settings.ok_or_else(|| invalid("Auto reviewer settings are unavailable"))?;
                match written(settings.update_auto_reviewer_cas(&seen.auto_settings, reviewer)) {
                    Ok(()) => false,
                    Err(None) => return Err(self.draft_conflict(project).await),
                    Err(Some(error)) => return Err(rejected(error)),
                }
            }
            PermissionChange::ChangeAutoRules { change } => {
                let mut rules = view.auto_rule_settings.clone();
                rules.apply(change).map_err(|error| invalid(&error))?;
                let settings =
                    settings.ok_or_else(|| invalid("Auto rule settings are unavailable"))?;
                match written(settings.update_auto_rules_cas(&seen.auto_settings, &rules)) {
                    Ok(()) => false,
                    Err(None) => return Err(self.draft_conflict(project).await),
                    Err(Some(error)) => return Err(rejected(error)),
                }
            }
        };
        let page = self.draft_permissions(project).await.map_err(rejected)?;
        Ok(DraftPermissionsChange { page, kept })
    }
    async fn draft_conflict(&self, project: &Path) -> DraftPermissionsError {
        match self.draft_permissions(project).await {
            Ok(latest) => DraftPermissionsError::Conflict(Box::new(latest)),
            Err(error) => DraftPermissionsError::Rejected(error),
        }
    }
    async fn stage_directories(
        &self,
        project: &Path,
        additional: Vec<PathBuf>,
    ) -> Result<(), AccessError> {
        let current = self.configuration(ConfigTarget::NewDraft).await?;
        self.inner.draft.lock().unwrap().stage(
            &current,
            ConfigurationChange::Directories {
                folder: rule_project(project)?,
                additional,
            },
        )
    }
    pub(super) fn stage_rule_change(&self, change: PermissionChange) -> Result<(), AccessError> {
        self.inner.draft.lock().unwrap().stage_rule_change(change)
    }
    pub(super) fn staged_rules(&self) -> Vec<PermissionRule> {
        self.inner.draft.lock().unwrap().staged_rules().to_vec()
    }
    async fn apply_configuration(
        &self,
        chat: Uuid,
        change: ConfigurationChange,
    ) -> Result<ConfigurationOutcome, AccessError> {
        let ConfigurationChange::Model { config, thinking } = change else {
            self.configure(chat, change).await?;
            return Ok(ConfigurationOutcome::Applied);
        };
        let change = match thinking {
            Some(effort) => self.set_model_with_thinking(chat, config, effort).await?,
            None => self.set_model(chat, config).await?,
        };
        Ok(match change {
            ModelChange::Applied => ConfigurationOutcome::Applied,
            ModelChange::Pending => ConfigurationOutcome::Pending,
        })
    }
    /// Checks a Skill for the New Agent's first input against the skills its Agent will find, so an
    /// unknown name never creates an Agent.
    pub(super) fn check_new_agent_skill(
        &self,
        workdir: &std::path::Path,
        name: &str,
        arguments: &str,
    ) -> Result<(), tickloop_code_settings::skills::SkillError> {
        tickloop_code_settings::skills::SkillRegistry::discover(
            workdir,
            self.inner.launch.config_root.as_deref(),
        )
        .expand(
            name,
            arguments,
            tickloop_code_settings::skills::SkillCaller::User,
        )
        .and_then(tickloop_harness::protocol::InvokedSkill::try_from)
        .map(|_| ())
    }
    fn initial_directory_sources(
        &self,
        folder: &Path,
        saved: Option<&mut tickloop_code_settings::LoadedSettings>,
    ) -> tickloop_harness::protocol::AdditionalDirectorySources {
        use tickloop_harness::protocol::AdditionalDirectorySource;
        let mut sources = saved
            .map(|saved| saved.additional_directories(folder))
            .unwrap_or_default();
        for path in &self.inner.launch.additional_directories {
            if let Ok(path) = canonical_directory(folder.join(path)) {
                sources
                    .entry(path)
                    .or_default()
                    .insert(AdditionalDirectorySource::LaunchArgument);
            }
        }
        sources
    }

    /// What the worker of the first Create starts with, read the way it will read it.
    fn new_agent_configuration(
        &self,
        folder: &Path,
    ) -> (
        AgentConfiguration,
        tickloop_harness::protocol::AdditionalDirectorySources,
    ) {
        use tickloop_harness::protocol::AdditionalDirectorySource;
        let launch = &self.effective_launch();
        let model = tickloop_harness::model::cards::model_for_persistent_selection(
            tickloop_harness::model::cards::launch_model(launch.model.as_ref()),
        );
        let mut saved = self.user_settings().map(|settings| settings.load());
        let defaults = new_chat_settings(saved.as_mut(), &launch.host_options, &model);
        let mut sources = self.initial_directory_sources(folder, saved.as_mut());
        let base = AgentConfiguration {
            search_config: saved.as_mut().map_or_else(
                tickloop_harness::protocol::SearchConfig::enabled_default,
                |saved| saved.search_config(),
            ),
            model,
            pending_model: None,
            reasoning_effort: defaults.reasoning_effort,
            thinking_enabled: defaults.thinking_enabled,
            task_tools_enabled: defaults.task_tools_enabled,
            execution_mode: defaults.execution_mode,
            additional_directories: sources.keys().cloned().collect(),
            delegated: false,
        };
        let configuration = self.inner.draft.lock().unwrap().configuration(base);
        sources.retain(|path, _| configuration.additional_directories.contains(path));
        for path in &configuration.additional_directories {
            sources
                .entry(path.clone())
                .or_insert_with(|| [AdditionalDirectorySource::Session].into());
        }
        (configuration, sources)
    }
    pub(super) async fn create(
        &self,
        launch: LaunchSpec,
        initial: Option<UserInput>,
    ) -> Result<(Uuid, Option<Receipt>), AccessError> {
        let launch_rules = self.inner.launch_rules.lock().unwrap().clone();
        let result = self
            .command(
                AgentCommand::Create {
                    launch_rules,
                    launch,
                    initial,
                    origin: self.origin(),
                },
                None,
            )
            .await?;
        let CommandResult::Created {
            snapshot,
            submitted,
            ..
        } = result
        else {
            return Err(unexpected());
        };
        self.inner.launch_rules.lock().unwrap().clear();
        let chat = snapshot.conversation.id;
        let execution = snapshot.execution;
        self.inner.cache_snapshot(snapshot, None);
        self.watch(execution).await?;
        Ok((chat.into(), submitted))
    }
    pub(super) async fn accept_created(&self, receipt: CommandReceipt) -> CreationAcceptance {
        let result = match receipt.into_result() {
            Ok(result) => result,
            Err(error) => return CreationAcceptance::Rejected(error),
        };
        let CommandResult::Created {
            snapshot,
            submitted,
            ..
        } = result
        else {
            return CreationAcceptance::Rejected(unexpected());
        };
        let execution = snapshot.execution;
        self.inner.cache_snapshot(snapshot, None);
        match self.watch(execution).await {
            Ok(()) => CreationAcceptance::Attached(execution.agent_id, submitted),
            Err(error) => CreationAcceptance::AttachUnavailable(error),
        }
    }
    pub(super) async fn restore_empty_chat(
        &self,
        chat: Uuid,
        workdir: PathBuf,
    ) -> Result<(), AccessError> {
        let mut launch = self.inner.launch.clone();
        launch.folder = workdir;
        launch.exact_chat = Some(chat);
        self.create(launch, None).await.map(|_| ())
    }
    pub(super) async fn submit(&self, chat: Uuid, text: String) -> Result<Receipt, AccessError> {
        self.submit_with_input_contexts(chat, text, Vec::new(), Vec::new())
            .await
    }
    pub(super) async fn submit_with_input_contexts(
        &self,
        chat: Uuid,
        text: String,
        contexts: Vec<InputContext>,
        paths: Vec<PathBuf>,
    ) -> Result<Receipt, AccessError> {
        let execution = self.reference(chat).await?;
        let result = self
            .command(
                AgentCommand::Submit {
                    execution,
                    input: UserInput::Text {
                        text,
                        contexts,
                        paths,
                    },
                    origin: self.origin(),
                },
                None,
            )
            .await?;
        match result {
            CommandResult::Submitted(receipt) => Ok(receipt),
            _ => Err(unexpected()),
        }
    }
    pub(super) async fn send_family_input(
        &self,
        source_chat: Uuid,
        target: ExecutionRef,
        text: String,
        contexts: Vec<InputContext>,
        paths: Vec<PathBuf>,
        retry_of: Option<Uuid>,
    ) -> Result<(Receipt, String), AccessError> {
        self.refresh_interaction_source().await?;
        let source_execution = self.reference(source_chat).await?;
        let origin = self.origin().ok_or_else(|| {
            AccessError::invalid("directed input needs an authenticated displayed View")
        })?;
        let result = self
            .command(
                AgentCommand::SendFamilyInput {
                    source_execution,
                    target_execution: target,
                    origin,
                    input: UserInput::Text {
                        text,
                        contexts,
                        paths,
                    },
                },
                retry_of,
            )
            .await?;
        match result {
            CommandResult::FamilyInputSent {
                receipt,
                target_handle,
                ..
            } => Ok((receipt, target_handle)),
            _ => Err(unexpected()),
        }
    }

    pub(super) async fn invoke_skill(
        &self,
        chat: Uuid,
        name: &str,
        arguments: &str,
    ) -> Result<Receipt, AccessError> {
        let execution = self.reference(chat).await?;
        match self
            .command(
                AgentCommand::Submit {
                    execution,
                    input: UserInput::Skill {
                        name: name.into(),
                        arguments: arguments.into(),
                    },
                    origin: self.origin(),
                },
                None,
            )
            .await?
        {
            CommandResult::Submitted(receipt) => Ok(receipt),
            _ => Err(unexpected()),
        }
    }
    pub(super) async fn has_skill(&self, chat: Uuid, name: &str) -> Result<bool, AccessError> {
        match self
            .value(
                chat,
                AgentDetailQuery::Skills {
                    name: Some(name.into()),
                },
            )
            .await?
        {
            ObservationValue::Skills {
                available: Some(value),
                ..
            } => Ok(value),
            _ => Err(unexpected()),
        }
    }

    /// Refuses a root-only change for a delegated Agent before any command is sent. Only a Chat
    /// this View has not read yet costs a snapshot.
    async fn preflight(&self, chat: Uuid, change: &ConfigurationChange) -> Result<(), AccessError> {
        let delegated = match self.cached_delegation(chat) {
            Some(delegated) => delegated,
            None => self.status(chat).await?.delegation.is_some(),
        };
        match unavailable_change(delegated, change) {
            Some(reason) => Err(AccessError::invalid(format!("Unavailable: {reason}"))),
            None => Ok(()),
        }
    }
    async fn configure(
        &self,
        chat: Uuid,
        change: ConfigurationChange,
    ) -> Result<CommandResult, AccessError> {
        self.preflight(chat, &change).await?;
        let execution = self.reference(chat).await?;
        self.command(
            AgentCommand::Configure {
                execution,
                change,
                provider: None,
            },
            None,
        )
        .await
    }
    pub(super) async fn set_execution_mode(
        &self,
        chat: Uuid,
        mode: ExecutionMode,
    ) -> Result<(), AccessError> {
        self.configure(chat, ConfigurationChange::ExecutionMode { mode })
            .await
            .map(|_| ())
    }
    #[cfg(test)]
    pub(super) async fn set_permission_rules(
        &self,
        chat: Uuid,
        rules: Vec<tickloop_harness::protocol::PermissionRule>,
    ) -> Result<(), AccessError> {
        self.configure(chat, ConfigurationChange::ReplaceRules { rules })
            .await
            .map(|_| ())
    }
    pub(super) async fn set_directories(
        &self,
        chat: Uuid,
        workdir: PathBuf,
        additional: Vec<PathBuf>,
    ) -> Result<(), AccessError> {
        self.configure(
            chat,
            ConfigurationChange::Directories {
                folder: workdir,
                additional,
            },
        )
        .await
        .map(|_| ())
    }
    pub(super) async fn set_model(
        &self,
        chat: Uuid,
        config: ModelConfig,
    ) -> Result<ModelChange, AccessError> {
        match self
            .configure(
                chat,
                ConfigurationChange::Model {
                    config,
                    thinking: None,
                },
            )
            .await?
        {
            CommandResult::ModelChanged(change) => Ok(change),
            _ => Err(unexpected()),
        }
    }
    pub(super) async fn set_model_with_thinking(
        &self,
        chat: Uuid,
        config: ModelConfig,
        effort: Option<ReasoningEffort>,
    ) -> Result<ModelChange, AccessError> {
        match self
            .configure(
                chat,
                ConfigurationChange::Model {
                    config,
                    thinking: Some(effort),
                },
            )
            .await?
        {
            CommandResult::ModelChanged(change) => Ok(change),
            _ => Err(unexpected()),
        }
    }
    pub(super) async fn generate_auto_environment(
        &self,
        chat: Uuid,
        options: tickloop_harness::protocol::AutoSetupOptions,
    ) -> Result<tickloop_harness::protocol::AutoEnvironmentDraft, AccessError> {
        let execution = self.reference(chat).await?;
        match self
            .command(
                AgentCommand::Configure {
                    execution,
                    change: ConfigurationChange::GenerateAutoEnvironment { options },
                    provider: None,
                },
                None,
            )
            .await?
        {
            CommandResult::AutoEnvironmentDraft(draft) => Ok(draft),
            _ => Err(unexpected()),
        }
    }

    pub(super) async fn permissions_view(
        &self,
        chat: Uuid,
    ) -> Result<PermissionsView, AccessError> {
        match self.value(chat, AgentDetailQuery::Permissions).await? {
            ObservationValue::Permissions(view) => Ok(*view),
            _ => Err(unexpected()),
        }
    }
    pub(super) async fn apply_permission_change(
        &self,
        chat: Uuid,
        change: PermissionChange,
        expected: PermissionsRevision,
    ) -> Result<PermissionsView, AccessError> {
        self.ensure_chat_settled(chat)?;
        let change = ConfigurationChange::PermissionChange { change, expected };
        self.preflight(chat, &change).await?;
        let execution = self.reference(chat).await?;
        let receipt = self
            .inner
            .access
            .execute(CommandEnvelope {
                daemon_epoch: self.daemon_epoch(),
                command_id: tickloop_ids::generate(),
                command: AgentCommand::Configure {
                    execution,
                    change,
                    provider: None,
                },
            })
            .await?;
        if let Some(error) = receipt.error {
            return Err(error);
        }
        match receipt.result {
            Some(CommandResult::Permissions(view)) => Ok(*view),
            _ => Err(unexpected()),
        }
    }
    async fn request(
        &self,
        chat: Uuid,
        id: Uuid,
        kind: PendingRequestKind,
    ) -> Result<RequestRef, AccessError> {
        self.status(chat).await?;
        let statuses = self.inner.statuses.read().unwrap();
        statuses
            .get(&chat)
            .and_then(|status| {
                status
                    .request_refs
                    .iter()
                    .find(|request| request.request_id == id && request.kind == kind)
            })
            .copied()
            .ok_or_else(|| {
                AccessError::invalid(
                    "This request has already been resolved or is no longer current",
                )
            })
    }
    pub(super) async fn answer_permission_with_comment(
        &self,
        chat: Uuid,
        id: Uuid,
        allow: bool,
        scope: PermissionScope,
        comment: Option<String>,
    ) -> Result<(), AccessError> {
        let request = self
            .request(chat, id, PendingRequestKind::Permission)
            .await?;
        self.command(
            AgentCommand::DecidePermission {
                request,
                allow,
                scope,
                comment,
                rule: None,
            },
            None,
        )
        .await
        .map(|_| ())
    }
    pub(super) async fn answer_permission_with_rule(
        &self,
        chat: Uuid,
        id: Uuid,
        specifier: String,
    ) -> Result<(), AccessError> {
        let request = self
            .request(chat, id, PendingRequestKind::Permission)
            .await?;
        self.command(
            AgentCommand::DecidePermission {
                request,
                allow: true,
                scope: PermissionScope::Persistent,
                comment: None,
                rule: Some(specifier),
            },
            None,
        )
        .await
        .map(|_| ())
    }
    pub(super) async fn answer_permission_with_scope(
        &self,
        chat: Uuid,
        id: Uuid,
        allow: bool,
        scope: PermissionScope,
    ) -> Result<(), AccessError> {
        self.answer_permission_with_comment(chat, id, allow, scope, None)
            .await
    }
    #[cfg(test)]
    pub(super) async fn answer_permission(
        &self,
        chat: Uuid,
        id: Uuid,
        allow: bool,
    ) -> Result<(), AccessError> {
        self.answer_permission_with_scope(chat, id, allow, PermissionScope::Once)
            .await
    }
    pub(super) async fn answer_question(
        &self,
        chat: Uuid,
        id: Uuid,
        answers: Vec<QuestionAnswer>,
    ) -> Result<(), AccessError> {
        let request = self.request(chat, id, PendingRequestKind::Question).await?;
        self.command(AgentCommand::AnswerQuestion { request, answers }, None)
            .await
            .map(|_| ())
    }
    pub(super) async fn decline_question(&self, chat: Uuid, id: Uuid) -> Result<(), AccessError> {
        let request = self.request(chat, id, PendingRequestKind::Question).await?;
        self.command(AgentCommand::DeclineQuestion { request }, None)
            .await
            .map(|_| ())
    }

    pub(super) async fn clear_completed_tasks(
        &self,
        chat: Uuid,
        cleanup: super::completed_tasks::CompletedTaskCleanup,
    ) -> Result<(), AccessError> {
        let execution = self.reference(chat).await?;
        match self
            .command(
                AgentCommand::ClearCompletedTasks {
                    execution,
                    root_loop_id: cleanup.root_loop_id,
                    expected_revision: cleanup.expected_revision,
                },
                None,
            )
            .await?
        {
            CommandResult::Done => Ok(()),
            _ => Err(unexpected()),
        }
    }

    async fn loop_command(
        &self,
        chat: Uuid,
        action: LoopAction,
    ) -> Result<CommandResult, AccessError> {
        let execution = self.reference(chat).await?;
        self.command(AgentCommand::ManageLoop { execution, action }, None)
            .await
    }
    pub(super) async fn set_scheduled_task_paused(
        &self,
        chat: Uuid,
        task_id: Uuid,
        paused: bool,
    ) -> Result<(), AccessError> {
        self.loop_command(chat, LoopAction::SetScheduledTaskPaused { task_id, paused })
            .await
            .map(|_| ())
    }
    pub(super) async fn delete_scheduled_task(
        &self,
        chat: Uuid,
        task_id: Uuid,
    ) -> Result<(), AccessError> {
        self.loop_command(chat, LoopAction::DeleteScheduledTask { task_id })
            .await
            .map(|_| ())
    }
    pub(super) async fn stop(&self, chat: Uuid) -> Result<Vec<OperationRecord>, AccessError> {
        match self.loop_command(chat, LoopAction::Stop).await? {
            CommandResult::Operations(value) => Ok(value),
            _ => Err(unexpected()),
        }
    }
    pub(super) async fn interrupt(&self, chat: Uuid) -> Result<Vec<OperationRecord>, AccessError> {
        match self.loop_command(chat, LoopAction::Interrupt).await? {
            CommandResult::Operations(value) => Ok(value),
            _ => Err(unexpected()),
        }
    }
    pub(super) async fn send_now(
        &self,
        chat: Uuid,
        run_id: Uuid,
    ) -> Result<Vec<OperationRecord>, AccessError> {
        match self
            .loop_command(chat, LoopAction::SendNow { run_id })
            .await?
        {
            CommandResult::Operations(value) => Ok(value),
            _ => Err(unexpected()),
        }
    }
    pub(super) async fn disable_event_sources(&self) -> Result<(), AccessError> {
        let root = self.inner.launch.config_root.as_ref();
        for row in self.catalog().await? {
            if row.availability != ExecutionAvailability::Live
                || row.record.as_ref().map(|record| &record.store_root) != root
            {
                continue;
            }
            if !self
                .status(row.chat_id)
                .await?
                .chat_loop
                .event_sources
                .is_empty()
            {
                self.loop_command(row.chat_id, LoopAction::DisableEventSources)
                    .await?;
            }
        }
        Ok(())
    }

    pub(super) async fn approve_plan(
        &self,
        chat: Uuid,
        version: u64,
        mode: ExecutionMode,
    ) -> Result<Option<String>, AccessError> {
        match self
            .loop_command(chat, LoopAction::ApprovePlan { version, mode })
            .await?
        {
            CommandResult::Text(value) => Ok(Some(value)),
            CommandResult::Done => Ok(None),
            _ => Err(unexpected()),
        }
    }
    pub(super) async fn decline_plan(
        &self,
        chat: Uuid,
        version: u64,
        feedback: Option<String>,
    ) -> Result<Option<Receipt>, AccessError> {
        match self
            .loop_command(chat, LoopAction::DeclinePlan { version, feedback })
            .await?
        {
            CommandResult::Submitted(receipt) => Ok(Some(receipt)),
            CommandResult::Done => Ok(None),
            _ => Err(unexpected()),
        }
    }
    pub(super) async fn update_tick_subscription(
        &self,
        chat: Uuid,
        subscription_id: Uuid,
        change: TickSubscriptionChange,
    ) -> Result<(), AccessError> {
        self.loop_command(
            chat,
            LoopAction::UpdateSubscription {
                subscription_id,
                change,
            },
        )
        .await
        .map(|_| ())
    }
    pub(super) async fn tick_send_update(
        &self,
        chat: Uuid,
        instance: tick_protocol::InstanceRef,
    ) -> Result<(), AccessError> {
        self.loop_command(chat, LoopAction::TickUpdate { instance })
            .await
            .map(|_| ())
    }
    pub(super) async fn tick_subscribe(
        &self,
        chat: Uuid,
        instance: tick_protocol::InstanceRef,
    ) -> Result<(), AccessError> {
        self.loop_command(chat, LoopAction::TickManage { instance })
            .await
            .map(|_| ())
    }
    pub(super) async fn add_artifact_tick(
        &self,
        chat: Uuid,
        tick: tickloop_harness::protocol::ArtifactTick,
    ) -> Result<Vec<tickloop_harness::protocol::ArtifactTick>, AccessError> {
        let execution = self.reference(chat).await?;
        match self
            .command(AgentCommand::AddArtifactTick { execution, tick }, None)
            .await?
        {
            CommandResult::ArtifactTicks(ticks) => Ok(ticks),
            _ => Err(unexpected()),
        }
    }
    pub(super) async fn link_artifact(
        &self,
        chat: Uuid,
        tick: tickloop_harness::protocol::ArtifactTick,
        history: tickloop_harness::protocol::ArtifactFeedbackHistory,
    ) -> Result<(), AccessError> {
        let execution = self.reference(chat).await?;
        self.command(
            AgentCommand::LinkArtifact {
                execution,
                tick,
                history,
            },
            None,
        )
        .await
        .map(|_| ())
    }
    pub(super) async fn remove_artifact_tick(
        &self,
        chat: Uuid,
        instance_id: String,
    ) -> Result<Vec<tickloop_harness::protocol::ArtifactTick>, AccessError> {
        let execution = self.reference(chat).await?;
        match self
            .command(
                AgentCommand::RemoveArtifactTick {
                    execution,
                    instance_id,
                },
                None,
            )
            .await?
        {
            CommandResult::ArtifactTicks(ticks) => Ok(ticks),
            _ => Err(unexpected()),
        }
    }
    pub(super) async fn remove_monitor_member(
        &self,
        chat: Uuid,
        instance: tick_protocol::InstanceRef,
    ) -> Result<MonitorMembership, AccessError> {
        let execution = self.reference(chat).await?;
        match self
            .command(
                AgentCommand::RemoveMonitorMember {
                    execution,
                    instance,
                },
                None,
            )
            .await?
        {
            CommandResult::MonitorMembership(membership) => Ok(membership),
            _ => Err(unexpected()),
        }
    }
    pub(super) async fn mark_monitor_handled(
        &self,
        chat: Uuid,
        instance: tick_protocol::InstanceRef,
    ) -> Result<MonitorMembership, AccessError> {
        let execution = self.reference(chat).await?;
        match self
            .command(
                AgentCommand::MarkMonitorHandled {
                    execution,
                    instance,
                },
                None,
            )
            .await?
        {
            CommandResult::MonitorMembership(membership) => Ok(membership),
            _ => Err(unexpected()),
        }
    }
    pub(super) async fn artifact_ticks(
        &self,
        chat: Uuid,
    ) -> Result<Vec<tickloop_harness::protocol::ArtifactTick>, AccessError> {
        let execution = self.reference(chat).await?;
        match self
            .command(AgentCommand::ListArtifactTicks { execution }, None)
            .await?
        {
            CommandResult::ArtifactTicks(ticks) => Ok(ticks),
            _ => Err(unexpected()),
        }
    }
    /// The context the New Agent's first request in `folder` would hold, with the draft's model;
    /// `None` while the draft has no model to start with.
    pub(super) async fn draft_context_breakdown(
        &self,
        folder: PathBuf,
    ) -> Result<Option<(ContextBreakdown, ModelConfig)>, AccessError> {
        let configuration = self.configuration(ConfigTarget::NewDraft).await?;
        if configuration.model.model.trim().is_empty() {
            return Ok(None);
        }
        let mut launch = self.effective_launch();
        launch.folder = folder;
        let draft = tickloop_harness::protocol::ContextDraft {
            model: configuration.model.clone(),
            execution_mode: configuration.execution_mode,
            search_config: configuration.search_config,
            task_tools_enabled: configuration.task_tools_enabled,
        };
        match self
            .inner
            .access
            .observe(ObservationRequest::DraftContextBreakdown {
                launch: Box::new(launch),
                draft: Box::new(draft),
            })
            .await?
            .initial
        {
            ObservationValue::ContextBreakdown(breakdown) => {
                Ok(Some((*breakdown, configuration.model)))
            }
            _ => Err(unexpected()),
        }
    }

    /// The running Tickhost of another build the daemon keeps until the user replaces it.
    pub(super) async fn other_tickhost(
        &self,
    ) -> Result<Option<tickloop_harness::protocol::OtherTickhost>, AccessError> {
        match self
            .inner
            .access
            .observe(ObservationRequest::OtherTickhost)
            .await?
            .initial
        {
            ObservationValue::OtherTickhost(other) => Ok(other),
            _ => Err(unexpected()),
        }
    }

    pub(super) fn supports_runtime_upgrade(&self) -> bool {
        self.inner.upgrades_supported
    }

    pub(super) fn is_handing_off(&self) -> bool {
        self.inner
            .recovery
            .as_ref()
            .is_some_and(RecoveringAgentClient::is_handing_off)
    }

    pub(super) fn upgrade_configuration(&self) -> Result<Option<serde_json::Value>, String> {
        if self.inner.first_create.lock().unwrap().is_some()
            || !self.inner.unconfirmed_inputs.lock().unwrap().is_empty()
        {
            return Ok(None);
        }
        serde_json::to_value(&*self.inner.draft.lock().unwrap())
            .map(Some)
            .map_err(|error| error.to_string())
    }

    pub(super) fn restore_upgrade_configuration(
        &self,
        value: serde_json::Value,
    ) -> Result<(), String> {
        *self.inner.draft.lock().unwrap() =
            serde_json::from_value(value).map_err(|error| error.to_string())?;
        Ok(())
    }

    pub(super) async fn runtime_version(
        &self,
    ) -> Result<tickloop_code_access::RuntimeVersion, AccessError> {
        match self
            .inner
            .access
            .observe(ObservationRequest::RuntimeVersion)
            .await?
            .initial
        {
            ObservationValue::RuntimeVersion(version) => Ok(*version),
            _ => Err(unexpected()),
        }
    }

    pub(super) async fn request_runtime_upgrade(
        &self,
        executable: PathBuf,
    ) -> Result<(), AccessError> {
        self.command(AgentCommand::UpgradeRuntime { executable }, None)
            .await?;
        Ok(())
    }
    pub(super) async fn hosted_ticks(&self, chat: Uuid) -> Result<Vec<HostedMember>, AccessError> {
        let execution = self.reference(chat).await?;
        match self
            .command(AgentCommand::ListHostedTicks { execution }, None)
            .await?
        {
            CommandResult::HostedTicks(members) => Ok(members),
            _ => Err(unexpected()),
        }
    }
    /// Reopens a hosted Tick that no longer runs in its row's place; the result names the new
    /// instance, the Chat's Monitor membership, and why subscriptions asked to follow did not.
    pub(super) async fn reopen_hosted_tick(
        &self,
        chat: Uuid,
        instance: tick_protocol::InstanceRef,
        restore_subscriptions: bool,
    ) -> Result<
        (
            tick_protocol::InstanceRef,
            tickloop_harness::protocol::MonitorMembership,
            Option<String>,
        ),
        AccessError,
    > {
        let execution = self.reference(chat).await?;
        match self
            .command(
                AgentCommand::ReopenHostedTick {
                    execution,
                    instance,
                    restore_subscriptions,
                },
                None,
            )
            .await?
        {
            CommandResult::HostedTickReopened {
                instance,
                membership,
                restore_error,
            } => Ok((instance, membership, restore_error)),
            _ => Err(unexpected()),
        }
    }
    pub(super) async fn close_hosted_tick(
        &self,
        chat: Uuid,
        instance: tick_protocol::InstanceRef,
    ) -> Result<(), AccessError> {
        let execution = self.reference(chat).await?;
        match self
            .command(
                AgentCommand::CloseHostedTick {
                    execution,
                    instance,
                },
                None,
            )
            .await?
        {
            CommandResult::Done => Ok(()),
            _ => Err(unexpected()),
        }
    }
    pub(super) async fn attach_hosted_tick(
        &self,
        chat: Uuid,
        instance: tick_protocol::InstanceRef,
    ) -> Result<tickloop_harness::protocol::HostedViewGrant, AccessError> {
        let execution = self.reference(chat).await?;
        match self
            .command(
                AgentCommand::AttachHostedTick {
                    execution,
                    instance,
                },
                None,
            )
            .await?
        {
            CommandResult::HostedTickView(grant) => Ok(grant),
            _ => Err(unexpected()),
        }
    }
    pub(super) async fn tick_subscriptions(
        &self,
        chat: Uuid,
    ) -> Result<Vec<TickSubscriptionStatus>, AccessError> {
        match self.value(chat, AgentDetailQuery::Subscriptions).await? {
            ObservationValue::Subscriptions(rows) => Ok(rows),
            _ => Err(unexpected()),
        }
    }
    pub(super) async fn request_denial_retry(
        &self,
        chat: Uuid,
        record_id: Uuid,
    ) -> Result<Receipt, AccessError> {
        match self
            .loop_command(chat, LoopAction::RetryDeniedOperation { record_id })
            .await?
        {
            CommandResult::Submitted(value) => Ok(value),
            _ => Err(unexpected()),
        }
    }
    pub(super) async fn approval_history_view(
        &self,
        chat: Uuid,
    ) -> Result<ApprovalHistoryView, AccessError> {
        match self.value(chat, AgentDetailQuery::ApprovalHistory).await? {
            ObservationValue::ApprovalHistory(view) => Ok(view),
            _ => Err(unexpected()),
        }
    }

    pub(super) async fn context_breakdown(
        &self,
        chat: Uuid,
    ) -> Result<ContextBreakdown, AccessError> {
        match self.value(chat, AgentDetailQuery::ContextBreakdown).await? {
            ObservationValue::ContextBreakdown(breakdown) => Ok(*breakdown),
            _ => Err(unexpected()),
        }
    }

    async fn history(
        &self,
        chat: Uuid,
        action: HistoryAction,
        id: Option<Uuid>,
    ) -> Result<CommandResult, AccessError> {
        let execution = self.reference(chat).await?;
        self.command(AgentCommand::EditHistory { execution, action }, id)
            .await
    }
    pub(super) async fn branch_chat(
        &self,
        request: BranchRequest,
    ) -> Result<BranchOutcome, AccessError> {
        match self
            .history(
                request.source_chat_id.into(),
                HistoryAction::Branch { name: request.name },
                Some(request.request_id),
            )
            .await?
        {
            CommandResult::Branch {
                source_chat_id,
                child_chat_id,
                name,
                snapshot,
                session_permissions,
                ..
            } => {
                self.inner.cache_snapshot(snapshot.clone(), None);
                Ok(BranchOutcome {
                    source_chat_id: source_chat_id.into(),
                    child_chat_id: child_chat_id.into(),
                    name,
                    snapshot: snapshot.into_snapshot(),
                    session_permissions,
                })
            }
            _ => Err(unexpected()),
        }
    }
    pub(super) async fn compact_with_instructions(
        &self,
        chat: Uuid,
        instructions: Option<String>,
    ) -> Result<CompactionOutcome, AccessError> {
        match self
            .history(chat, HistoryAction::Compact { instructions }, None)
            .await?
        {
            CommandResult::Compact { summary, snapshot } => {
                self.inner
                    .cache_snapshot(snapshot.clone(), Some(snapshot.execution));
                Ok(CompactionOutcome {
                    summary,
                    snapshot: snapshot.into_snapshot(),
                })
            }
            _ => Err(unexpected()),
        }
    }
    pub(super) async fn summarize_checkpoint(
        &self,
        chat: Uuid,
        request: DirectedSummaryRequest,
    ) -> Result<DirectedSummaryOutcome, AccessError> {
        match self
            .history(chat, HistoryAction::DirectedSummary { request }, None)
            .await?
        {
            CommandResult::DirectedSummary {
                snapshot,
                direction,
                summary,
                guidance,
                messages_summarized,
                draft,
                continuation_error,
            } => {
                self.inner
                    .cache_snapshot(snapshot.clone(), Some(snapshot.execution));
                Ok(DirectedSummaryOutcome {
                    snapshot: snapshot.into_snapshot(),
                    direction,
                    summary,
                    guidance,
                    messages_summarized,
                    draft,
                    continuation_error,
                })
            }
            _ => Err(unexpected()),
        }
    }
    pub(super) async fn rewind(
        &self,
        chat: Uuid,
        message_id: Uuid,
        mode: RewindMode,
    ) -> Result<RewindOutcome, AccessError> {
        match self
            .history(chat, HistoryAction::Rewind { message_id, mode }, None)
            .await?
        {
            CommandResult::Rewind {
                snapshot,
                mode,
                code,
                conversation_restored,
                draft,
            } => {
                self.inner
                    .cache_snapshot(snapshot.clone(), Some(snapshot.execution));
                Ok(RewindOutcome {
                    snapshot: snapshot.into_snapshot(),
                    mode,
                    code,
                    conversation_restored,
                    draft,
                })
            }
            _ => Err(unexpected()),
        }
    }
    /// The conversation without the interrupted run's prompt and that prompt, when the Agent
    /// took it back.
    pub(super) async fn take_back_interrupted_prompt(
        &self,
        chat: Uuid,
        run_id: Uuid,
    ) -> Result<Option<(ChatSnapshot, TakenBackPrompt)>, AccessError> {
        match self
            .history(
                chat,
                HistoryAction::TakeBackInterruptedPrompt { run_id },
                None,
            )
            .await?
        {
            CommandResult::PromptTakenBack { snapshot, draft } => {
                self.inner
                    .cache_snapshot(snapshot.clone(), Some(snapshot.execution));
                Ok(Some((snapshot.into_snapshot(), draft)))
            }
            CommandResult::Done => Ok(None),
            _ => Err(unexpected()),
        }
    }
    pub(super) async fn rewind_file_changes(
        &self,
        chat: Uuid,
        message_id: Uuid,
    ) -> Result<Option<Vec<PathBuf>>, AccessError> {
        match self
            .value(chat, AgentDetailQuery::RewindFiles { message_id })
            .await?
        {
            ObservationValue::RewindFiles(value) => Ok(value),
            _ => Err(unexpected()),
        }
    }
    pub(super) async fn saved_chats(&self, workdir: &Path) -> Result<Vec<SavedChat>, AccessError> {
        let mut records = Vec::new();
        loop {
            let observation = self
                .inner
                .access
                .observe(ObservationRequest::SavedRecords {
                    folder: workdir.into(),
                    store_root: self.inner.launch.config_root.clone(),
                    page: Page {
                        offset: records.len(),
                        limit: MAX_PAGE_LIMIT,
                    },
                })
                .await?;
            let ObservationValue::Records(page) = observation.initial else {
                return Err(unexpected());
            };
            let complete = page.len() < MAX_PAGE_LIMIT;
            records.extend(page);
            if complete {
                return Ok(records);
            }
        }
    }

    async fn load(&self, chat: Uuid, as_new: bool) -> Result<ChatSnapshot, AccessError> {
        if !as_new
            && matches!(
                self.value(chat, AgentDetailQuery::Snapshot).await,
                Ok(ObservationValue::Agent(_))
            )
        {
            return self.snapshot(chat).await;
        }
        let root = self
            .inner
            .launch
            .config_root
            .clone()
            .ok_or_else(|| AccessError::not_found("saved conversations are unavailable"))?;
        let result = self
            .command(
                AgentCommand::LoadRecord {
                    record: TranscriptRef {
                        store_root: root,
                        chat_id: chat,
                    },
                    as_new,
                    launch: Some(self.effective_launch()),
                },
                None,
            )
            .await?;
        match result {
            CommandResult::Loaded { mut snapshot, .. } => {
                self.fill_goal_history(&mut snapshot).await;
                let execution = snapshot.execution;
                self.inner.cache_snapshot(snapshot.clone(), None);
                self.watch(execution).await?;
                Ok(snapshot.into_snapshot())
            }
            _ => Err(unexpected()),
        }
    }
    pub(super) async fn resume_chat(&self, chat: Uuid) -> Result<ChatSnapshot, AccessError> {
        self.load(chat, false).await
    }
    pub(super) async fn resume_as_new_chat(&self, chat: Uuid) -> Result<ChatSnapshot, AccessError> {
        self.load(chat, true).await
    }

    async fn task(&self, chat: Uuid, action: TaskAction) -> Result<CommandResult, AccessError> {
        let execution = self.reference(chat).await?;
        self.command(AgentCommand::ManageTask { execution, action }, None)
            .await
    }
    pub(super) async fn submit_user_shell(
        &self,
        chat: Uuid,
        command: String,
        background: bool,
    ) -> Result<UserShellReceipt, AccessError> {
        match self
            .task(
                chat,
                TaskAction::SubmitShell {
                    command,
                    background,
                },
            )
            .await?
        {
            CommandResult::ShellSubmitted(receipt) => Ok(receipt),
            _ => Err(unexpected()),
        }
    }
    async fn control_task(
        &self,
        chat: Uuid,
        task_id: Uuid,
        origin: TaskOrigin,
        stop: bool,
    ) -> Result<(), AccessError> {
        let execution = self.reference(chat).await?;
        let task = TaskRef {
            execution,
            task_id,
            origin,
        };
        self.task(
            chat,
            if stop {
                TaskAction::Stop { task }
            } else {
                TaskAction::Background { task }
            },
        )
        .await
        .map(|_| ())
    }
    pub(super) async fn background_command(
        &self,
        chat: Uuid,
        id: Uuid,
    ) -> Result<CommandSnapshot, AccessError> {
        self.control_task(chat, id, TaskOrigin::Agent, false)
            .await?;
        self.status(chat)
            .await?
            .commands
            .into_iter()
            .find(|task| task.operation_id == id)
            .ok_or_else(|| AccessError::not_found("command does not exist"))
    }
    pub(super) async fn stop_command(
        &self,
        chat: Uuid,
        id: Uuid,
    ) -> Result<CommandSnapshot, AccessError> {
        self.control_task(chat, id, TaskOrigin::Agent, true).await?;
        self.status(chat)
            .await?
            .commands
            .into_iter()
            .find(|task| task.operation_id == id)
            .ok_or_else(|| AccessError::not_found("command does not exist"))
    }
    pub(super) async fn commands(&self, chat: Uuid) -> Result<Vec<CommandSnapshot>, AccessError> {
        Ok(self.status(chat).await?.commands)
    }
    pub(super) async fn background_user_shell(
        &self,
        chat: Uuid,
        id: Uuid,
    ) -> Result<UserShellRecord, AccessError> {
        self.control_task(chat, id, TaskOrigin::User, false).await?;
        self.status(chat)
            .await?
            .user_shells
            .into_iter()
            .find(|task| task.id == id)
            .ok_or_else(|| AccessError::not_found("Shell does not exist"))
    }
    pub(super) async fn stop_user_shell(
        &self,
        chat: Uuid,
        id: Uuid,
    ) -> Result<UserShellRecord, AccessError> {
        self.control_task(chat, id, TaskOrigin::User, true).await?;
        self.status(chat)
            .await?
            .user_shells
            .into_iter()
            .find(|task| task.id == id)
            .ok_or_else(|| AccessError::not_found("Shell does not exist"))
    }
    pub(super) async fn task_output(
        &self,
        chat: Uuid,
        id: Uuid,
        origin: TaskOrigin,
        offset: u64,
        limit: usize,
    ) -> Result<ShellTaskOutputChunk, AccessError> {
        let execution = self.reference(chat).await?;
        match self
            .value(
                chat,
                AgentDetailQuery::TaskOutput {
                    task: TaskRef {
                        execution,
                        task_id: id,
                        origin,
                    },
                    offset,
                    limit,
                },
            )
            .await?
        {
            ObservationValue::TaskOutput(output) => Ok(output),
            _ => Err(unexpected()),
        }
    }
    pub(super) async fn read_command(
        &self,
        chat: Uuid,
        id: Uuid,
        offset: u64,
        limit: usize,
    ) -> Result<ToolResult, AccessError> {
        self.output_result(chat, id, TaskOrigin::Agent, offset, limit)
            .await
    }
    pub(super) async fn read_user_shell_output(
        &self,
        chat: Uuid,
        id: Uuid,
        offset: u64,
        limit: usize,
    ) -> Result<ToolResult, AccessError> {
        self.output_result(chat, id, TaskOrigin::User, offset, limit)
            .await
    }
    async fn output_result(
        &self,
        chat: Uuid,
        id: Uuid,
        origin: TaskOrigin,
        offset: u64,
        limit: usize,
    ) -> Result<ToolResult, AccessError> {
        let chunk = self.task_output(chat, id, origin, offset, limit).await?;
        Ok(ToolResult {
            content: serde_json::to_string(&chunk)
                .map_err(|error| AccessError::invalid(error.to_string()))?,
            is_error: false,
            output_ref: None,
        })
    }
    pub(super) async fn task_rows(&self, chat: Uuid) -> Result<Vec<ShellTask>, AccessError> {
        match self.value(chat, AgentDetailQuery::Tasks).await? {
            ObservationValue::Tasks(mut tasks) => {
                if tasks.iter().any(|task| task.updated_at_ms.is_none()) {
                    let snapshot = self.status(chat).await?;
                    for task in tasks.iter_mut().filter(|task| task.updated_at_ms.is_none()) {
                        task.updated_at_ms = match task.origin {
                            ShellTaskOrigin::User => snapshot
                                .user_shells
                                .iter()
                                .find(|record| record.id == task.id)
                                .and_then(|record| {
                                    recover_shell_update_time(
                                        record.updated_at_ms,
                                        record.state_since,
                                        record.output_ref.as_deref(),
                                    )
                                }),
                            ShellTaskOrigin::Agent => snapshot
                                .commands
                                .iter()
                                .find(|record| record.operation_id == task.id)
                                .and_then(|record| {
                                    recover_shell_update_time(
                                        record.updated_at_ms,
                                        None,
                                        Some(&record.output_ref),
                                    )
                                }),
                        };
                    }
                }
                Ok(tasks)
            }
            _ => Err(unexpected()),
        }
    }
    pub(super) async fn take_queued_inputs(
        &self,
        chat: Uuid,
        draft_empty: bool,
    ) -> Result<Vec<QueuedInput>, AccessError> {
        match self
            .task(chat, TaskAction::TakeBackQueued { draft_empty })
            .await?
        {
            CommandResult::QueueTaken(inputs) => Ok(inputs),
            _ => Err(unexpected()),
        }
    }
    pub(super) async fn queue_command(
        &self,
        chat: Uuid,
        text: String,
    ) -> Result<Uuid, AccessError> {
        let mut action = super::parse_after_turn(self, chat.into(), &text).await?;
        if let AfterTurnAction::Submit { origin, .. } = &mut action {
            *origin = self.origin();
        }
        let execution = self.reference(chat).await?;
        match self
            .command(AgentCommand::EnqueueAfterTurn { execution, action }, None)
            .await?
        {
            CommandResult::Queued { command_id } => Ok(command_id),
            _ => Err(unexpected()),
        }
    }

    pub(super) async fn list_chats(&self) -> Vec<Box<LiveChatSnapshot>> {
        let mut snapshots = Vec::new();
        if let Ok(rows) = self.catalog().await {
            for row in rows {
                if let Ok(snapshot) = self.live_snapshot(row.chat_id).await {
                    snapshots.push(snapshot);
                }
            }
        }
        snapshots
    }
    pub(super) fn saves_chats(&self) -> bool {
        self.inner.launch.session_persistence
            && self.inner.launch.config_root.as_ref().is_some_and(|root| {
                !tickloop_code_settings::UserSettings::in_root(root).skips_transcripts()
            })
    }
    pub(super) fn user_settings_warnings(&self, chat: Uuid) -> Vec<String> {
        self.inner
            .statuses
            .read()
            .unwrap()
            .get(&chat)
            .map(|status| status.warnings.clone())
            .unwrap_or_default()
    }
    pub(super) fn transcript_load_warnings(&self, chat: Uuid) -> Vec<String> {
        self.user_settings_warnings(chat)
    }
    pub(super) fn transcript_write_error(&self) -> Option<String> {
        self.inner
            .statuses
            .read()
            .unwrap()
            .values()
            .find_map(|status| status.transcript_write_error.clone())
    }
    pub(super) fn loop_trigger_notices(&self) -> Vec<LoopTriggerNotice> {
        self.inner
            .statuses
            .read()
            .unwrap()
            .values()
            .flat_map(|status| status.loop_trigger_notices.clone())
            .collect()
    }
    #[cfg(test)]
    pub(super) async fn flush_transcripts(&self) {
        let _ = self.command(AgentCommand::FlushTranscripts, None).await;
    }
    pub(super) async fn jev_key_status(
        &self,
    ) -> Result<tickloop_harness::protocol::ApiKeyStatus, AccessError> {
        let root = self
            .inner
            .launch
            .config_root
            .as_ref()
            .ok_or_else(|| AccessError::invalid("Jev key storage is unavailable"))?;
        tickloop_harness::model::credentials::api_key_status(
            root,
            tickloop_harness::model::judgment::JEV_API_KEY_ENV,
        )
        .map_err(|error| AccessError::storage(error.to_string()))
    }
    pub(super) async fn save_jev_key(
        &self,
        value: &str,
    ) -> Result<tickloop_harness::protocol::ApiKeyStatus, AccessError> {
        let root = self
            .inner
            .launch
            .config_root
            .as_ref()
            .ok_or_else(|| AccessError::invalid("Jev key storage is unavailable"))?;
        tickloop_harness::model::credentials::save_api_key(
            root,
            tickloop_harness::model::judgment::JEV_API_KEY_ENV,
            value,
        )
        .map_err(|error| AccessError::storage(error.to_string()))?;
        Ok(tickloop_harness::protocol::ApiKeyStatus::Saved)
    }
    /// The Chat's MCP servers as its Worker has them, once it has taken up its configuration.
    pub(super) async fn mcp_servers(
        &self,
        chat: Uuid,
    ) -> Result<Vec<tickloop_harness::protocol::mcp::McpServerStatus>, AccessError> {
        match self.value(chat, AgentDetailQuery::McpServers).await? {
            ObservationValue::McpServers(servers) => Ok(servers),
            _ => Err(unexpected()),
        }
    }

    /// Connects the MCP server `name` a New Agent in `folder` would start, as that Agent would,
    /// and returns its status with the tools it lists; the daemon closes the connection after.
    pub(super) async fn draft_mcp_server(
        &self,
        folder: PathBuf,
        name: &str,
    ) -> Result<tickloop_harness::protocol::mcp::McpServerStatus, AccessError> {
        let mut launch = self.effective_launch();
        launch.folder = folder;
        match self
            .inner
            .access
            .observe(ObservationRequest::DraftMcpServer {
                launch: Box::new(launch),
                name: name.to_owned(),
            })
            .await?
            .initial
        {
            ObservationValue::McpServers(servers) => {
                servers.into_iter().next().ok_or_else(unexpected)
            }
            _ => Err(unexpected()),
        }
    }

    pub(super) async fn reconnect_mcp_server(
        &self,
        chat: Uuid,
        name: &str,
    ) -> Result<(), AccessError> {
        let execution = self.reference(chat).await?;
        self.command(
            AgentCommand::ReconnectMcpServer {
                execution,
                name: name.to_owned(),
            },
            None,
        )
        .await
        .map(|_| ())
    }

    pub(super) async fn refresh_configuration(&self, chat: Uuid) -> Result<(), AccessError> {
        let execution = self.reference(chat).await?;
        self.command(AgentCommand::RefreshConfiguration { execution }, None)
            .await
            .map(|_| ())
    }

    #[cfg(test)]
    pub(super) async fn shutdown(&self) {
        if let Some(access) = &self.inner.in_process_access {
            access.shutdown().await;
        }
    }
}

fn unexpected() -> AccessError {
    AccessError::invalid("Agent daemon returned an unexpected typed result")
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::{Value, json};
    use std::os::unix::fs::MetadataExt;
    use tickloop_code_daemon::serve_listener;
    use tickloop_harness::protocol::ToolDefinition;

    use tickloop_code_access::{DisplayReceipt, HostWorkContext, VIEW_LEASE_SECONDS};

    use tickloop_code_access::test_support::TRANSPORT_LIMITS;
    use tickloop_harness::runtime::host_tools::{
        HostCallScope, HostToolPermission, HostToolProvider, PreparedHostCall,
    };
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    struct ReadInteractionSource;

    #[tokio::test]
    async fn next_terminal_submit_reregisters_a_truly_expired_view_without_replaying_input() {
        let directory = tempfile::tempdir().unwrap();
        let model = Arc::new(tickloop_harness::test_support::ScriptedModel::new(vec![
            tickloop_harness::test_support::text("Accepted once."),
        ]));
        let runtime = tickloop_harness::runtime::Agent::new_embedded(
            tickloop_harness::test_support::config(),
            model.clone(),
        );
        let access = Arc::new(EmbeddedAgentAccess::new(runtime.clone()));
        let epoch = access.daemon_epoch();
        let terminal = ConsoleExecution::from_access_for_test(
            access.clone(),
            epoch,
            ViewRef {
                session_id: Uuid::new_v4(),
                view_id: Uuid::new_v4(),
            },
            LaunchSpec::embedded(directory.path().into()),
            None,
        );
        let chat = terminal.create_chat(directory.path().into()).await.unwrap();
        terminal.display_chat(Some(chat)).await.unwrap();
        let execution = terminal.reference(chat).await.unwrap();
        let old_source = terminal.origin().unwrap();
        // View leases run on Tokio's clock, so the lease expires without waiting it out.
        tokio::time::pause();
        tokio::time::advance(std::time::Duration::from_secs(VIEW_LEASE_SECONDS + 1)).await;
        tokio::time::resume();
        let rejected = access
            .execute(CommandEnvelope {
                daemon_epoch: epoch,
                command_id: Uuid::new_v4(),
                command: AgentCommand::Submit {
                    execution,
                    input: UserInput::Text {
                        text: "Expired input must not execute.".into(),
                        contexts: Vec::new(),
                        paths: Vec::new(),
                    },
                    origin: Some(old_source.clone()),
                },
            })
            .await
            .unwrap();
        assert_eq!(
            rejected.error.unwrap().code,
            AccessErrorCode::VersionConflict
        );
        assert!(model.requests.lock().unwrap().is_empty());
        assert!(
            runtime
                .snapshot(chat.into())
                .await
                .unwrap()
                .messages
                .is_empty()
        );

        let mut events = runtime.subscribe();
        let accepted = terminal
            .submit(chat, "Renew and accept this input once.".into())
            .await
            .unwrap();
        loop {
            let event = tickloop_harness::test_support::next_event(&mut events).await;
            if event.run_id != accepted.run_id {
                continue;
            }
            match event.kind {
                tickloop_harness::protocol::EventKind::Completed { .. } => break,
                tickloop_harness::protocol::EventKind::Failed(error) => {
                    panic!("renewed submission failed: {error}")
                }
                _ => {}
            }
        }
        let renewed_source = terminal.origin().unwrap();
        assert_eq!(renewed_source.view, old_source.view);
        assert_eq!(
            renewed_source.display_version,
            old_source.display_version + 1
        );
        assert_eq!(terminal.reference(chat).await.unwrap(), execution);
        assert_eq!(terminal.daemon_epoch(), epoch);
        assert_eq!(accepted.chat_id, chat);
        assert_eq!(model.requests.lock().unwrap().len(), 1);
        let snapshot = runtime.snapshot(chat.into()).await.unwrap();
        assert_eq!(snapshot.chat_loop.id, accepted.loop_id);
        assert_eq!(
            snapshot
                .events
                .iter()
                .filter(|event| matches!(
                    event.kind,
                    tickloop_harness::protocol::EventKind::Started
                ))
                .count(),
            1
        );
        let observation = access
            .observe(ObservationRequest::Overview {
                filter: AgentFilter::default(),
                page: Page::default(),
                after: None,
            })
            .await
            .unwrap();
        let ObservationValue::Overview(rows) = observation.initial else {
            panic!("expected catalog");
        };
        let displayed = &rows
            .iter()
            .find(|row| row.execution == execution)
            .unwrap()
            .views;
        assert_eq!(displayed.len(), 1);
        assert_eq!(displayed[0].view, old_source.view);
        assert_eq!(displayed[0].version, renewed_source.display_version);
        terminal.leave_view().await.unwrap();
        access.shutdown().await;
    }

    #[async_trait]
    impl HostToolProvider for ReadInteractionSource {
        fn definitions(&self) -> Vec<ToolDefinition> {
            vec![ToolDefinition {
                name: "ReadInteractionSource".into(),
                description: "Read the context captured for this host call.".into(),
                input_schema: json!({"type":"object","properties":{}}),
            }]
        }

        fn permission(
            &self,
            _: tickloop_harness::protocol::ChatId,
            _: &str,
            _: &Value,
        ) -> HostToolPermission {
            HostToolPermission::ReadOnly
        }

        async fn prepare_call_in_scope(
            &self,
            chat: tickloop_harness::protocol::ChatId,
            scope: &HostCallScope<'_>,
            name: &str,
            input: &Value,
            cancel: CancellationToken,
        ) -> Result<PreparedHostCall, String> {
            let Some(context) = scope.context else {
                return self.prepare_call(chat, name, input, cancel).await;
            };
            Ok(PreparedHostCall::new(
                name.into(),
                input.clone(),
                serde_json::to_value(context).unwrap(),
                HostToolPermission::ReadOnly,
                "Read interaction source".into(),
            ))
        }

        async fn execute(
            &self,
            _: tickloop_harness::protocol::ChatId,
            _: &str,
            input: Value,
            _: CancellationToken,
        ) -> ToolResult {
            ToolResult {
                content: input.to_string(),
                is_error: false,
                output_ref: None,
            }
        }
    }

    #[tokio::test]
    async fn next_terminal_input_refreshes_a_moved_view_without_changing_agent_work_context() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let socket = directory.path().join("host.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let pane = Arc::new(tokio::sync::Mutex::new(tickhost_connect::herdr::Pane {
            pane_id: "source-pane".into(),
            terminal_id: "source-terminal".into(),
            workspace_id: "workspace-first".into(),
            tab_id: "tab-first".into(),
            cwd: None,
            foreground_cwd: None,
            tokens: Default::default(),
        }));
        let served_pane = pane.clone();
        let host = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let (reader, mut writer) = stream.into_split();
                let mut line = String::new();
                BufReader::new(reader).read_line(&mut line).await.unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(request["method"], "pane.current");
                let mut response = serde_json::to_vec(&json!({
                    "id":request["id"],"result":{"pane":served_pane.lock().await.clone()}
                }))
                .unwrap();
                response.push(b'\n');
                writer.write_all(&response).await.unwrap();
            }
        });
        let model = Arc::new(tickloop_harness::test_support::ScriptedModel::new(vec![
            tickloop_harness::test_support::call("source", "ReadInteractionSource", json!({})),
            tickloop_harness::test_support::text("Read the new input source."),
        ]));
        let runtime = tickloop_harness::runtime::Agent::new_embedded(
            tickloop_harness::test_support::config(),
            model,
        );
        runtime
            .install_host_tools(Arc::new(ReadInteractionSource))
            .unwrap();
        let access = Arc::new(EmbeddedAgentAccess::new(runtime.clone()));
        let epoch = access.daemon_epoch();
        let metadata = std::fs::symlink_metadata(&socket).unwrap();
        let mut launch = LaunchSpec::embedded(directory.path().into());
        launch.host_work_context = Some(HostWorkContext {
            socket: socket.clone(),
            workspace: Some("task-workspace".into()),
        });
        let terminal = ConsoleExecution::from_access(
            access,
            epoch,
            ViewRef {
                session_id: Uuid::new_v4(),
                view_id: Uuid::new_v4(),
            },
            launch,
            Some(HostLocation {
                host_instance: format!("herdr:{}:{}", metadata.dev(), metadata.ino()),
                socket,
                workspace: Some("workspace-first".into()),
                tab: Some("tab-first".into()),
                pane: Some("source-pane".into()),
                terminal: Some("source-terminal".into()),
                client: Some("source-client".into()),
            }),
        );
        let chat = terminal.create_chat(directory.path().into()).await.unwrap();
        terminal.display_chat(Some(chat)).await.unwrap();
        let previous_source = terminal.origin().unwrap();
        {
            let mut current = pane.lock().await;
            current.workspace_id = "workspace-moved".into();
            current.tab_id = "tab-moved".into();
        }
        let mut events = runtime.subscribe();
        terminal
            .submit(chat, "Read this input's host source.".into())
            .await
            .unwrap();
        let result = loop {
            let event = tickloop_harness::test_support::next_event(&mut events).await;
            match event.kind {
                tickloop_harness::protocol::EventKind::ToolCompleted {
                    call_id, result, ..
                } if call_id == "source" => {
                    break result;
                }
                tickloop_harness::protocol::EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        };
        assert!(!result.is_error, "{}", result.content);
        let captured: Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(captured["workspace_id"], "task-workspace");
        assert_eq!(
            captured["interaction"]["pane"]["workspace_id"],
            "workspace-moved"
        );
        assert_eq!(captured["interaction"]["pane"]["tab_id"], "tab-moved");
        assert_eq!(
            terminal.snapshot(chat).await.unwrap().folder,
            Some(directory.path().canonicalize().unwrap())
        );
        let current_source = terminal.origin().unwrap();
        assert_eq!(
            current_source.display_version,
            previous_source.display_version + 1
        );
        assert_eq!(
            current_source.host.unwrap().client,
            Some("source-client".into())
        );
        terminal.leave_view().await.unwrap();
        runtime.shutdown().await;
        host.abort();
    }

    #[tokio::test]
    async fn an_oversized_input_rejected_before_send_is_not_an_unknown_submission() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let socket = directory.path().join("a.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let model = Arc::new(tickloop_harness::test_support::ScriptedModel::new(vec![
            tickloop_harness::test_support::text("Explicit correction worked."),
        ]));
        let runtime = tickloop_harness::runtime::Agent::new_embedded(
            tickloop_harness::test_support::config(),
            model.clone(),
        );
        let access = Arc::new(EmbeddedAgentAccess::new(runtime.clone()));
        let epoch = access.daemon_epoch();
        let server = tokio::spawn(async move {
            serve_listener(listener, access, epoch, TRANSPORT_LIMITS)
                .await
                .unwrap();
        });
        let client = Arc::new(LocalAgentClient::connect(socket).await.unwrap());
        let terminal = ConsoleExecution::from_access(
            client.clone(),
            epoch,
            ViewRef {
                session_id: client.session_id(),
                view_id: Uuid::new_v4(),
            },
            LaunchSpec::embedded(directory.path().into()),
            None,
        );
        let chat = terminal.create_chat(directory.path().into()).await.unwrap();
        let error = terminal
            .submit(chat, "x".repeat(TRANSPORT_LIMITS.frame_bytes()))
            .await
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("frame"), "{message}");
        assert!(!message.contains("unknown acceptance"), "{message}");
        assert!(
            runtime
                .snapshot(chat.into())
                .await
                .unwrap()
                .messages
                .is_empty()
        );
        assert!(model.requests.lock().unwrap().is_empty());
        let mut events = runtime.subscribe();
        terminal
            .submit(chat, "Explicitly correct the oversized input.".into())
            .await
            .unwrap();
        loop {
            match tickloop_harness::test_support::next_event(&mut events)
                .await
                .kind
            {
                tickloop_harness::protocol::EventKind::Completed { .. } => break,
                tickloop_harness::protocol::EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        assert_eq!(model.requests.lock().unwrap().len(), 1);
        terminal.leave_view().await.unwrap();
        runtime.shutdown().await;
        server.abort();
    }

    struct AttachUnavailable {
        actual: EmbeddedAgentAccess,
        unavailable: std::sync::atomic::AtomicBool,
    }

    #[async_trait]
    impl AgentAccess for AttachUnavailable {
        async fn observe(&self, query: ObservationRequest) -> Result<Observation, AccessError> {
            if matches!(query, ObservationRequest::Agent { .. })
                && self.unavailable.load(std::sync::atomic::Ordering::SeqCst)
            {
                return Err(AccessError::unavailable("fixture View cannot attach"));
            }
            self.actual.observe(query).await
        }
        async fn execute(&self, command: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
            self.actual.execute(command).await
        }
        async fn show_view(&self, update: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
            self.actual.show_view(update).await
        }
    }

    #[tokio::test]
    async fn a_created_agent_this_view_cannot_attach_is_distinguished_from_a_rejected_creation() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = tickloop_harness::runtime::Agent::new_embedded(
            tickloop_harness::test_support::config(),
            Arc::new(tickloop_harness::test_support::ScriptedModel::new(vec![])),
        );
        let actual = EmbeddedAgentAccess::new(runtime.clone());
        let epoch = actual.daemon_epoch();
        let view = || ViewRef {
            session_id: Uuid::new_v4(),
            view_id: Uuid::new_v4(),
        };
        let create = |workdir: PathBuf| CommandEnvelope {
            daemon_epoch: epoch,
            command_id: Uuid::new_v4(),
            command: AgentCommand::Create {
                launch_rules: Vec::new(),
                launch: LaunchSpec::embedded(workdir),
                initial: Some(UserInput::Shell {
                    command: "printf created".into(),
                    background: true,
                }),
                origin: None,
            },
        };
        let access = Arc::new(AttachUnavailable {
            actual,
            unavailable: true.into(),
        });
        let terminal = ConsoleExecution::from_access_for_test(
            access.clone(),
            epoch,
            view(),
            LaunchSpec::embedded(directory.path().into()),
            None,
        );
        let created = access
            .execute(create(directory.path().into()))
            .await
            .unwrap();
        assert!(matches!(
            terminal.accept_created(created).await,
            CreationAcceptance::AttachUnavailable(_)
        ));
        let rejected = access
            .execute(create(directory.path().join("missing")))
            .await
            .unwrap();
        assert!(rejected.error.is_some());
        assert!(matches!(
            terminal.accept_created(rejected).await,
            CreationAcceptance::Rejected(_)
        ));
        access
            .unavailable
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let receipt = access
            .execute(create(directory.path().into()))
            .await
            .unwrap();
        assert!(matches!(
            terminal.accept_created(receipt).await,
            CreationAcceptance::Attached(..)
        ));
        runtime.shutdown().await;
    }

    /// Whole-Chat reads and commands, in the order they reached the daemon.
    #[derive(Debug, PartialEq)]
    enum Traffic {
        Snapshot,
        Command,
    }

    /// Records every command this View sends and can refuse Create or one Configure kind.
    struct Recording {
        actual: EmbeddedAgentAccess,
        commands: Mutex<Vec<AgentCommand>>,
        traffic: Mutex<Vec<Traffic>>,
        refuse_create: std::sync::atomic::AtomicBool,
        refuse_execution_mode: std::sync::atomic::AtomicBool,
        /// Agent observations fail, as when the daemon accepted work but this View cannot attach.
        refuse_agent_observation: std::sync::atomic::AtomicBool,
        /// Create and input commands run, but their replies are lost.
        lose_replies: std::sync::atomic::AtomicBool,
        /// Configure commands run, but their replies are lost.
        lose_configure_replies: std::sync::atomic::AtomicBool,
        /// Receipt lookups find nothing yet.
        hide_receipts: std::sync::atomic::AtomicBool,
        /// Create registers the Agent and then fails, as when its host context cannot be set: the
        /// reply and receipt are Failed but name the registered execution.
        fail_after_register: std::sync::atomic::AtomicBool,
    }

    /// Turns a completed Create receipt into the daemon's failed-after-registration receipt.
    fn failed_after_register(receipt: &mut CommandReceipt) {
        if let Some(CommandResult::Created { execution, .. }) = &receipt.result {
            receipt.target = Some(*execution);
            receipt.state = ReceiptState::Failed;
            receipt.result = None;
            receipt.error = Some(AccessError::invalid(
                "fixture host context failed after registration",
            ));
        }
    }

    fn flag(value: &std::sync::atomic::AtomicBool) -> bool {
        value.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn set(value: &std::sync::atomic::AtomicBool, to: bool) {
        value.store(to, std::sync::atomic::Ordering::SeqCst);
    }

    impl Recording {
        fn commands(&self) -> Vec<AgentCommand> {
            self.commands.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl AgentAccess for Recording {
        async fn observe(&self, query: ObservationRequest) -> Result<Observation, AccessError> {
            if matches!(
                query,
                ObservationRequest::Agent {
                    detail: AgentDetailQuery::Snapshot,
                    ..
                }
            ) {
                self.traffic.lock().unwrap().push(Traffic::Snapshot);
            }
            match &query {
                ObservationRequest::Agent { .. } if flag(&self.refuse_agent_observation) => {
                    return Err(AccessError::unavailable("fixture View cannot attach"));
                }
                ObservationRequest::Receipt { .. } if flag(&self.hide_receipts) => {
                    let mut observation = self.actual.observe(query).await?;
                    observation.initial = ObservationValue::Receipt(None);
                    return Ok(observation);
                }
                _ => {}
            }
            let mut observation = self.actual.observe(query).await?;
            if flag(&self.fail_after_register)
                && let ObservationValue::Receipt(Some(receipt)) = &mut observation.initial
            {
                failed_after_register(receipt);
            }
            Ok(observation)
        }
        async fn execute(&self, command: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
            let mut command = command;
            self.commands.lock().unwrap().push(command.command.clone());
            self.traffic.lock().unwrap().push(Traffic::Command);
            let loses_reply = match &command.command {
                AgentCommand::Create { .. } => flag(&self.lose_replies),
                AgentCommand::Configure { .. } => flag(&self.lose_configure_replies),
                command => input_execution(command).is_some() && flag(&self.lose_replies),
            };
            if matches!(&command.command, AgentCommand::Create { .. }) && flag(&self.refuse_create)
            {
                return Err(AccessError::unavailable(
                    "fixture daemon refuses this command",
                ));
            }
            // The daemon refuses a change it cannot apply with a Failed receipt, kept under the
            // same command id; an unusable directory stands in for the refused mode.
            if let AgentCommand::Configure { change, .. } = &mut command.command
                && matches!(change, ConfigurationChange::ExecutionMode { .. })
                && flag(&self.refuse_execution_mode)
            {
                *change = ConfigurationChange::Directories {
                    folder: "/nonexistent/tickloop-refused-mode".into(),
                    additional: Vec::new(),
                };
            }
            let mut receipt = self.actual.execute(command).await?;
            if flag(&self.fail_after_register) {
                failed_after_register(&mut receipt);
            }
            if loses_reply {
                return Err(AccessError::new(
                    AccessErrorCode::OutcomeUnknown,
                    "fixture reply was lost",
                ));
            }
            Ok(receipt)
        }
        async fn show_view(&self, update: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
            self.actual.show_view(update).await
        }
    }

    fn recording_terminal(
        directory: &Path,
    ) -> (
        tickloop_harness::runtime::Agent,
        Arc<Recording>,
        ConsoleExecution,
    ) {
        recording_terminal_with_model(
            directory,
            tickloop_harness::test_support::persisted_config(),
        )
    }

    fn recording_terminal_with_model(
        directory: &Path,
        model: ModelConfig,
    ) -> (
        tickloop_harness::runtime::Agent,
        Arc<Recording>,
        ConsoleExecution,
    ) {
        let runtime = tickloop_harness::runtime::Agent::new_embedded(
            model.clone(),
            Arc::new(tickloop_harness::test_support::ScriptedModel::new(vec![])),
        );
        let actual = EmbeddedAgentAccess::new(runtime.clone());
        let epoch = actual.daemon_epoch();
        let access = Arc::new(Recording {
            actual,
            commands: Mutex::default(),
            traffic: Mutex::default(),
            refuse_create: false.into(),
            refuse_execution_mode: false.into(),
            refuse_agent_observation: false.into(),
            lose_replies: false.into(),
            lose_configure_replies: false.into(),
            hide_receipts: false.into(),
            fail_after_register: false.into(),
        });
        let mut launch = LaunchSpec::embedded(directory.into());
        launch.model = Some(model);
        let terminal = ConsoleExecution::from_access_for_test(
            access.clone(),
            epoch,
            ViewRef {
                session_id: Uuid::new_v4(),
                view_id: Uuid::new_v4(),
            },
            launch,
            None,
        );
        (runtime, access, terminal)
    }

    /// Shift+Tab and the settings menus read the configuration before changing it; the change
    /// then knows from that read whether the Agent is a delegated child, so it reads no second
    /// snapshot of the whole Chat before its command.
    #[tokio::test]
    async fn a_change_after_reading_the_configuration_sends_its_command_without_another_read() {
        let directory = tempfile::tempdir().unwrap();
        let (runtime, access, terminal) = recording_terminal(directory.path());
        let chat = terminal.create_chat(directory.path().into()).await.unwrap();
        let target = ConfigTarget::Agent(chat);
        let mode = terminal
            .configuration(target)
            .await
            .unwrap()
            .execution_mode
            .next_in(&ExecutionMode::CYCLE_ORDER)
            .unwrap();
        let read = access.traffic.lock().unwrap().len();
        terminal
            .change_configuration(target, ConfigurationChange::ExecutionMode { mode })
            .await
            .unwrap();
        assert_eq!(
            access.traffic.lock().unwrap().get(read),
            Some(&Traffic::Command),
            "the change went out without reading the Chat again"
        );
        assert_eq!(
            runtime.snapshot(chat.into()).await.unwrap().policy.mode,
            mode
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn dependent_model_and_effort_choices_replay_in_the_order_they_were_made() {
        use tickloop_harness::model::cards::ModelCatalog;
        let high = Some(ReasoningEffort::High);
        let profiles = ModelCatalog::defaults().profiles();
        let supports_high = |model: &ModelConfig| {
            tickloop_harness::model::validate_reasoning_effort(model, high).is_ok()
        };
        let lacking = profiles
            .iter()
            .find(|model| !supports_high(model))
            .expect("a bundled model without High")
            .clone();
        let mut supporting = profiles.iter().filter(|model| supports_high(model));
        let first = supporting
            .next()
            .expect("a bundled model with High")
            .clone();
        let second = supporting
            .next()
            .expect("another bundled model with High")
            .clone();
        let directory = tempfile::tempdir().unwrap();
        let (runtime, access, terminal) =
            recording_terminal_with_model(directory.path(), lacking.clone());
        for change in [
            ConfigurationChange::Model {
                config: first.clone(),
                thinking: None,
            },
            ConfigurationChange::ReasoningEffort { effort: high },
            ConfigurationChange::Model {
                config: second.clone(),
                thinking: None,
            },
        ] {
            assert!(matches!(
                terminal
                    .change_configuration(ConfigTarget::NewDraft, change)
                    .await,
                Ok(ConfigurationOutcome::Staged)
            ));
        }
        let preview = terminal
            .configuration(ConfigTarget::NewDraft)
            .await
            .unwrap();
        assert_eq!(preview.model.model_id, second.model_id);
        assert_eq!(preview.reasoning_effort, high);

        let Ok(chat) = terminal.create_from_draft(directory.path().into()).await else {
            panic!("each staged choice was valid when it was made");
        };
        let replayed = access
            .commands()
            .into_iter()
            .filter_map(|command| match command {
                AgentCommand::Configure { change, .. } => Some(change),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            matches!(
                replayed.as_slice(),
                [
                    ConfigurationChange::Model { config: a, .. },
                    ConfigurationChange::ReasoningEffort { effort: Some(ReasoningEffort::High) },
                    ConfigurationChange::Model { config: b, .. },
                ] if a.model_id == first.model_id && b.model_id == second.model_id
            ),
            "{replayed:?}"
        );
        let created = runtime.snapshot(chat.into()).await.unwrap();
        assert_eq!(created.model.model_id, second.model_id);
        assert_eq!(created.reasoning_effort, high);
        runtime.shutdown().await;
    }

    fn count(access: &Recording, matches: impl Fn(&AgentCommand) -> bool) -> usize {
        access
            .commands()
            .iter()
            .filter(|command| matches(command))
            .count()
    }

    fn creates(access: &Recording) -> usize {
        count(access, |command| {
            matches!(command, AgentCommand::Create { .. })
        })
    }

    async fn stage_plan(terminal: &ConsoleExecution) {
        terminal
            .change_configuration(
                ConfigTarget::NewDraft,
                ConfigurationChange::ExecutionMode {
                    mode: ExecutionMode::Plan,
                },
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn context_before_the_first_message_counts_the_draft_without_creating_an_agent() {
        let directory = tempfile::tempdir().unwrap();
        let (_runtime, access, terminal) = recording_terminal(directory.path());
        stage_plan(&terminal).await;
        let (breakdown, model) = terminal
            .draft_context_breakdown(directory.path().into())
            .await
            .unwrap()
            .expect("the draft has a model");
        assert_eq!(
            model.model,
            tickloop_harness::test_support::persisted_config().model
        );
        assert!(!breakdown.calibrated);
        let categories = breakdown
            .categories
            .iter()
            .map(|usage| usage.category)
            .collect::<Vec<_>>();
        assert!(categories.contains(&tickloop_harness::protocol::ContextCategory::SystemPrompt));
        assert!(categories.contains(&tickloop_harness::protocol::ContextCategory::SystemTools));
        assert!(!categories.contains(&tickloop_harness::protocol::ContextCategory::Messages));
        assert_eq!(
            breakdown.used_tokens,
            breakdown
                .categories
                .iter()
                .map(|usage| usage.tokens)
                .sum::<u64>()
        );
        assert_eq!(creates(&access), 0, "counting creates no Agent");
    }

    #[tokio::test]
    async fn a_created_agent_that_cannot_be_attached_yet_is_never_created_twice() {
        let directory = tempfile::tempdir().unwrap();
        let (runtime, access, terminal) = recording_terminal(directory.path());
        stage_plan(&terminal).await;
        set(&access.refuse_agent_observation, true);
        let Err(DraftCreationError::AttachUnavailable { chat, .. }) =
            terminal.create_from_draft(directory.path().into()).await
        else {
            panic!("a confirmed Create is reported as created even when attaching fails");
        };
        assert_eq!(runtime.list_chats().await.len(), 1);
        assert!(terminal.first_create_pending(chat));
        set(&access.refuse_agent_observation, false);
        let Ok(attached) = terminal.create_from_draft(directory.path().into()).await else {
            panic!("attaching again finishes the same Agent");
        };
        assert_eq!(attached, chat);
        assert_eq!(creates(&access), 1);
        assert!(!terminal.first_create_pending(chat));
        assert_eq!(
            runtime.snapshot(chat.into()).await.unwrap().policy.mode,
            ExecutionMode::Plan,
            "the staged mode still applies after the late attach"
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn setup_registers_a_selected_draft_when_its_create_finishes() {
        let directory = tempfile::tempdir().unwrap();
        let (runtime, access, terminal) = recording_terminal(directory.path());
        stage_plan(&terminal).await;
        set(&access.refuse_agent_observation, true);
        let Err(DraftCreationError::AttachUnavailable { chat, .. }) =
            terminal.create_from_draft(directory.path().into()).await
        else {
            panic!("expected a registered but unattached draft");
        };
        assert!(terminal.first_create_pending(chat));
        let console = super::super::Console::test_interactive_for_chat(chat);
        let mut menus = super::super::Menus::new(None, Some(directory.path().into()));
        menus.model_setup(&terminal, &console, false).await.unwrap();
        assert!(menus.model_setup_pending());
        assert!(!menus.model_setup_applies_to(Some(chat)));
        set(&access.refuse_agent_observation, false);
        let mut selected = Some(chat.into());
        let attached = super::super::ensure_chat_with_model_setup(
            &console,
            &terminal,
            &mut menus,
            &mut selected,
            directory.path(),
        )
        .await
        .unwrap();
        assert_eq!(attached, chat);
        assert_eq!(creates(&access), 1);
        assert!(!terminal.first_create_pending(chat));
        assert!(menus.model_setup_applies_to(Some(chat)));
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn an_unconfirmed_first_create_blocks_another_create_until_its_receipt_resolves() {
        let directory = tempfile::tempdir().unwrap();
        let (runtime, access, terminal) = recording_terminal(directory.path());
        stage_plan(&terminal).await;
        set(&access.lose_replies, true);
        set(&access.hide_receipts, true);
        assert!(matches!(
            terminal.create_from_draft(directory.path().into()).await,
            Err(DraftCreationError::Unconfirmed(_))
        ));
        set(&access.lose_replies, false);
        assert!(matches!(
            terminal.create_from_draft(directory.path().into()).await,
            Err(DraftCreationError::Unconfirmed(_))
        ));
        assert_eq!(creates(&access), 1, "an unknown Create is never sent again");
        set(&access.hide_receipts, false);
        let Ok(chat) = terminal.create_from_draft(directory.path().into()).await else {
            panic!("the resolved receipt names the Agent the first Create made");
        };
        assert_eq!(creates(&access), 1);
        assert_eq!(
            runtime
                .list_chats()
                .await
                .into_iter()
                .map(|snapshot| snapshot.id)
                .collect::<Vec<_>>(),
            vec![chat]
        );
        assert_eq!(
            runtime.snapshot(chat.into()).await.unwrap().policy.mode,
            ExecutionMode::Plan
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn an_unconfirmed_input_blocks_a_duplicate_until_its_receipt_resolves() {
        let directory = tempfile::tempdir().unwrap();
        let (runtime, access, terminal) = recording_terminal(directory.path());
        let chat = terminal
            .create_from_draft(directory.path().into())
            .await
            .unwrap_or_else(|_| panic!("created"));
        set(&access.lose_replies, true);
        set(&access.hide_receipts, true);
        assert!(terminal.submit(chat, "once".into()).await.is_err());
        set(&access.lose_replies, false);
        assert!(terminal.submit(chat, "once".into()).await.is_err());
        let submits = || {
            count(&access, |command| {
                matches!(command, AgentCommand::Submit { .. })
            })
        };
        assert_eq!(submits(), 1, "an unknown input is never sent again");
        set(&access.hide_receipts, false);
        assert!(terminal.submit(chat, "next".into()).await.is_ok());
        assert_eq!(submits(), 2);
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn new_agent_settings_stay_in_the_view_until_the_first_input_creates_the_agent() {
        let directory = tempfile::tempdir().unwrap();
        // The draft reads its model through the bundled catalog, so the created Agent matches it
        // only when it starts from the catalog model. No input is sent, so no key is read.
        let (runtime, access, terminal) = recording_terminal_with_model(
            directory.path(),
            tickloop_harness::model::cards::ModelCatalog::defaults()
                .get("deepseek-api", "deepseek-flash")
                .unwrap(),
        );
        let defaults = terminal
            .configuration(ConfigTarget::NewDraft)
            .await
            .unwrap();
        assert!(matches!(
            terminal
                .change_configuration(
                    ConfigTarget::NewDraft,
                    ConfigurationChange::ExecutionMode {
                        mode: ExecutionMode::Plan,
                    },
                )
                .await,
            Ok(ConfigurationOutcome::Staged)
        ));
        let staged = terminal
            .configuration(ConfigTarget::NewDraft)
            .await
            .unwrap();
        assert_eq!(staged.execution_mode, ExecutionMode::Plan);
        assert!(access.commands().is_empty());
        assert!(runtime.list_chats().await.is_empty());

        let Ok(chat) = terminal.create_from_draft(directory.path().into()).await else {
            panic!("the first input creates the Agent");
        };
        assert!(matches!(
            access.commands().as_slice(),
            [
                AgentCommand::Create { initial: None, .. },
                AgentCommand::Configure {
                    change: ConfigurationChange::ExecutionMode {
                        mode: ExecutionMode::Plan,
                    },
                    ..
                },
            ]
        ));
        assert_eq!(
            terminal
                .configuration(ConfigTarget::Agent(chat))
                .await
                .unwrap(),
            staged
        );
        assert_eq!(
            terminal
                .configuration(ConfigTarget::NewDraft)
                .await
                .unwrap(),
            defaults
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_daemon_that_refuses_creation_leaves_the_new_agent_draft_unchanged() {
        let directory = tempfile::tempdir().unwrap();
        let (runtime, access, terminal) = recording_terminal(directory.path());
        terminal
            .change_configuration(
                ConfigTarget::NewDraft,
                ConfigurationChange::ExecutionMode {
                    mode: ExecutionMode::Plan,
                },
            )
            .await
            .unwrap();
        access
            .refuse_create
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(
            terminal.create_from_draft(directory.path().into()).await,
            Err(DraftCreationError::NotCreated(_))
        ));
        assert_eq!(
            terminal
                .configuration(ConfigTarget::NewDraft)
                .await
                .unwrap()
                .execution_mode,
            ExecutionMode::Plan
        );
        assert!(runtime.list_chats().await.is_empty());
        runtime.shutdown().await;
    }

    fn submits(access: &Recording) -> usize {
        count(access, |command| {
            matches!(command, AgentCommand::Submit { .. })
        })
    }

    fn configures(access: &Recording) -> usize {
        count(access, |command| {
            matches!(command, AgentCommand::Configure { .. })
        })
    }

    #[tokio::test]
    async fn a_refused_new_agent_choice_keeps_input_away_until_the_rest_are_cancelled() {
        let directory = tempfile::tempdir().unwrap();
        let (runtime, access, terminal) = recording_terminal(directory.path());
        stage_plan(&terminal).await;
        set(&access.refuse_execution_mode, true);
        let Err(DraftCreationError::ChoiceRefused {
            chat, command_id, ..
        }) = terminal.create_from_draft(directory.path().into()).await
        else {
            panic!("the refused mode is reported against the created Agent");
        };
        let snapshot = runtime.snapshot(chat.into()).await.unwrap();
        assert_ne!(snapshot.policy.mode, ExecutionMode::Plan);
        assert!(
            terminal.submit(chat, "too early".into()).await.is_err(),
            "no ordinary input reaches a partially configured Agent"
        );
        assert!(
            terminal
                .change_configuration(
                    ConfigTarget::Agent(chat),
                    ConfigurationChange::ExecutionMode {
                        mode: ExecutionMode::FullAccess,
                    },
                )
                .await
                .is_err(),
            "a wider mode cannot slip in before the staged choices settle"
        );
        set(&access.refuse_execution_mode, false);
        assert!(matches!(
            terminal.create_from_draft(directory.path().into()).await,
            Err(DraftCreationError::ChoiceRefused { command_id: again, .. }) if again == command_id
        ));
        assert_eq!(configures(&access), 1, "a refused choice is not retried");
        assert!(matches!(
            terminal.acknowledge_first_create(command_id),
            Some(CreateAcknowledgement::ChoicesCancelled)
        ));
        assert!(terminal.submit(chat, "now".into()).await.is_ok());
        assert_eq!(submits(&access), 1);
        assert_eq!(creates(&access), 1);
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_lost_configure_reply_is_resolved_by_its_receipt_and_the_input_waits_for_every_choice()
     {
        let directory = tempfile::tempdir().unwrap();
        let extra = tempfile::tempdir().unwrap();
        let (runtime, access, terminal) = recording_terminal(directory.path());
        stage_plan(&terminal).await;
        terminal
            .change_configuration(
                ConfigTarget::NewDraft,
                ConfigurationChange::Directories {
                    folder: directory.path().into(),
                    additional: vec![extra.path().into()],
                },
            )
            .await
            .unwrap();
        let Ok(super::super::RuleCommand::Add(rule)) =
            super::super::parse_rule_command("/rule allow Read session")
        else {
            panic!("a session rule");
        };
        terminal
            .stage_rule_change(tickloop_harness::protocol::PermissionChange::UpsertRule {
                rule: rule.clone(),
            })
            .unwrap();
        set(&access.lose_configure_replies, true);
        set(&access.hide_receipts, true);
        let Err(DraftCreationError::ChoiceUnknown { chat, command_id }) =
            terminal.create_from_draft(directory.path().into()).await
        else {
            panic!("a Configure whose reply was lost is unknown, not applied or refused");
        };
        set(&access.lose_configure_replies, false);
        assert!(
            terminal.submit(chat, "too early".into()).await.is_err(),
            "the first input waits until every staged choice is proven"
        );
        assert!(matches!(
            terminal.create_from_draft(directory.path().into()).await,
            Err(DraftCreationError::ChoiceUnknown { command_id: again, .. }) if again == command_id
        ));
        assert_eq!(
            configures(&access),
            1,
            "an unknown Configure is never resent"
        );
        set(&access.hide_receipts, false);
        let Ok(created) = terminal.create_from_draft(directory.path().into()).await else {
            panic!("the late accepted receipt lets the remaining choices go out");
        };
        assert_eq!(created, chat);
        assert!(terminal.submit(chat, "first input".into()).await.is_ok());
        let mode_changes = count(&access, |command| {
            matches!(
                command,
                AgentCommand::Configure {
                    change: ConfigurationChange::ExecutionMode { .. },
                    ..
                }
            )
        });
        let directory_changes = count(&access, |command| {
            matches!(
                command,
                AgentCommand::Configure {
                    change: ConfigurationChange::Directories { .. },
                    ..
                }
            )
        });
        let rule_changes = count(&access, |command| {
            matches!(
                command,
                AgentCommand::Configure {
                    change: ConfigurationChange::PermissionChange { .. },
                    ..
                }
            )
        });
        assert_eq!(
            (
                creates(&access),
                mode_changes,
                directory_changes,
                rule_changes,
                submits(&access)
            ),
            (1, 1, 1, 1, 1)
        );
        let snapshot = runtime.snapshot(chat.into()).await.unwrap();
        assert_eq!(snapshot.policy.mode, ExecutionMode::Plan);
        assert_eq!(
            snapshot.additional_directories,
            vec![extra.path().canonicalize().unwrap()]
        );
        assert!(
            terminal
                .permissions_view(chat)
                .await
                .unwrap()
                .rules
                .iter()
                .any(|view| view.rule.id == rule.id)
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_create_that_failed_after_registering_its_agent_is_never_repeated_until_acknowledged()
    {
        let directory = tempfile::tempdir().unwrap();
        let (runtime, access, terminal) = recording_terminal(directory.path());
        stage_plan(&terminal).await;
        set(&access.fail_after_register, true);
        let Err(DraftCreationError::RegisteredFailed {
            command_id,
            agent_id,
            ..
        }) = terminal.create_from_draft(directory.path().into()).await
        else {
            panic!("a Failed receipt that names an execution is not reported as no Agent");
        };
        set(&access.fail_after_register, false);
        assert!(matches!(
            terminal.create_from_draft(directory.path().into()).await,
            Err(DraftCreationError::RegisteredFailed { agent_id: again, .. }) if again == agent_id
        ));
        assert_eq!(creates(&access), 1);
        assert_eq!(submits(&access) + configures(&access), 0);
        assert!(matches!(
            terminal.acknowledge_first_create(command_id),
            Some(CreateAcknowledgement::DraftRestored)
        ));
        assert_eq!(
            terminal
                .configuration(ConfigTarget::NewDraft)
                .await
                .unwrap()
                .execution_mode,
            ExecutionMode::Plan,
            "acknowledging gives the draft back for a user-authorized next attempt"
        );
        let Ok(chat) = terminal.create_from_draft(directory.path().into()).await else {
            panic!("after the acknowledgement the user's next input creates an Agent");
        };
        assert_eq!(creates(&access), 2);
        assert_eq!(
            runtime.snapshot(chat.into()).await.unwrap().policy.mode,
            ExecutionMode::Plan
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_late_receipt_of_a_create_that_failed_after_registering_is_never_repeated() {
        let directory = tempfile::tempdir().unwrap();
        let (runtime, access, terminal) = recording_terminal(directory.path());
        set(&access.fail_after_register, true);
        set(&access.lose_replies, true);
        set(&access.hide_receipts, true);
        assert!(matches!(
            terminal.create_from_draft(directory.path().into()).await,
            Err(DraftCreationError::Unconfirmed(_))
        ));
        set(&access.lose_replies, false);
        set(&access.hide_receipts, false);
        assert!(matches!(
            terminal.create_from_draft(directory.path().into()).await,
            Err(DraftCreationError::RegisteredFailed { .. })
        ));
        assert!(matches!(
            terminal.create_from_draft(directory.path().into()).await,
            Err(DraftCreationError::RegisteredFailed { .. })
        ));
        assert_eq!(creates(&access), 1);
        runtime.shutdown().await;
    }

    /// Sends one input of each kind whose reply is lost, then shows that its later receipt is the
    /// only thing read: the original acceptance is adopted and nothing is sent twice.
    async fn assert_late_accepted_input_is_adopted(
        kind: &str,
        send: impl AsyncFn(&ConsoleExecution, Uuid) -> Result<(), AccessError>,
        sent: impl Fn(&AgentCommand) -> bool,
        accepted: impl Fn(&CommandResult) -> bool,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let (runtime, access, terminal) = recording_terminal(directory.path());
        let chat = terminal
            .create_from_draft(directory.path().into())
            .await
            .unwrap_or_else(|_| panic!("created"));
        set(&access.lose_replies, true);
        set(&access.hide_receipts, true);
        assert!(send(&terminal, chat).await.is_err(), "{kind}: reply lost");
        set(&access.lose_replies, false);
        let id = terminal
            .unknown_input(chat)
            .unwrap_or_else(|| panic!("{kind}: the unknown input is tracked by its id"));
        assert!(matches!(
            terminal.resolve_input(chat, id).await,
            InputResolution::Unknown
        ));
        assert!(
            terminal.submit(chat, "edited".into()).await.is_err(),
            "{kind}: new input waits while the earlier acceptance is unknown"
        );
        let sent_count = || count(&access, &sent);
        assert_eq!(sent_count(), 1, "{kind}");
        set(&access.hide_receipts, false);
        let InputResolution::Accepted(result) = terminal.resolve_input(chat, id).await else {
            panic!("{kind}: the late receipt proves the original acceptance");
        };
        assert!(accepted(&result), "{kind}: adopted the original result");
        assert_eq!(sent_count(), 1, "{kind}: adopting sends nothing");
        assert!(terminal.unknown_input(chat).is_none());
        assert!(
            terminal.submit(chat, "edited".into()).await.is_ok(),
            "{kind}: an edited input is sent as a new intent after proof"
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_late_accepted_message_is_adopted_instead_of_sent_again() {
        assert_late_accepted_input_is_adopted(
            "message",
            async |terminal, chat| terminal.submit(chat, "once".into()).await.map(|_| ()),
            |command| {
                matches!(
                    command,
                    AgentCommand::Submit {
                        input: UserInput::Text { text, .. },
                        ..
                    } if text == "once"
                )
            },
            |result| matches!(result, CommandResult::Submitted(_)),
        )
        .await;
    }

    #[tokio::test]
    async fn a_late_accepted_skill_is_adopted_instead_of_sent_again() {
        assert_late_accepted_input_is_adopted(
            "skill",
            async |terminal, chat| {
                terminal
                    .invoke_skill(chat, "tickloop", "")
                    .await
                    .map(|_| ())
            },
            |command| {
                matches!(
                    command,
                    AgentCommand::Submit {
                        input: UserInput::Skill { .. },
                        ..
                    }
                )
            },
            |result| matches!(result, CommandResult::Submitted(_)),
        )
        .await;
    }

    #[tokio::test]
    async fn a_late_accepted_queued_input_is_adopted_instead_of_queued_again() {
        assert_late_accepted_input_is_adopted(
            "queued",
            async |terminal, chat| {
                terminal
                    .queue_command(chat, "/tickloop".into())
                    .await
                    .map(|_| ())
            },
            |command| matches!(command, AgentCommand::EnqueueAfterTurn { .. }),
            |result| matches!(result, CommandResult::Queued { .. }),
        )
        .await;
    }

    #[tokio::test]
    async fn a_late_accepted_user_shell_is_adopted_instead_of_run_again() {
        assert_late_accepted_input_is_adopted(
            "shell",
            async |terminal, chat| {
                terminal
                    .submit_user_shell(chat, "true".into(), false)
                    .await
                    .map(|_| ())
            },
            |command| {
                matches!(
                    command,
                    AgentCommand::ManageTask {
                        action: TaskAction::SubmitShell { .. },
                        ..
                    }
                )
            },
            |result| matches!(result, CommandResult::ShellSubmitted(_)),
        )
        .await;
    }

    fn rule(command: &str) -> PermissionRule {
        let Ok(super::super::RuleCommand::Add(rule)) = super::super::parse_rule_command(command)
        else {
            panic!("{command} adds a rule");
        };
        rule
    }

    #[tokio::test]
    async fn new_agent_saved_policy_writes_against_the_seen_revision_and_sends_nothing() {
        use super::super::permissions_surface::PermissionsSubject;
        let directory = tempfile::tempdir().unwrap();
        let (runtime, access, terminal) = recording_terminal(directory.path());
        let config_root = directory.path().canonicalize().unwrap().join("config");
        let settings = tickloop_code_settings::UserSettings::in_root(&config_root);
        terminal.use_user_settings(Some(settings.clone()));
        let project = directory.path().canonicalize().unwrap();
        let saved = |settings: &tickloop_code_settings::UserSettings| {
            settings
                .project_rules_with_revision(&project)
                .unwrap()
                .0
                .into_iter()
                .map(|rule| rule.id)
                .collect::<Vec<_>>()
        };
        let seen = |page: &super::super::permissions_surface::PermissionsPage| match &page.subject {
            PermissionsSubject::NewDraft {
                revision,
                project: shown,
            } => {
                assert_eq!(
                    shown, &project,
                    "the view keeps the project it was opened for"
                );
                revision.clone()
            }
            PermissionsSubject::Agent { .. } => panic!("the New Agent view has no Agent"),
        };
        let opened = seen(&terminal.draft_permissions(directory.path()).await.unwrap());

        let first = rule("/rule allow Read persistent");
        let changed = terminal
            .change_draft_permissions(
                directory.path(),
                &opened,
                tickloop_harness::protocol::PermissionChange::UpsertRule {
                    rule: first.clone(),
                },
            )
            .await
            .unwrap_or_else(|_| panic!("a saved rule is written at once"));
        assert!(!changed.kept);
        assert_eq!(saved(&settings), vec![first.id]);

        let stale = terminal
            .change_draft_permissions(
                directory.path(),
                &opened,
                tickloop_harness::protocol::PermissionChange::UpsertRule {
                    rule: rule("/rule deny Write persistent"),
                },
            )
            .await;
        assert!(
            matches!(stale, Err(DraftPermissionsError::Conflict(_))),
            "a change made against an older view is refused"
        );
        assert_eq!(
            saved(&settings),
            vec![first.id],
            "a refused change writes nothing"
        );

        let current = seen(&changed.page);
        let elsewhere = rule("/rule ask Bash persistent");
        settings
            .update_permission_rules_cas(
                &project,
                &current.project_rules,
                [(
                    project.as_path(),
                    &tickloop_code_settings::ProjectRuleChanges {
                        removed: Default::default(),
                        added: vec![elsewhere.clone()],
                    },
                )],
            )
            .unwrap();
        assert_eq!(
            terminal
                .save_project_rule_change(
                    directory.path(),
                    &tickloop_harness::protocol::PermissionChange::RemoveRule { id: first.id },
                    Some(&current.project_rules),
                )
                .unwrap(),
            RuleSave::Conflict,
            "the write is checked against the revision the user acted on, not a newer one"
        );
        assert_eq!(saved(&settings), vec![first.id, elsewhere.id]);

        let latest = seen(&terminal.draft_permissions(directory.path()).await.unwrap());
        let reviewer = terminal
            .change_draft_permissions(
                directory.path(),
                &latest,
                tickloop_harness::protocol::PermissionChange::SelectReviewer {
                    reviewer: tickloop_harness::protocol::AutoReviewerPreference::Jev,
                },
            )
            .await;
        assert!(matches!(reviewer, Err(DraftPermissionsError::Rejected(_))));
        assert_eq!(
            settings.auto_settings_with_revision().unwrap().0.reviewer,
            tickloop_harness::protocol::AutoReviewerPreference::Llm,
            "Jev without a key changes nothing"
        );

        let session = rule("/rule allow Glob session");
        let kept = terminal
            .change_draft_permissions(
                directory.path(),
                &latest,
                tickloop_harness::protocol::PermissionChange::UpsertRule {
                    rule: session.clone(),
                },
            )
            .await
            .unwrap_or_else(|_| panic!("a session rule is kept for the first input"));
        assert!(kept.kept);
        assert_eq!(saved(&settings), vec![first.id, elsewhere.id]);
        assert!(
            access.commands().is_empty() && runtime.list_chats().await.is_empty(),
            "nothing reaches the daemon before the first input"
        );

        let Ok(chat) = terminal.create_from_draft(directory.path().into()).await else {
            panic!("the first input creates the Agent");
        };
        assert!(
            terminal
                .permissions_view(chat)
                .await
                .unwrap()
                .rules
                .iter()
                .any(|view| view.rule.id == session.id),
            "the kept session rule is replayed on the created Agent"
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn task_tools_settings_reads_the_target_store_without_reconfiguring_the_model() {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().canonicalize().unwrap();
        let agent_root = project.join("agent-settings");
        let view_root = project.join("view-settings");
        let agent_settings = tickloop_code_settings::UserSettings::in_root(&agent_root);
        let view_settings = tickloop_code_settings::UserSettings::in_root(&view_root);
        agent_settings.save_task_tools_enabled(false).unwrap();
        view_settings.save_task_tools_enabled(true).unwrap();
        let model = tickloop_harness::test_support::config();
        let client = Arc::new(tickloop_harness::test_support::ScriptedModel::new(vec![]));
        let runtime = tickloop_harness::runtime::Agent::new_with_user_settings_embedded(
            model.clone(),
            client.clone(),
            agent_settings.clone(),
        );
        let access = Arc::new(EmbeddedAgentAccess::new(runtime.clone()));
        let mut launch = LaunchSpec::embedded(project);
        launch.model = Some(model.clone());
        launch.config_root = Some(agent_root);
        let created = access
            .execute(CommandEnvelope {
                daemon_epoch: access.daemon_epoch(),
                command_id: Uuid::new_v4(),
                command: AgentCommand::Create {
                    launch_rules: Vec::new(),
                    launch: launch.clone(),
                    initial: None,
                    origin: None,
                },
            })
            .await
            .unwrap()
            .into_result()
            .unwrap();
        let CommandResult::Created { snapshot, .. } = created else {
            panic!("Agent was not created");
        };
        let chat = snapshot.conversation.id;
        let before = runtime.snapshot(chat).await.unwrap();
        launch.config_root = Some(view_root);
        let terminal = ConsoleExecution::from_access_for_test(
            access.clone(),
            access.daemon_epoch(),
            ViewRef {
                session_id: Uuid::new_v4(),
                view_id: Uuid::new_v4(),
            },
            launch,
            None,
        );
        terminal.use_user_settings(Some(view_settings.clone()));
        let target = ConfigTarget::Agent(chat.into());
        assert!(
            !terminal
                .configuration(target)
                .await
                .unwrap()
                .task_tools_enabled
        );
        assert!(
            terminal
                .configuration(ConfigTarget::NewDraft)
                .await
                .unwrap()
                .task_tools_enabled
        );

        agent_settings.save_task_tools_enabled(true).unwrap();
        view_settings.save_task_tools_enabled(false).unwrap();
        assert!(
            !terminal
                .status(chat.into())
                .await
                .unwrap()
                .task_tools_enabled,
            "ordinary status does not read the settings store"
        );
        assert!(
            terminal
                .configuration(target)
                .await
                .unwrap()
                .task_tools_enabled,
            "explicit Settings reads a changed target store even while the Agent is idle"
        );
        assert!(
            !terminal
                .configuration(ConfigTarget::NewDraft)
                .await
                .unwrap()
                .task_tools_enabled
        );

        view_settings.save_task_tools_enabled(true).unwrap();
        terminal
            .change_configuration(
                target,
                ConfigurationChange::TaskToolsEnabled { enabled: false },
            )
            .await
            .unwrap();
        assert!(!agent_settings.load().task_tools_enabled());
        assert!(view_settings.load().task_tools_enabled());
        let after = runtime.snapshot(chat).await.unwrap();
        assert_eq!(after.model, before.model);
        assert_eq!(after.pending_model, before.pending_model);
        assert!(after.chat_loop.run_id.is_none());
        assert_eq!(runtime.list_chats().await.len(), 1);
        assert!(client.requests.lock().unwrap().is_empty());
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn a_saved_new_agent_model_is_the_launch_model_for_dispatch_and_the_first_create() {
        use tickloop_harness::model::cards::ModelCatalog;
        let directory = tempfile::tempdir().unwrap();
        let (runtime, access, terminal) = recording_terminal(directory.path());
        let config_root = directory.path().canonicalize().unwrap().join("config");
        terminal.use_user_settings(Some(tickloop_code_settings::UserSettings::in_root(
            &config_root,
        )));
        let initial_model = terminal.launch_spec().model.unwrap().model;
        let chosen = ModelCatalog::defaults()
            .profiles()
            .into_iter()
            .find(|model| model.model != initial_model)
            .expect("another built-in model");
        stage_plan(&terminal).await;
        assert_ne!(
            terminal.launch_spec().model.map(|model| model.model),
            Some(chosen.model.clone())
        );

        terminal
            .change_configuration(
                ConfigTarget::NewDraft,
                ConfigurationChange::Model {
                    config: chosen.clone(),
                    thinking: None,
                },
            )
            .await
            .unwrap_or_else(|error| panic!("the model is saved as the default: {error}"));
        assert!(
            access.commands().is_empty(),
            "saving a default sends nothing"
        );
        let dispatch = terminal.launch_spec().model.expect("a launch model");
        assert_eq!(
            (&dispatch.model, &dispatch.base_url),
            (&chosen.model, &chosen.base_url),
            "overview dispatch uses the confirmed model and its endpoint"
        );

        let Ok(_) = terminal.create_from_draft(directory.path().into()).await else {
            panic!("the first input creates the Agent");
        };
        let commands = access.commands();
        let Some(AgentCommand::Create { launch, .. }) = commands.first() else {
            panic!("the first command is the Create");
        };
        let launched = launch.model.as_ref().expect("the Create names its model");
        assert_eq!(
            (&launched.model, &launched.base_url),
            (&chosen.model, &chosen.base_url)
        );
        assert!(
            !commands.iter().any(|command| matches!(
                command,
                AgentCommand::Configure {
                    change: ConfigurationChange::Model { .. },
                    ..
                }
            )),
            "a saved default is launched, not replayed"
        );
        assert!(commands.iter().any(|command| matches!(
            command,
            AgentCommand::Configure {
                change: ConfigurationChange::ExecutionMode {
                    mode: ExecutionMode::Plan
                },
                ..
            }
        )));
        assert!(
            terminal
                .launch_spec()
                .model
                .is_some_and(|model| model.model == chosen.model),
            "the next dispatch keeps the confirmed model; the staged Plan never entered it"
        );
        runtime.shutdown().await;
    }
}
