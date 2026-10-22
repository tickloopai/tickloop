//! What the wake-up base tests share: a model that can hold a turn, the waits they need, and the
//! processes and crash images they run a Chat in.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tickloop_agent_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_agent_protocol::{
    ChatId, Content, EventKind, LoopState, LoopTriggerNoticeKind, ModelIdentity, Protocol, Role,
    Usage,
};
use tickloop_agent_test_support::config;
use tickloop_agent_test_support::shell_ticks::ShellTick;
use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use tickloop_agent_runtime::Agent;

pub(crate) const WAIT: Duration = Duration::from_secs(10);
/// Long enough for a delivery that was going to start a turn to have started it.
pub(crate) const SETTLE: Duration = Duration::from_millis(500);
/// A prompt containing `HOLD:<key>` holds its turn until the test releases `key`.
const HOLD: &str = "HOLD:";
/// The key of the turn [`hold_a_turn`] holds.
const HELD_TURN: &str = "turn";
/// A prompt containing it starts a background Subagent whose task holds it under `child`.
pub(crate) const SPAWN_HELD_CHILD: &str = "SPAWN a held child";
pub(crate) const HELD_CHILD: &str = "child";

/// Answers every turn at once, except a turn its prompt holds, which waits for its key to be
/// released, and a turn asked to start a held child, which calls Subagent for it.
#[derive(Default)]
pub(crate) struct ContractModel {
    requests: Mutex<Vec<ModelRequest>>,
    entered: Mutex<Vec<String>>,
    changed: Notify,
    releases: Mutex<Releases>,
}

/// Releases not yet taken by a held turn, and held turns that ended unreleased, cancelled with
/// their run; releasing one of those releases nothing, so no later turn under its key runs
/// through on a stale release.
#[derive(Default)]
struct Releases {
    pending: Vec<String>,
    ended: Vec<String>,
}

fn take(keys: &mut Vec<String>, key: &str) -> bool {
    match keys.iter().position(|taken| taken == key) {
        Some(index) => {
            keys.remove(index);
            true
        }
        None => false,
    }
}

fn hold_key(text: &str) -> Option<String> {
    let start = text.find(HOLD)? + HOLD.len();
    Some(
        text[start..]
            .chars()
            .take_while(|character| character.is_alphanumeric())
            .collect(),
    )
}

/// The text of the user messages that end the request: the turn's prompt with any host notes
/// that follow it.
fn last_user_text(request: &ModelRequest) -> Option<String> {
    let start = request
        .messages
        .iter()
        .rposition(|message| message.role != Role::User)
        .map_or(0, |index| index + 1);
    let texts: Vec<&str> = request.messages[start..]
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    (!texts.is_empty()).then(|| texts.join("\n"))
}

