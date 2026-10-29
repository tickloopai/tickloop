//! `UserPromptSubmit`, `SessionStart`, `SessionEnd`, `Stop`, `SubagentStop`, `StopFailure` and
//! `PreCompact` on the Loop's lifecycle, read as Claude Code 2.1.302 reads them.
//!
//! What a hook gives the model is a host note appended at the end of the conversation, so the
//! request prefix an earlier request sent stays byte for byte the same.
//!
//! - `UserPromptSubmit` runs as a Run that a user's prompt started begins, before its first model
//!   request: a block takes the prompt back out of the conversation, as an interrupted prompt
//!   is, and ends the Run; `continue: false` keeps the prompt and ends the Run unanswered.
//!   Triggers and Agent messages are not prompts and run no hook.
//! - It runs too for each prompt queued to a running turn as the turn takes queued input in, at
//!   a tool boundary or as a `Stop` block goes on, as Claude Code's main thread does: a blocked
//!   prompt is taken back the same way while the turn goes on, and a prompt whose hook ends its
//!   turn waits, with the prompts queued after it, for a turn of its own.
//! - `SessionStart` runs before the next model request of a root Chat after it was created
//!   (`startup`), loaded from history (`resume`), started by `/clear` (`clear`) or compacted
//!   (`compact`).
//! - `Stop` runs when a root Chat's model ends its turn without calling tools, `SubagentStop`
//!   when a Subagent's does or before its report is handed back: a block goes on with the
//!   hooks' reasons, the hooks then reading `stop_hook_active: true`; Esc and `/stop` end the Run
//!   as always. As in Claude Code, they may block 8 consecutive times with no tool call between
//!   (a hand-back is none): the ninth block is overridden with a warning and the turn ends.
//!   Input queued meanwhile joins after the reasons, so a loop that never calls tools still
//!   reads it. A Run that read an `asyncRewake` hook's notification reads
//!   `stop_hook_active: true` too, as Claude Code queues that notification.
//! - `StopFailure` tells the hooks a turn ended on a model error, `SessionEnd` that a root Chat
//!   ends with its Worker; neither changes anything.
//! - `PreCompact` runs before a compaction: a block cancels it, and plain output from the others
//!   joins the summary's instructions.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use serde_json::{Map, Value, json};
use tickloop_harness_model::{ModelError, ModelFailureKind, ModelRequest};
use tickloop_harness_protocol::hooks::HookEvent;
use tickloop_harness_protocol::{Content, HostNote, Message, PendingInput, Receipt, Role, RunId};
use tokio_util::sync::CancellationToken;

use super::{HookCall, HookVerdict};
use crate::{Agent, AgentError, Chat};

/// Claude Code's note when a hook ends a prompt's turn without a reason.
const DEFAULT_STOP_REASON: &str = "Operation stopped by hook";
/// Claude Code's `SessionEnd` reason when the process ends any other way than `/clear` or a
/// logout.
const SESSION_END_REASON: &str = "other";
/// Claude Code's longest wait for `SessionEnd` hooks as its process ends.
const SESSION_END_BUDGET: Duration = Duration::from_secs(60);
/// Claude Code's most consecutive blocks of a turn's end by `Stop` or `SubagentStop` hooks with
/// no tool call between them; the next one is overridden.
const STOP_BLOCK_CAP: u32 = 8;

/// Why a session starts, Claude Code's `SessionStart` source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionSource {
    Startup,
    Resume,
    Clear,
    Compact,
}

impl SessionSource {
    fn name(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Resume => "resume",
            Self::Clear => "clear",
            Self::Compact => "compact",
        }
    }
}

/// What the session and turn hooks keep for a Chat in this process.
#[derive(Clone, Debug, Default)]
pub(crate) struct SessionHookState {
    /// The `SessionStart` the Chat's next model request runs first.
    session_start: Option<SessionSource>,
    /// Runs a `Stop` or `SubagentStop` hook kept going: their hooks read `stop_hook_active`.
    continued_by_stop: HashSet<RunId>,
    /// How many consecutive times `Stop` or `SubagentStop` hooks blocked each Run's end since its
    /// model last called a tool other than the hand-back.
    stop_blocks: HashMap<RunId, u32>,
    /// Runs whose prompt a `UserPromptSubmit` hook blocked, with the prompt's message index:
    /// the prompt is taken back once the Run ends.
    blocked_prompts: HashMap<RunId, usize>,
    /// Runs a user's prompt started whose `UserPromptSubmit` hooks have not let it through yet.
    unchecked_prompts: HashSet<RunId>,
    /// `asyncRewake` notifications no model request has read yet.
    unread_rewakes: Vec<Message>,
    /// Queued prompts a hook ended the turn of: they, and the prompts queued after them, wait
    /// for the running turn to end.
    held_prompts: HashSet<RunId>,
}

