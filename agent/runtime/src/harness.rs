//! How a host assembles an Agent: the seams it reaches the outside through, and the options it
//! imposes on every Chat.

use std::{collections::HashMap, sync::Arc, time::Duration};

use tickloop_agent_base::network::Reach;
use tickloop_agent_model::{ModelClient, ModelClients, ModelError};
use tickloop_agent_protocol::{
    AgentHostOptions, ChatId, Decision, LoopId, LoopState, ModelConfig, PermissionRule,
    PromptPolicy, SearchConfig, UserShellState,
};

use uuid::Uuid;

use crate::{
    Agent, AgentError, Chat, environment::ExecutionEnvironment, persistence::ChatRecorder,
    transcript::TranscriptStore,
};

/// How long `quiesce` lets a Chat's title request finish before cancelling it.
const TITLE_SETTLE_LIMIT: Duration = Duration::from_secs(10);
/// How often `quiesce` looks at the Chat again when no event arrives.
const QUIESCE_RECHECK: Duration = Duration::from_millis(100);

pub struct HarnessSeams {
    /// Where the Chats' Transcripts are kept; `None` keeps no Transcript.
    pub transcripts: Option<Arc<dyn TranscriptStore>>,
    /// `None`: the Chats have no Folder. Every local constructor gives a local environment.
    pub environment: Option<Arc<dyn ExecutionEnvironment>>,
    /// Builds the client of each model a Chat selects; the clients share this instance's
    /// private-history provenance.
    pub models: Arc<dyn ModelClients>,
}

pub struct HarnessOptions {
    /// The model new Chats use.
    pub model: ModelConfig,
    /// The user's settings file; `None` reads and writes no settings.
    pub settings: Option<tickloop_agent_config::UserSettings>,
    /// The initial search configuration of every new or restored Chat. Fixed defaults are
    /// reapplied on restore; `set_search` can still change an individual loaded Chat.
    pub search: SearchDefaults,
    pub host: AgentHostOptions,
    pub tools: ToolPolicy,
    /// Permission rules the host imposes on every Chat, at the level of session rules and never
    /// saved.
    pub host_rules: Vec<PermissionRule>,
    /// Task tools when no settings file is supplied; `None` defaults to off.
    /// A settings file takes precedence over this host value.
    pub task_tools: Option<bool>,
    pub waits: WaitHosting,
    pub citation_style: CitationStyle,
    /// Instructions the host adds to every conversation's system prompt, as a section of their
    /// own after the Output Style; `None` adds nothing.
    pub system_prompt_addition: Option<String>,
    /// The permits a request to a model's vendor takes, one of each gate in order, such as one
    /// gate per holding and one per node; it holds them for that single request and its stream
    /// alone. Empty for a local Agent.
    pub request_gates: Vec<Arc<tokio::sync::Semaphore>>,
    /// The IANA time zone scheduled tasks read their cron expressions in: a stateless host
    /// passes the user's. `None` uses the machine's local zone.
    pub time_zone: Option<String>,
}