impl ContractModel {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The text of the newest user message of each conversation request, in order.
    pub(crate) fn prompts(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| !request.tools.is_empty())
            .filter_map(last_user_text)
            .collect()
    }

    /// How many turns took a prompt mentioning `marker`.
    pub(crate) fn seen(&self, marker: &str) -> usize {
        self.prompts()
            .iter()
            .filter(|prompt| prompt.contains(marker))
            .count()
    }

    pub(crate) fn first_turn_with(&self, marker: &str) -> Option<usize> {
        self.prompts()
            .iter()
            .position(|prompt| prompt.contains(marker))
    }

    pub(crate) async fn wait_seen(&self, marker: &str, turns: usize) {
        tokio::time::timeout(WAIT, async {
            while self.seen(marker) < turns {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{marker} reached {turns} turn(s): {:?}", self.prompts()));
    }

    /// Waits until a turn held under `key` reached the model, once for each such turn.
    pub(crate) async fn wait_held(&self, key: &str) {
        tokio::time::timeout(WAIT, async {
            loop {
                let changed = self.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                {
                    let mut entered = self.entered.lock().unwrap();
                    if let Some(index) = entered.iter().position(|held| held == key) {
                        entered.remove(index);
                        return;
                    }
                }
                changed.await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "a turn held under {key} reaches the model: {:?}",
                self.prompts()
                    .iter()
                    .map(|prompt| prompt.chars().take(60).collect::<String>())
                    .collect::<Vec<_>>()
            )
        });
    }

    pub(crate) fn release(&self, key: &str) {
        let mut releases = self.releases.lock().unwrap();
        if !take(&mut releases.ended, key) {
            releases.pending.push(key.into());
        }
        drop(releases);
        self.changed.notify_waiters();
    }

    pub(crate) fn release_held_turn(&self) {
        self.release(HELD_TURN);
    }

    fn take_release(&self, key: &str) -> bool {
        take(&mut self.releases.lock().unwrap().pending, key)
    }

    fn end_unreleased(&self, key: &str) {
        let mut releases = self.releases.lock().unwrap();
        if !take(&mut releases.pending, key) {
            releases.ended.push(key.into());
        }
    }
}

#[async_trait]
impl ModelClient for ContractModel {
    async fn generate_title(
        &self,
        request: ModelRequest,
        _: UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        Ok(tickloop_agent_test_support::title_output(request))
    }

    fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            provider: tickloop_agent_protocol::ModelProvider::Api,
            profile: "fixture".into(),
            protocol: Protocol::Messages,
            requested_model: "fixture".into(),
        }
    }

    async fn complete(
        &self,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        let is_turn = !request.tools.is_empty();
        let answers_a_tool = request.messages.last().is_some_and(|message| {
            message
                .content
                .iter()
                .any(|content| matches!(content, Content::ToolResult { .. }))
        });
        let prompt = last_user_text(&request).unwrap_or_default();
        self.requests.lock().unwrap().push(request);
        if is_turn && !answers_a_tool && prompt.contains(SPAWN_HELD_CHILD) {
            return Ok(ModelOutput {
                private_source: None,
                content: vec![Content::ToolUse {
                    id: "spawn".into(),
                    name: tickloop_agent_protocol::SUBAGENT_TOOL_NAME.into(),
                    input: serde_json::json!({
                        "task": format!("{HOLD}{HELD_CHILD} child task"),
                        "name": "helper",
                        "run_in_background": true,
                    }),
                }],
                usage: Usage::default(),
                stop_reason: "tool_use".into(),
            });
        }
        if is_turn
            && !answers_a_tool
            && let Some(key) = hold_key(&prompt)
        {
            self.entered.lock().unwrap().push(key.clone());
            self.changed.notify_waiters();
            loop {
                let changed = self.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if self.take_release(&key) {
                    break;
                }
                tokio::select! {
                    _ = changed => {}
                    _ = cancel.cancelled() => {
                        self.end_unreleased(&key);
                        return Err(ModelError::Cancelled);
                    }
                }
            }
        }
        let _ = events.send(ModelDelta::Text("ok".into()));
        Ok(ModelOutput {
            private_source: None,
            content: vec![Content::Text { text: "ok".into() }],
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

pub(crate) async fn complete_turn(agent: &Agent, chat: ChatId, text: &str) {
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, text.into()).await.unwrap();
    tokio::time::timeout(WAIT, async {
        loop {
            let event = tickloop_agent_test_support::next_event(&mut events).await;
            if event.run_id == receipt.run_id
                && matches!(
                    event.kind,
                    EventKind::Completed { .. } | EventKind::Failed(_) | EventKind::Interrupted
                )
            {
                return;
            }
        }
    })
    .await
    .expect("the turn ends");
}

/// Starts a turn the model holds until [`ContractModel::release_held_turn`], and waits until the
/// model has it.
pub(crate) async fn hold_a_turn(agent: &Agent, chat: ChatId, model: &ContractModel) {
    agent
        .submit(chat, format!("{HOLD}{HELD_TURN} this turn"))
        .await
        .unwrap();
    model.wait_held(HELD_TURN).await;
}

pub(crate) async fn wait_for_state(agent: &Agent, chat: ChatId, expected: LoopState) {
    tokio::time::timeout(WAIT, async {
        while agent.snapshot(chat).await.unwrap().chat_loop.state != expected {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the Loop reaches {expected:?}"));
}

pub(crate) async fn wait_for_notice(agent: &Agent, chat: ChatId, kind: LoopTriggerNoticeKind) {
    tokio::time::timeout(WAIT, async {
        while !agent
            .loop_trigger_notices()
            .iter()
            .any(|notice| notice.chat_id == chat && notice.kind == kind)
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("a {kind:?} notice appears"));
}

/// What the disk holds at this moment, as a host that crashed now leaves it: a copy of the
/// state root that nothing written later reaches.
pub(crate) fn crash_image(root: &Path, into: &Path) -> PathBuf {
    fn copy(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap().flatten() {
            let target = to.join(entry.file_name());
            if entry.path().is_dir() {
                copy(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), target).unwrap();
            }
        }
    }
    let image = into.join(format!("crash-{}", Uuid::new_v4()));
    copy(root, &image);
    image
}

pub(crate) fn files_under(directory: &Path, parent: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![directory.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory)
            .into_iter()
            .flatten()
            .flatten()
        {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path
                .parent()
                .is_some_and(|parent_path| parent_path.ends_with(parent))
            {
                found.push(path);
            }
        }
    }
    found
}

/// A process holding a saved Chat: resumed, continued by its user, watching `task`'s Shell
/// Tick on a stream of its own.
pub(crate) async fn resumed_process(
    root: &Path,
    chat: ChatId,
    task: Uuid,
    model: Arc<ContractModel>,
) -> (Agent, ShellTick) {
    let agent = Agent::open_embedded(root.into(), config(), model).unwrap();
    let shell = ShellTick::new(task);
    shell.install(&agent).await;
    agent.resume_chat(chat).await.unwrap();
    complete_turn(&agent, chat, "resume here").await;
    shell.wait_until_live(&agent, chat).await;
    (agent, shell)
}

/// A saved Chat whose Loop subscribes to `task`'s Shell Tick, and the process that made it.
pub(crate) async fn subscribed_process(
    root: &Path,
    folder: &Path,
    task: Uuid,
    model: Arc<ContractModel>,
) -> (Agent, ShellTick, ChatId) {
    let agent = Agent::open_embedded(root.into(), config(), model).unwrap();
    let shell = ShellTick::new(task);
    shell.install(&agent).await;
    let chat = agent.create_chat(folder.into()).await.unwrap();
    complete_turn(&agent, chat, "watch the Shell").await;
    shell.subscribe(&agent, chat).await;
    (agent, shell, chat)
}

/// The stand-in scheduled run `n` of task `task`, delivered as a source with no producer yet.
pub(crate) fn scheduled(task: &str, n: u64) -> tickloop_agent_protocol::TriggerSource {
    tickloop_agent_protocol::TriggerSource::ScheduledTask {
        task_id: task.into(),
        scheduled_for_ms: n,
    }
}
