//! How a host assembles an Agent: the seams it reaches the outside through, and the options it
//! imposes on every Chat.

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, SystemTime},
};

use tickloop_agent_base::network::Reach;
use tickloop_agent_model::{ModelClient, ModelClients, ModelError};
use tickloop_agent_protocol::{
    AgentHostOptions, ChatId, ModelConfig, PermissionRule, UserShellState,
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
    pub host: AgentHostOptions,
    pub tools: ToolPolicy,
    /// Permission rules the host imposes on every Chat, at the level of session rules and never
    /// saved.
    pub host_rules: Vec<PermissionRule>,
    /// Task tools when no settings file is supplied; `None` defaults to off.
    /// A settings file takes precedence over this host value.
    pub task_tools: Option<bool>,
    /// The shortest time a Trigger's schedule may leave between two runs; a schedule that runs
    /// more often is refused.
    pub min_schedule_interval: Duration,
    pub waits: WaitHosting,
}

/// A local Agent's floor for Trigger schedules: runs one second apart, as local timers always
/// allowed.
pub const LOCAL_MIN_SCHEDULE_INTERVAL: Duration = Duration::from_secs(1);

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
    /// EnterPlanMode, ExitPlanMode and TodoWrite.
    pub plan: bool,
    /// AskUserQuestion.
    pub questions: bool,
    /// Subagent, and a child's report back.
    pub delegation: bool,
    /// ListSubagents, SendMessage and StopSubagent.
    pub family_messages: bool,
    pub tasks: bool,
    /// StartGoal.
    pub goals: bool,
    /// The host's tools, such as the Artifact and Tick tools, and its MCP resources.
    pub artifacts: bool,
    pub tick_subscriptions: bool,
    /// The addresses WebFetch reaches in a Chat without a Folder; a Chat with a Folder reaches
    /// anywhere. Both constructors choose public addresses only.
    pub folderless_reach: Reach,
}

impl ToolPolicy {
    /// Every capability: a local Agent's.
    pub const fn local() -> Self {
        Self {
            web_fetch: true,
            web_search: true,
            skills: true,
            plan: true,
            questions: true,
            delegation: true,
            family_messages: true,
            tasks: true,
            goals: true,
            artifacts: true,
            tick_subscriptions: true,
            folderless_reach: Reach::PublicAddresses,
        }
    }

    /// WebFetch alone: nothing waits for a person, and nothing outlives the turn.
    pub const fn web_fetch_only() -> Self {
        Self {
            web_fetch: true,
            web_search: false,
            skills: false,
            plan: false,
            questions: false,
            delegation: false,
            family_messages: false,
            tasks: false,
            goals: false,
            artifacts: false,
            tick_subscriptions: false,
            folderless_reach: Reach::PublicAddresses,
        }
    }
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
    /// Nothing runs, but a Run waits for a person, or a Loop waits for its Trigger or event
    /// source.
    Waiting(Waits),
}

/// What a quiet Chat waits on, as a host keeps it to know when to come back.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Waits {
    /// The requests that wait for a person, in the order they were made.
    pub people: Vec<PersonWait>,
    /// The earliest time a Trigger of a Loop that can take it is scheduled, or a Loop that has
    /// not ended reaches its deadline, whatever the Loop's state. It may be past already; the
    /// host wakes the family then with it as the due time.
    pub wake_at: Option<SystemTime>,
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
        if seams.environment.is_none() && options.settings.is_some() {
            return Err(AgentError::Invalid(
                "Chats without a Folder read no local user settings".into(),
            ));
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
                (family_state(&chats, &members), members)
            };
            match state {
                Some((true, _)) if !self.inner.shutdown.is_cancelled() => {}
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
        let (member_runs, member_waits) = chat_state(*member, chat);
        runs |= member_runs;
        if let Some(member_waits) = member_waits {
            let family_waits = waits.get_or_insert_with(Waits::default);
            family_waits.people.extend(member_waits.people);
            family_waits.wake_at = family_waits
                .wake_at
                .into_iter()
                .chain(member_waits.wake_at)
                .min();
        }
    }
    found.then_some((runs, waits))
}

fn chat_state(chat_id: ChatId, chat: &Chat) -> (bool, Option<Waits>) {
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
        || !chat.ready_runs.is_empty()
        || chat.parked.keys().any(|run| !waits_for_person(run))
        || user_shell_in(&[UserShellState::Running, UserShellState::Cancelling]);
    let wake_at = crate::triggers::wake_at(&chat.snapshot);
    let waits = !chat.parked.is_empty()
        || waits_for_member
        || !chat.trigger_cancellations.is_empty()
        || !chat.event_cancellations.is_empty()
        || wake_at.is_some()
        || user_shell_in(&[UserShellState::WaitingPermission]);
    (
        runs,
        waits.then(|| Waits {
            wake_at: wake_at
                .map(|milliseconds| SystemTime::UNIX_EPOCH + Duration::from_millis(milliseconds)),
            ..waits_of(chat_id, &chat.snapshot)
        }),
    )
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
        wake_at: None,
    }
}

#[cfg(test)]
#[path = "harness_tests.rs"]
mod tests;