impl HarnessOptions {
    /// A local Agent's standard composition; hosts override the capabilities they provide.
    pub fn local(model: ModelConfig) -> Self {
        Self {
            model,
            settings: None,
            search: SearchDefaults::default(),
            host: AgentHostOptions::embedded_defaults(),
            tools: ToolPolicy::local(),
            host_rules: Vec::new(),
            task_tools: None,
            waits: WaitHosting::Process,
            citation_style: CitationStyle::default(),
            system_prompt_addition: None,
            request_gates: Vec::new(),
            time_zone: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SearchDefaults {
    /// Read local user settings when supplied; delegated Chats inherit their parent's current
    /// configuration when created. Without settings, restored children keep that configuration.
    #[default]
    UserSettings,
    /// Start every Chat with this configuration, regardless of user settings or its parent.
    /// `None` disables WebSearch.
    Fixed(Option<SearchConfig>),
}

/// How web evidence is cited. Numbered sources keep the same identity across the family.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CitationStyle {
    #[default]
    MarkdownLinks,
    Numbered,
}

/// Who keeps a Run that waits for a person. The waits are recorded either way.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WaitHosting {
    /// This process: a load ends the waits it finds and a shutdown cancels them, as a local
    /// Worker always has.
    #[default]
    Process,
    /// The Transcript: a load rebuilds each suspended Run, and a shutdown releases a suspended
    /// Run without writing anything, so another instance can go on with it.
    Durable,
}

/// Which of the Agent's own capabilities its Chats offer the model, and where WebFetch may reach
/// without a Folder. The file tools, Bash and TaskStop also need a Folder; each capability still
/// follows its own settings, such as Task tools or web search.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolPolicy {
    pub web_fetch: bool,
    pub web_search: bool,
    pub skills: bool,
    /// Whether a user may explicitly invoke a Skill; independent of model loading.
    pub user_skills: bool,
    /// EnterPlanMode and ExitPlanMode.
    pub plan: bool,
    /// AskUserQuestion.
    pub questions: bool,
    /// Subagent, and a child's report back.
    pub delegation: bool,
    /// ListSubagents, SendMessage and StopSubagent.
    pub family_messages: bool,
    pub tasks: bool,
    /// CronCreate, CronList, CronDelete and ScheduleWakeup, for the root Agent only.
    pub scheduled_tasks: bool,
    /// The host's tools, such as the Artifact and Tick tools, and its MCP resources.
    pub artifacts: bool,
    pub tick_subscriptions: bool,
    /// The addresses WebFetch reaches in a Chat without a Folder; a Chat with a Folder reaches
    /// anywhere. Both constructors choose public addresses only.
    pub folderless_reach: Reach,
    /// Whether WebFetch fetches only URLs with provenance: written by the user, or contained in
    /// an earlier search result or fetched page of the Chat. A URL the model composed, which
    /// injected instructions could use to send data out, is refused.
    pub web_fetch_requires_provenance: bool,
    /// Whether ArtifactEdit and ArtifactComment write only to Artifacts with provenance: created
    /// by the Chat's family, named in the user's input, linked to a Loop of the family or shared
    /// through the host. Those are admitted without review; any other target is refused.
    pub artifact_writes_require_provenance: bool,
}

impl ToolPolicy {
    /// Every capability: a local Agent's.
    pub const fn local() -> Self {
        Self {
            web_fetch: true,
            web_search: true,
            skills: true,
            user_skills: true,
            plan: true,
            questions: true,
            delegation: true,
            family_messages: true,
            tasks: true,
            scheduled_tasks: true,
            artifacts: true,
            tick_subscriptions: true,
            folderless_reach: Reach::PublicAddresses,
            web_fetch_requires_provenance: false,
            artifact_writes_require_provenance: false,
        }
    }

    /// A durable Chat's: WebFetch of URLs and Artifact writes to targets with provenance,
    /// questions, Subagents and their messages, Task tools and the host's tools, with waits the
    /// Transcript keeps.
    pub const fn durable_chat() -> Self {
        Self {
            web_fetch: true,
            web_search: false,
            skills: true,
            user_skills: false,
            plan: false,
            questions: true,
            delegation: true,
            family_messages: true,
            tasks: true,
            scheduled_tasks: true,
            artifacts: true,
            tick_subscriptions: false,
            folderless_reach: Reach::PublicAddresses,
            web_fetch_requires_provenance: true,
            artifact_writes_require_provenance: true,
        }
    }

    /// Whether a Chat with these tools may wait for a person or keep work past its turn.
    fn waits_or_outlives_the_turn(&self) -> bool {
        self.questions
            || self.delegation
            || self.family_messages
            || self.tasks
            || self.scheduled_tasks
            || self.artifacts
    }

    /// WebFetch alone: nothing waits for a person, and nothing outlives the turn.
    pub const fn web_fetch_only() -> Self {
        Self {
            web_fetch: true,
            web_search: false,
            skills: false,
            user_skills: false,
            plan: false,
            questions: false,
            delegation: false,
            family_messages: false,
            tasks: false,
            scheduled_tasks: false,
            artifacts: false,
            tick_subscriptions: false,
            folderless_reach: Reach::PublicAddresses,
            web_fetch_requires_provenance: false,
            artifact_writes_require_provenance: false,
        }
    }
}

