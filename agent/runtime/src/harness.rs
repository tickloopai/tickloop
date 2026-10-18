//! How a host assembles an Agent: the seams it reaches the outside through, and the options it
//! imposes on every Chat.

use std::{sync::Arc, time::Duration};

use tickloop_agent_model::{ModelClient, ModelClients, ModelError};
use tickloop_agent_protocol::{AgentHostOptions, ChatId, ModelConfig, PermissionRule};

use crate::{
    Agent, AgentError, environment::ExecutionEnvironment, persistence::ChatRecorder,
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
}

/// Which of the Agent's own capabilities its Chats offer the model. The file tools, Bash and
/// TaskStop also need a Folder; each capability still follows its own settings, such as Task
/// tools or web search.
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
        }
    }
}

/// Serves one client for every model the Chats select: an Agent built around a single client.
pub(crate) struct OneClient(pub(crate) Arc<dyn ModelClient>);

impl ModelClients for OneClient {
    fn client(&self, _: &ModelConfig) -> Result<Arc<dyn ModelClient>, ModelError> {
        Ok(self.0.clone())
    }
}

/// What remains of a Chat once `quiesce` returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quiescence {
    /// Nothing of the Chat runs or waits to run.
    Idle,
    /// Nothing runs, but a Run waits for a person, or a Loop waits for its Trigger or event
    /// source.
    Waiting,
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

    /// Waits until the Chat has no active or ready Run and its title request has ended: one
    /// still running after ten seconds is cancelled and awaited.
    pub async fn quiesce(&self, chat_id: ChatId) -> Quiescence {
        let mut events = self.subscribe();
        let quiescence = loop {
            let state = self.inner.chats.lock().await.get(&chat_id).map(|chat| {
                let runs = chat.active.is_some()
                    || chat.active_run.is_some()
                    || !chat.ready_runs.is_empty()
                    || chat.has_foreground_user_shell();
                let waits = !chat.parked.is_empty()
                    || !chat.trigger_cancellations.is_empty()
                    || !chat.event_cancellations.is_empty();
                (runs, waits)
            });
            match state {
                Some((true, _)) if !self.inner.shutdown.is_cancelled() => {}
                Some((_, true)) => break Quiescence::Waiting,
                _ => break Quiescence::Idle,
            }
            tokio::select! {
                _ = events.recv() => {}
                _ = tokio::time::sleep(QUIESCE_RECHECK) => {}
                _ = self.inner.shutdown.cancelled() => {}
            }
        };
        let title = self.inner.title_requests.lock().await.get(&chat_id).cloned();
        if let Some(title) = title {
            if tokio::time::timeout(TITLE_SETTLE_LIMIT, title.done.cancelled())
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