impl SessionHookState {
    /// The Chat's next model request runs the `SessionStart` hooks for `source` first.
    pub(crate) fn start_session(&mut self, source: SessionSource) {
        self.session_start = Some(source);
    }

    /// `run` starts with a user's prompt, which its `UserPromptSubmit` hooks check first.
    pub(crate) fn await_prompt_hooks(&mut self, run: RunId) {
        self.held_prompts.remove(&run);
        self.unchecked_prompts.insert(run);
    }

    /// Whether the queued prompt of `run` waits for the running turn to end.
    pub(crate) fn holds_prompt(&self, run: RunId) -> bool {
        self.held_prompts.contains(&run)
    }

    /// An `asyncRewake` hook notified the Loop with `notification`: the Run whose model reads
    /// it goes on as a `Stop` block's Run does.
    pub(crate) fn note_rewake(&mut self, notification: Message) {
        self.unread_rewakes.push(notification);
    }

    /// The Run `run` is about to send `messages`: the rewake notifications among them were
    /// read by it.
    fn read_rewakes(&mut self, messages: &[Message], run: RunId) {
        let unread = self.unread_rewakes.len();
        self.unread_rewakes
            .retain(|notification| !messages.contains(notification));
        if self.unread_rewakes.len() < unread {
            self.continued_by_stop.insert(run);
        }
    }

    /// Whether a prompt waits for its `UserPromptSubmit` hooks.
    pub(crate) fn awaits_prompt_hooks(&self) -> bool {
        !self.unchecked_prompts.is_empty()
    }

    /// The model of `run` called a tool other than the hand-back: blocks of its turn's end count
    /// from zero again.
    pub(crate) fn note_tool_calls(&mut self, run: RunId) {
        self.stop_blocks.remove(&run);
    }

    /// Forgets what the hooks kept for `run`, which ended.
    pub(crate) fn forget_run(&mut self, run: RunId) {
        self.continued_by_stop.remove(&run);
        self.stop_blocks.remove(&run);
        self.unchecked_prompts.remove(&run);
    }

    /// The prompt index of `run` a `UserPromptSubmit` hook blocked, forgetting it.
    pub(crate) fn take_blocked_prompt(&mut self, run: RunId) -> Option<usize> {
        self.blocked_prompts.remove(&run)
    }
}

/// Whether `pending` is a prompt a person gave, which `UserPromptSubmit` hooks check.
pub(crate) fn is_user_prompt(pending: &PendingInput) -> bool {
    pending.trigger_source.is_none()
        && pending.agent_message.is_none()
        && !pending.task_continuation
}

/// What a prompt queued to a running turn does as the turn takes it in, after its
/// `UserPromptSubmit` hooks.
pub(crate) enum QueuedPrompt {
    /// It joins the turn, followed by these notes from its hooks.
    Joins(Vec<String>),
    /// A hook blocked it: it is taken back out of the conversation as it joins.
    Blocked,
}

/// What `UserPromptSubmit` lets the Run that a prompt started do.
pub(crate) enum PromptGate {
    /// The Run answers the prompt.
    Proceed,
    /// The Run ends without a model request: a hook blocked the prompt, ended the turn, or the
    /// Run was interrupted while the hooks ran.
    End,
}

/// Claude Code's `<Event>:<query>` hook name, which its notes to the model start with.
fn hook_name(event: HookEvent, query: Option<&str>) -> String {
    match query {
        Some(query) => format!("{}:{query}", event.name()),
        None => event.name().to_owned(),
    }
}

/// Claude Code's notes for a hook's additional context and plain output.
fn context_notes(name: &str, verdict: &HookVerdict) -> Vec<String> {
    let mut notes = Vec::new();
    if !verdict.additional_context.is_empty() {
        notes.push(format!(
            "{name} hook additional context: {}",
            verdict.additional_context.join("\n")
        ));
    }
    notes.extend(
        verdict
            .plain_text
            .iter()
            .map(|text| format!("{name} hook success: {text}")),
    );
    notes
}