/// The tools whose calls a durable Chat's host admits by its own rule instead of Auto review.
/// WebFetch fetches only URLs with provenance there, so it carries no data the model composed;
/// Artifact writes are admitted by their target's provenance instead of a rule.
/// Scheduled tasks the Agent creates there wait for the user to confirm them instead.
const DURABLE_CHAT_ALLOWED_TOOLS: [&str; 8] = [
    tickloop_agent_tools::WEB_FETCH_TOOL_NAME,
    tickloop_agent_protocol::SEND_AGENT_MESSAGE_TOOL_NAME,
    crate::agent_messages::STOP_SUBAGENT_TOOL_NAME,
    tickloop_agent_protocol::SUBAGENT_TOOL_NAME,
    crate::mcp_tools::ARTIFACT_READ_TOOL,
    crate::scheduled_tasks::tools::CRON_CREATE_TOOL_NAME,
    crate::scheduled_tasks::tools::CRON_DELETE_TOOL_NAME,
    crate::scheduled_tasks::tools::SCHEDULE_WAKEUP_TOOL_NAME,
];

/// The host rules of a durable Chat, beside [`ToolPolicy::durable_chat`]: each allows one of
/// its tools whose calls Auto would otherwise review. Their ids are fixed, so every host has the
/// same rules.
pub fn durable_chat_host_rules() -> Vec<PermissionRule> {
    DURABLE_CHAT_ALLOWED_TOOLS
        .iter()
        .map(|tool| PermissionRule {
            id: tickloop_agent_transcript::input_ids::host_rule(tool),
            decision: Decision::Allow,
            tool: (*tool).into(),
            specifier: None,
            input: None,
            loop_id: None,
            persistent: false,
        })
        .collect()
}

/// Why the host's composition cannot keep its promises, or `None` when it can.
fn invalid_composition(has_environment: bool, options: &HarnessOptions) -> Option<&'static str> {
    let tools = &options.tools;
    let durable = options.waits == WaitHosting::Durable;
    if !has_environment
        && options.waits == WaitHosting::Process
        && tools.waits_or_outlives_the_turn()
    {
        return Some(
            "Chats without a Folder that ask questions, delegate, keep tasks, or use the host's \
             tools need durable waits",
        );
    }
    if durable && tools.tick_subscriptions {
        return Some("durable waits offer no Tick Subscriptions");
    }
    if durable && !has_environment && tools.plan {
        return Some("durable Chats without a Folder have no plan mode");
    }
    if durable && options.host.prompt_policy != PromptPolicy::DenyOnPrompt {
        return Some("durable waits need a host that denies what would prompt");
    }
    None
}

/// Serves one client for every model the Chats select, so a model switch keeps it: the legacy
/// constructors' Agent built around a single client, such as a test's scripted model.
pub(crate) struct OneClient(pub(crate) Arc<dyn ModelClient>);

impl ModelClients for OneClient {
    fn client(&self, _: &ModelConfig) -> Result<Arc<dyn ModelClient>, ModelError> {
        Ok(self.0.clone())
    }
}

/// What remains of a Chat once `quiesce` returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Quiescence {
    /// Nothing of the Chat runs or waits to run.
    Idle,
    /// Nothing runs, but a Run waits for a person, or a receiving Loop waits for its Tick
    /// Subscriptions or the requests of an artifact it is linked to.
    Waiting(Waits),
}

/// What a quiet Chat waits on, as a host keeps it to know when to come back.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Waits {
    /// The requests that wait for a person, in the order they were made.
    pub people: Vec<PersonWait>,
    /// The artifact links whose @tickloop requests the family's Loops receive: the host
    /// delivers each with `TurnOp::Deliver`.
    pub deliveries: Vec<DeliveryBinding>,
    /// Unix milliseconds when a scheduled task of the family comes due: the host calls
    /// [`Agent::wake_scheduled_tasks`] with this time then. `None` when none is timing or the
    /// Loop pauses receiving.
    pub next_wake_at_ms: Option<u64>,
}

/// A Loop's link to an artifact for one account's @tickloop requests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeliveryBinding {
    pub chat: ChatId,
    pub loop_id: LoopId,
    /// The Artifacts service the artifact belongs to.
    pub origin: String,
    pub account_id: String,
    pub artifact_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PersonWait {
    pub chat: ChatId,
    /// The question's or permission's request id, or the user shell's operation id.
    pub request: Uuid,
    pub kind: PersonWaitKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PersonWaitKind {
    Question,
    /// Waits in this process only.
    Permission,
    /// Waits in this process only.
    UserShell,
}

impl Agent {
    /// An Agent whose Transcripts, Execution Environment, model clients and Chat policies the
    /// host chooses. Without an environment its Chats have no Folder.
    pub fn with_seams(seams: HarnessSeams, options: HarnessOptions) -> Result<Self, AgentError> {
        if let SearchDefaults::Fixed(Some(config)) = &options.search {
            tickloop_agent_tools::web_search::validate_config(config)
                .map_err(AgentError::Invalid)?;
        }
        if seams.environment.is_none() && options.settings.is_some() {
            return Err(AgentError::Invalid(
                "Chats without a Folder read no local user settings".into(),
            ));
        }
        if let Some(reason) = invalid_composition(seams.environment.is_some(), &options) {
            return Err(AgentError::Invalid(reason.into()));
        }
        if let Some(zone) = &options.time_zone {
            crate::scheduled_tasks::resolve_zone(Some(zone)).map_err(AgentError::Invalid)?;
        }
        let client = seams.models.client(
            &tickloop_agent_model::cards::model_for_persistent_selection(options.model.clone()),
        )?;
        let saves_transcripts = options
            .settings
            .as_ref()
            .is_none_or(|settings| !settings.skips_transcripts());
        let recorder = seams
            .transcripts
            .filter(|_| saves_transcripts)
            .map(ChatRecorder::new);
        Ok(Self::build(
            options,
            seams.models,
            client,
            recorder,
            seams.environment,
        ))
    }

    /// Waits until nothing of the Chat's family runs: no member has an active or ready Run that
    /// does not wait, and the title request of each member's submissions that have returned has
    /// ended: one still running after ten seconds is cancelled and awaited. A Run holding its
    /// slot while it waits for a child counts as waiting: the family runs while that child does.
    pub async fn quiesce(&self, chat_id: ChatId) -> Quiescence {
        let mut events = self.subscribe();
        let (quiescence, members) = loop {
            let (state, members) = {
                let chats = self.inner.chats.lock().await;
                let members = family_members(&chats, chat_id);
                (family_state(self, &chats, &members), members)
            };
            match state {
                // Nothing starts once the instance shuts down or hands its families over.
                Some((true, _))
                    if !self.inner.shutdown.is_cancelled()
                        && !self.inner.handover.is_cancelled() => {}
                Some((_, Some(waits))) => break (Quiescence::Waiting(waits), members),
                _ => break (Quiescence::Idle, members),
            }
            tokio::select! {
                _ = events.recv() => {}
                _ = tokio::time::sleep(QUIESCE_RECHECK) => {}
                _ = self.inner.shutdown.cancelled() => {}
            }
        };
        for member in members {
            let title = self.inner.title_requests.lock().await.get(&member).cloned();
            if let Some(title) = title
                && tokio::time::timeout(TITLE_SETTLE_LIMIT, title.done.cancelled())
                    .await
                    .is_err()
            {
                title.cancel.cancel();
                title.done.cancelled().await;
            }
        }
        quiescence
    }
}

/// The Chat and every other member of its family, the root first.
fn family_members(chats: &HashMap<ChatId, Chat>, chat_id: ChatId) -> Vec<ChatId> {
    let Ok(root) = crate::family_tree::root(chats, chat_id) else {
        return vec![chat_id];
    };
    let mut members = vec![root];
    members.extend(crate::family_tree::descendants(chats, root).unwrap_or_default());
    members
}

/// Whether anything of the family runs, and what it waits on; `None` when no member is here.
fn family_state(
    agent: &Agent,
    chats: &HashMap<ChatId, Chat>,
    members: &[ChatId],
) -> Option<(bool, Option<Waits>)> {
    let mut found = false;
    let mut runs = false;
    let mut waits: Option<Waits> = None;
    for member in members {
        let Some(chat) = chats.get(member) else {
            continue;
        };
        found = true;
        let (member_runs, member_waits) = chat_state(agent, *member, chat);
        runs |= member_runs;
        if let Some(member_waits) = member_waits {
            let family_waits = waits.get_or_insert_with(Waits::default);
            family_waits.people.extend(member_waits.people);
            family_waits.deliveries.extend(member_waits.deliveries);
            family_waits.next_wake_at_ms = family_waits
                .next_wake_at_ms
                .into_iter()
                .chain(member_waits.next_wake_at_ms)
                .min();
        }
    }
    found.then_some((runs, waits))
}

fn chat_state(agent: &Agent, chat_id: ChatId, chat: &Chat) -> (bool, Option<Waits>) {
    let user_shell_in = |states: &[UserShellState]| {
        chat.snapshot
            .user_shells
            .iter()
            .any(|record| states.contains(&record.state))
    };
    // A parked Run whose person answered is about to take the execution slot again.
    let waits_for_person = |run: &tickloop_agent_protocol::RunId| {
        chat.snapshot
            .questions
            .iter()
            .any(|request| request.run_id == *run)
            || chat
                .snapshot
                .permissions
                .iter()
                .any(|request| request.execution.run_id() == Some(*run))
    };
    // A Run holding the slot while it waits for a child or its children.
    let waits_for_member = chat.active_run.is_some_and(|run| {
        chat.bookkeeping
            .lifecycle
            .open_wait_of(run, |kind| {
                matches!(
                    kind,
                    tickloop_agent_transcript::WaitKind::Subagent { .. }
                        | tickloop_agent_transcript::WaitKind::Children { .. }
                )
            })
            .is_some()
    });
    let runs = ((chat.active.is_some() || chat.active_run.is_some()) && !waits_for_member)
        || has_startable_input(agent, chat)
        || !chat.family_finishing.is_empty()
        // A Run that ended still records its end with its parent and delivers notifications.
        || !chat.ready_runs.is_empty()
        || chat.parked.keys().any(|run| !waits_for_person(run))
        || user_shell_in(&[UserShellState::Running, UserShellState::Cancelling]);
    let deliveries = deliveries_of(chat_id, &chat.snapshot);
    let next_wake_at_ms = Agent::next_scheduled_wake(chat);
    let waits = !chat.parked.is_empty()
        || !deliveries.is_empty()
        || next_wake_at_ms.is_some()
        || waits_for_member
        || crate::wakeup::listens(&chat.snapshot.chat_loop)
        || user_shell_in(&[UserShellState::WaitingPermission]);
    (
        runs,
        waits.then(|| Waits {
            deliveries,
            next_wake_at_ms,
            ..waits_of(chat_id, &chat.snapshot)
        }),
    )
}

/// Whether an input queued to a Loop that can take it starts once the free execution slot is
/// taken, as between a Run's end and the next Run's start: the Chat still runs then.
fn has_startable_input(agent: &Agent, chat: &Chat) -> bool {
    let snapshot = &chat.snapshot;
    let is_held_back = !snapshot.pending_commands.is_empty()
        || chat.has_foreground_user_shell()
        || snapshot
            .user_shells
            .iter()
            .any(|record| record.state == UserShellState::Queued);
    chat.active.is_none()
        && !is_held_back
        && !matches!(
            snapshot.chat_loop.state,
            LoopState::WaitingPermission | LoopState::WaitingQuestion
        )
        && snapshot.pending_inputs.iter().any(|pending| {
            pending.armed
                && pending.trigger_source.as_ref().is_none_or(|source| {
                    agent.takes_trigger_input(chat, source) == crate::wakeup::Eligibility::Eligible
                })
        })
}

/// The links of the Chat's Loop that receive their artifact's requests: none while it pauses
/// receiving.
fn deliveries_of(
    chat: ChatId,
    snapshot: &tickloop_agent_protocol::ChatSnapshot,
) -> Vec<DeliveryBinding> {
    let chat_loop = &snapshot.chat_loop;
    chat_loop
        .event_sources
        .iter()
        .filter(|source| chat_loop.receiving.is_receiving() && !source.resume_required)
        .map(|source| DeliveryBinding {
            chat,
            loop_id: chat_loop.id,
            origin: source.origin.clone(),
            account_id: source.account_id.clone(),
            artifact_id: source.artifact_id.clone(),
        })
        .collect()
}

fn waits_of(chat: ChatId, snapshot: &tickloop_agent_protocol::ChatSnapshot) -> Waits {
    let questions = snapshot.questions.iter().map(|request| PersonWait {
        chat,
        request: request.id,
        kind: PersonWaitKind::Question,
    });
    let permissions = snapshot.permissions.iter().map(|request| PersonWait {
        chat,
        request: request.id,
        kind: PersonWaitKind::Permission,
    });
    let user_shells = snapshot
        .user_shells
        .iter()
        .filter(|record| record.state == UserShellState::WaitingPermission)
        .map(|record| PersonWait {
            chat,
            request: record.id,
            kind: PersonWaitKind::UserShell,
        });
    Waits {
        people: questions.chain(permissions).chain(user_shells).collect(),
        deliveries: Vec::new(),
        next_wake_at_ms: None,
    }
}

#[cfg(test)]
#[path = "harness_tests.rs"]
mod tests;