fn text_of(content: &[Content]) -> String {
    content
        .iter()
        .filter_map(|content| match content {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The text of the model's last reply, Claude Code's `last_assistant_message`.
fn last_assistant_message(chat: &Chat) -> Option<String> {
    chat.snapshot
        .messages
        .iter()
        .rev()
        .find(|message| message.role == Role::Assistant)
        .map(|message| text_of(&message.content))
        .filter(|text| !text.trim().is_empty())
}

/// The prompts `run` started with: the messages it began with that a user typed.
fn run_prompts(chat: &Chat, run: RunId) -> Vec<usize> {
    let snapshot = &chat.snapshot;
    let Some(start) = snapshot
        .message_runs
        .iter()
        .find(|(_, owner)| **owner == run)
        .map(|(index, _)| *index)
    else {
        return Vec::new();
    };
    snapshot
        .prompt_execution_modes
        .range(start..)
        .map(|(index, _)| *index)
        .collect()
}

/// Claude Code's `StopFailure` error kind for a model failure.
fn failure_name(error: &ModelError) -> &'static str {
    match error.kind() {
        ModelFailureKind::RateLimited => "rate_limit",
        ModelFailureKind::AuthenticationRequired
        | ModelFailureKind::AuthenticationFailed
        | ModelFailureKind::EntitlementDenied => "authentication_failed",
        ModelFailureKind::QuotaExhausted => "billing_error",
        ModelFailureKind::Configuration
        | ModelFailureKind::InvalidResponse
        | ModelFailureKind::ResponseRejected => "invalid_request",
        ModelFailureKind::Connection
        | ModelFailureKind::Timeout
        | ModelFailureKind::StreamInterrupted
        | ModelFailureKind::ProviderFailure => "server_error",
        _ => "unknown",
    }
}

pub(crate) fn append_hook_notes(
    chat: &mut Chat,
    receipt: &Receipt,
    event: HookEvent,
    notes: Vec<String>,
) {
    for note in notes {
        crate::host_notes::append_note(&mut chat.snapshot, receipt, note, HostNote::Hook(event));
    }
}

impl Agent {
    /// Appends `notes` from the hooks of `event` to the active Run's conversation and records
    /// them, as other host notes are.
    async fn note_from_hooks(
        &self,
        receipt: &Receipt,
        event: HookEvent,
        notes: Vec<String>,
    ) -> Result<(), AgentError> {
        if notes.is_empty() {
            return Ok(());
        }
        let (snapshot, bookkeeping) = {
            let mut chats = self.inner.chats.lock().await;
            let chat = chats
                .get_mut(&receipt.chat_id)
                .ok_or(AgentError::NotFound)?;
            if chat.active_run != Some(receipt.run_id) {
                return Ok(());
            }
            append_hook_notes(chat, receipt, event, notes);
            (chat.snapshot.clone(), chat.bookkeeping.clone())
        };
        self.checkpoint_transcript(&snapshot, &bookkeeping)
            .await
            .map_err(AgentError::Storage)
    }

    /// Runs `UserPromptSubmit` for the prompts the Run `receipt` started with, before its first
    /// model request.
    pub(crate) async fn user_prompt_submit(
        &self,
        receipt: &Receipt,
        cancel: &CancellationToken,
    ) -> Result<PromptGate, AgentError> {
        let (prompts, prompt) = {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&receipt.chat_id).ok_or(AgentError::NotFound)?;
            if !chat
                .hook_state
                .session
                .unchecked_prompts
                .contains(&receipt.run_id)
            {
                return Ok(PromptGate::Proceed);
            }
            let prompts = run_prompts(chat, receipt.run_id);
            let prompt = prompts
                .iter()
                .map(|index| text_of(&chat.snapshot.messages[*index].content))
                .collect::<Vec<_>>()
                .join("\n\n");
            (prompts, prompt)
        };
        let Some(&first_prompt) = prompts.first() else {
            self.let_prompt_through(receipt).await;
            return Ok(PromptGate::Proceed);
        };
        let mut fields = Map::new();
        fields.insert("prompt".into(), json!(prompt));
        let verdict = self
            .run_hooks(
                receipt,
                HookCall {
                    event: HookEvent::UserPromptSubmit,
                    match_query: None,
                    fields,
                    tool: None,
                },
                cancel,
            )
            .await;
        if verdict.cancelled || cancel.is_cancelled() {
            return Ok(PromptGate::End);
        }
        if !verdict.blocking_errors.is_empty() {
            let mut chats = self.inner.chats.lock().await;
            let chat = chats
                .get_mut(&receipt.chat_id)
                .ok_or(AgentError::NotFound)?;
            chat.hook_state
                .session
                .blocked_prompts
                .insert(receipt.run_id, first_prompt);
            return Ok(PromptGate::End);
        }
        let name = hook_name(HookEvent::UserPromptSubmit, None);
        let mut notes = context_notes(&name, &verdict);
        if verdict.prevent_continuation {
            notes.push(
                verdict
                    .stop_reason
                    .clone()
                    .unwrap_or_else(|| DEFAULT_STOP_REASON.into()),
            );
        }
        self.note_from_hooks(receipt, HookEvent::UserPromptSubmit, notes)
            .await?;
        self.let_prompt_through(receipt).await;
        Ok(if verdict.prevent_continuation {
            PromptGate::End
        } else {
            PromptGate::Proceed
        })
    }

    /// Runs `UserPromptSubmit` for the `prompts` queued to the Run `active`, in queue order, as
    /// the Run takes them in: what each does, or `None` when the Run was interrupted meanwhile,
    /// which leaves them all queued. A prompt a hook ends the turn of is held with the prompts
    /// after it, which are not checked and get no entry, as Claude Code leaves them queued.
    pub(crate) async fn check_queued_prompts(
        &self,
        active: &Receipt,
        prompts: Vec<(RunId, String)>,
        cancel: &CancellationToken,
    ) -> Option<HashMap<RunId, QueuedPrompt>> {
        let name = hook_name(HookEvent::UserPromptSubmit, None);
        let mut checked = HashMap::new();
        for (run, prompt) in prompts {
            let mut fields = Map::new();
            fields.insert("prompt".into(), json!(prompt));
            let verdict = self
                .run_hooks(
                    active,
                    HookCall {
                        event: HookEvent::UserPromptSubmit,
                        match_query: None,
                        fields,
                        tool: None,
                    },
                    cancel,
                )
                .await;
            if verdict.cancelled || cancel.is_cancelled() {
                return None;
            }
            if !verdict.blocking_errors.is_empty() {
                checked.insert(run, QueuedPrompt::Blocked);
            } else if verdict.prevent_continuation {
                if let Some(chat) = self.inner.chats.lock().await.get_mut(&active.chat_id) {
                    chat.hook_state.session.held_prompts.insert(run);
                }
                break;
            } else {
                checked.insert(run, QueuedPrompt::Joins(context_notes(&name, &verdict)));
            }
        }
        Some(checked)
    }

    /// The prompt of the Run `receipt` passed its hooks: the Chat's title may come from it.
    async fn let_prompt_through(&self, receipt: &Receipt) {
        if let Some(chat) = self.inner.chats.lock().await.get_mut(&receipt.chat_id) {
            chat.hook_state
                .session
                .unchecked_prompts
                .remove(&receipt.run_id);
        }
        self.start_title_generation(receipt.chat_id).await;
    }

    /// Runs the `SessionStart` hooks the Chat waits for, before a model request of the Run
    /// `receipt`; the source waits on when the Run is interrupted while they run. Notes first
    /// which `asyncRewake` notifications the request reads.
    pub(crate) async fn announce_session_start(
        &self,
        receipt: &Receipt,
        cancel: &CancellationToken,
    ) -> Result<(), AgentError> {
        let (source, model) = {
            let mut chats = self.inner.chats.lock().await;
            let chat = chats
                .get_mut(&receipt.chat_id)
                .ok_or(AgentError::NotFound)?;
            if !chat.hook_state.session.unread_rewakes.is_empty() {
                let start = chat.snapshot.active_start();
                chat.hook_state
                    .session
                    .read_rewakes(&chat.snapshot.messages[start..], receipt.run_id);
            }
            let Some(source) = chat.hook_state.session.session_start.take() else {
                return Ok(());
            };
            (source, chat.snapshot.model.model.clone())
        };
        let mut fields = Map::new();
        fields.insert("source".into(), json!(source.name()));
        fields.insert("model".into(), json!(model));
        let verdict = self
            .run_hooks(
                receipt,
                HookCall {
                    event: HookEvent::SessionStart,
                    match_query: Some(source.name().into()),
                    fields,
                    tool: None,
                },
                cancel,
            )
            .await;
        if verdict.cancelled || cancel.is_cancelled() {
            if let Some(chat) = self.inner.chats.lock().await.get_mut(&receipt.chat_id) {
                chat.hook_state.session.session_start.get_or_insert(source);
            }
            return Err(ModelError::Cancelled.into());
        }
        let name = hook_name(HookEvent::SessionStart, Some(source.name()));
        self.note_from_hooks(
            receipt,
            HookEvent::SessionStart,
            context_notes(&name, &verdict),
        )
        .await
    }

    /// Runs `Stop`, or `SubagentStop` in a Subagent, as the model ends its turn without calling
    /// tools: the request that goes on with the hooks' reasons when one blocks the turn's end,
    /// followed by the input queued to the turn meanwhile, as at a tool boundary.
    pub(crate) async fn stop_turn_hooks(
        &self,
        receipt: &Receipt,
        cancel: &CancellationToken,
    ) -> Result<Option<ModelRequest>, String> {
        let Some(feedback) = self.run_stop_hooks(receipt, cancel).await? else {
            return Ok(None);
        };
        {
            let mut chats = self.inner.chats.lock().await;
            let chat = chats
                .get_mut(&receipt.chat_id)
                .ok_or("Agent is unavailable")?;
            if chat.active_run != Some(receipt.run_id) {
                return Ok(None);
            }
            let event = stop_event(chat);
            append_hook_notes(chat, receipt, event, feedback);
            self.save(&chat.snapshot);
        }
        self.merge_queued_messages(receipt)
            .await
            .map_err(|error| error.to_string())?;
        self.complete_file_history_snapshots(receipt.chat_id).await;
        let chats = self.inner.chats.lock().await;
        let chat = chats.get(&receipt.chat_id).ok_or("Agent is unavailable")?;
        self.continuation_request(chat).map(Some)
    }

    /// Runs `SubagentStop` before a Subagent hands its report back: the reasons a hook blocks
    /// the hand-back with, which the model reads instead.
    pub(crate) async fn subagent_stop_before_handback(
        &self,
        receipt: &Receipt,
        cancel: &CancellationToken,
    ) -> Result<Option<String>, String> {
        Ok(self
            .run_stop_hooks(receipt, cancel)
            .await?
            .map(|feedback| feedback.join("\n\n")))
    }

    /// The `Stop` or `SubagentStop` hooks' feedback when they block the turn's end, after which
    /// they read `stop_hook_active`; none when the block is one more than the cap allows.
    async fn run_stop_hooks(
        &self,
        receipt: &Receipt,
        cancel: &CancellationToken,
    ) -> Result<Option<Vec<String>>, String> {
        let (event, mut fields, blocks) = {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&receipt.chat_id).ok_or("Agent is unavailable")?;
            let blocks = chat
                .hook_state
                .session
                .stop_blocks
                .get(&receipt.run_id)
                .map_or(1, |blocks| blocks + 1);
            let mut fields = Map::new();
            fields.insert(
                "stop_hook_active".into(),
                json!(
                    chat.hook_state
                        .session
                        .continued_by_stop
                        .contains(&receipt.run_id)
                ),
            );
            fields.insert(
                "last_assistant_message".into(),
                last_assistant_message(chat).map_or(Value::Null, Value::String),
            );
            (stop_event(chat), fields, blocks)
        };
        if event == HookEvent::SubagentStop {
            let transcript = match self.hook_transcript_path(receipt.chat_id).await {
                Ok(Some(path)) => path.to_string_lossy().into_owned(),
                _ => String::new(),
            };
            fields.insert("agent_transcript_path".into(), json!(transcript));
        }
        let is_over_cap = blocks > STOP_BLOCK_CAP;
        let verdict = self
            .run_hooks_overriding_block(
                receipt,
                HookCall {
                    event,
                    match_query: None,
                    fields,
                    tool: None,
                },
                cancel,
                is_over_cap.then(|| stop_block_cap_warning(blocks)),
            )
            .await;
        if verdict.cancelled
            || cancel.is_cancelled()
            || verdict.prevent_continuation
            || verdict.blocking_errors.is_empty()
            || is_over_cap
        {
            return Ok(None);
        }
        if let Some(chat) = self.inner.chats.lock().await.get_mut(&receipt.chat_id) {
            let session = &mut chat.hook_state.session;
            session.continued_by_stop.insert(receipt.run_id);
            session.stop_blocks.insert(receipt.run_id, blocks);
        }
        Ok(Some(
            verdict
                .blocking_errors
                .iter()
                .map(|error| format!("{} hook feedback:\n{error}", event.name()))
                .collect(),
        ))
    }

    /// Tells the `StopFailure` hooks the Run `receipt` ends on a model error, without waiting for
    /// them.
    pub(crate) fn notify_stop_failure(&self, receipt: &Receipt, error: &ModelError) {
        if error.kind() == ModelFailureKind::Cancelled {
            return;
        }
        let kind = failure_name(error);
        let details = error.to_string();
        let mut fields = Map::new();
        fields.insert("error".into(), json!(kind));
        fields.insert("error_details".into(), json!(details));
        fields.insert("last_assistant_message".into(), json!(details));
        let agent = self.clone();
        let receipt = receipt.clone();
        let cancel = self.inner.shutdown.child_token();
        self.inner.tasks.spawn(async move {
            agent
                .run_hooks(
                    &receipt,
                    HookCall {
                        event: HookEvent::StopFailure,
                        match_query: None,
                        fields,
                        tool: None,
                    },
                    &cancel,
                )
                .await;
        });
    }

    /// Runs `PreCompact` before the Chat `receipt` names is compacted: the instructions the
    /// summary follows, joined by the plain output of the hooks, or why a hook blocked it.
    pub(crate) async fn pre_compact(
        &self,
        receipt: &Receipt,
        is_automatic: bool,
        instructions: Option<String>,
        cancel: &CancellationToken,
    ) -> Result<Option<String>, AgentError> {
        let trigger = if is_automatic { "auto" } else { "manual" };
        let mut fields = Map::new();
        fields.insert("trigger".into(), json!(trigger));
        fields.insert(
            "custom_instructions".into(),
            instructions.clone().map_or(Value::Null, Value::String),
        );
        let verdict = self
            .run_hooks(
                receipt,
                HookCall {
                    event: HookEvent::PreCompact,
                    match_query: Some(trigger.into()),
                    fields,
                    tool: None,
                },
                cancel,
            )
            .await;
        if verdict.cancelled {
            return Err(ModelError::Cancelled.into());
        }
        if !verdict.blocking_errors.is_empty() {
            return Err(AgentError::Invalid(format!(
                "a PreCompact hook blocked compaction: {}",
                verdict.blocking_errors.join("\n")
            )));
        }
        let joined = instructions
            .into_iter()
            .chain(verdict.plain_text)
            .collect::<Vec<_>>()
            .join("\n\n");
        Ok(Some(joined).filter(|joined| !joined.is_empty()))
    }

    /// Tells every root Chat's `SessionEnd` hooks that the Chat ends with this Worker, waiting
    /// for them at most Claude Code's longest time.
    pub(crate) async fn end_sessions(&self) {
        let receipts = self
            .inner
            .chats
            .lock()
            .await
            .values()
            .filter(|chat| chat.snapshot.delegation.is_none() && chat.snapshot.folder.is_some())
            .map(|chat| Receipt {
                chat_id: chat.snapshot.id,
                loop_id: chat.snapshot.chat_loop.id,
                run_id: chat.snapshot.chat_loop.run_id.unwrap_or_else(RunId::new),
            })
            .collect::<Vec<_>>();
        if receipts.is_empty() {
            return;
        }
        let cancel = CancellationToken::new();
        let ends = receipts.iter().map(|receipt| {
            let mut fields = Map::new();
            fields.insert("reason".into(), json!(SESSION_END_REASON));
            self.run_hooks(
                receipt,
                HookCall {
                    event: HookEvent::SessionEnd,
                    match_query: None,
                    fields,
                    tool: None,
                },
                &cancel,
            )
        });
        let all = futures_util::future::join_all(ends);
        tokio::pin!(all);
        tokio::select! {
            _ = &mut all => {}
            () = tokio::time::sleep(SESSION_END_BUDGET) => {
                cancel.cancel();
                all.await;
            }
        }
    }
}

/// Claude Code's warning as it overrides the `blocks`th consecutive block of a turn's end,
/// without its advice to raise the cap, which Tickloop fixes.
fn stop_block_cap_warning(blocks: u32) -> String {
    format!(
        "A hook blocked the turn from ending {blocks} consecutive times — overriding and ending \
         turn. For Stop/SubagentStop hooks, check stop_hook_active in the input and return \
         success while it's true."
    )
}

fn stop_event(chat: &Chat) -> HookEvent {
    if chat.snapshot.delegation.is_some() {
        HookEvent::SubagentStop
    } else {
        HookEvent::Stop
    }
}
