//! The goal in a running Agent: the state machine's events come from the user's `/goal`
//! commands, the goal tools, Esc, `/stop`, history loads, model usage and each ended turn; its
//! effects reach the Transcript, the conversation, the event stream and the wake-up base, which
//! delivers every continuation.

use std::collections::HashMap;
use std::time::Duration;

use tickloop_agent_base::{private_fs::unix_time_milliseconds, timestamp::format_timestamp};
use tickloop_agent_model::{ModelConfigExt, REQUEST_OVERHEAD_TOKENS};
use tickloop_agent_protocol::{
    ChatId, CommandState, Content, EventKind, ExecutionMode, GoalAction, GoalNotice, GoalSnapshot,
    GoalStatus, GoalVerdict, LoopState, ModelConfig, ModelRequestKind, OperationRecord, Receipt,
    RequestOutcome, ResponseRejection, Role, RunId, ToolResult, TriggerIdentity, TriggerSource,
    Usage, UserShellState, local_messages,
};
use tickloop_agent_transcript::{GoalEntry, GoalEventEntry, Line};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::check;
use super::prompts::{self, GoalPromptContext};
use super::state::{
    self, ContinuationConditions, GoalEffect, GoalError, GoalEvent, GoalTransition, TurnSummary,
};
use super::store::GoalStore;
use super::tools::{
    CREATE_GOAL_TOOL_NAME, GET_GOAL_TOOL_NAME, GoalReport, NewGoal, UPDATE_GOAL_TOOL_NAME,
};
use crate::model_request::RequestContext;
use crate::wakeup::TriggerDelivery;
use crate::{Agent, AgentError, Chat};

/// Assumed when the check model's card names no context window, as for the approval reviewer.
const DEFAULT_CHECK_CONTEXT_WINDOW_TOKENS: usize = 128_000;
/// The approval reviewer's bound on one request's payload, which the check shares.
const MAX_CHECK_PAYLOAD_BYTES: usize = 1024 * 1024;
const NO_GOAL: &str = "This Chat has no goal.";
const SUBAGENT_HAS_NO_GOAL: &str = "a Subagent has no goal; only the root Agent's Chat has one";

fn prompt_context(goal: &GoalSnapshot) -> GoalPromptContext<'_> {
    GoalPromptContext {
        objective: &goal.objective,
        last_check: match goal.last_check.as_ref().map(|check| &check.verdict) {
            Some(GoalVerdict::NotMet { reason }) => Some(reason),
            Some(GoalVerdict::Met | GoalVerdict::Impossible { .. }) | None => None,
        },
        tokens_used: goal.tokens_used,
        token_budget: goal.token_budget,
        time_used: Duration::from_millis(goal.time_used_ms),
    }
}

/// The tokens a model usage report charges to a goal: fresh input and output, as Codex counts
/// them; cached input is not charged again.
pub(crate) fn charged_tokens(usage: &Usage) -> u64 {
    usage
        .input_tokens
        .unwrap_or_default()
        .saturating_add(usage.output_tokens.unwrap_or_default())
}

/// The goal's state with its running usage left out: what changing makes a goal record.
fn without_usage(goal: Option<&GoalSnapshot>) -> Option<GoalSnapshot> {
    goal.map(|goal| GoalSnapshot {
        tokens_used: 0,
        time_used_ms: 0,
        updated_at_ms: 0,
        ..goal.clone()
    })
}

fn invalid(error: GoalError) -> AgentError {
    AgentError::Invalid(error.to_string())
}

fn tool_result(content: String, is_error: bool) -> ToolResult {
    ToolResult {
        content,
        is_error,
        output_ref: None,
    }
}

/// How a turn ended, for its goal.
pub(crate) enum GoalTurnEnd {
    Completed,
    /// An execution error ended it abnormally.
    Failed(String),
    /// Esc, `/stop`, a newer input sent now or the host closing; only its time counts.
    Interrupted,
}

/// How a turn that just ended went for the goal, read from the conversation and the ledger.
fn turn_summary(chat: &Chat, run: RunId, failure: Option<String>) -> TurnSummary {
    let snapshot = &chat.snapshot;
    let lifecycle = &chat.bookkeeping.lifecycle;
    let continuation = lifecycle
        .open_input(run)
        .and_then(|input| lifecycle.input(input))
        .and_then(|input| input.trigger.as_ref())
        .is_some_and(|trigger| {
            matches!(trigger.identity, TriggerIdentity::GoalContinuation { .. })
        });
    let replies = snapshot
        .messages
        .iter()
        .enumerate()
        .filter(|(index, message)| {
            message.role == Role::Assistant && snapshot.message_runs.get(index) == Some(&run)
        })
        .flat_map(|(_, message)| &message.content);
    let (mut called_tools, mut replied_substantively) = (false, false);
    for content in replies {
        match content {
            Content::ToolUse { .. } => called_tools = true,
            Content::Text { text } if !text.trim().is_empty() => replied_substantively = true,
            _ => {}
        }
    }
    TurnSummary {
        continuation,
        called_tools,
        replied_substantively,
        failure,
    }
}

/// Why a goal change did not happen.
#[derive(Debug, thiserror::Error)]
pub(crate) enum GoalUpdateError {
    #[error(transparent)]
    Goal(#[from] GoalError),
    #[error("the Chat is no longer loaded")]
    NotLoaded,
    #[error("the goal could not be stored: {0}")]
    Storage(String),
}

impl From<GoalUpdateError> for AgentError {
    fn from(error: GoalUpdateError) -> Self {
        match error {
            GoalUpdateError::Goal(error) => invalid(error),
            GoalUpdateError::NotLoaded => AgentError::NotFound,
            GoalUpdateError::Storage(error) => AgentError::Storage(error),
        }
    }
}

/// Applies `events` in order to `base`; the first that does not apply undoes them all.
fn apply_events(
    base: Option<&GoalSnapshot>,
    events: Vec<GoalEvent>,
    now_ms: u64,
) -> Result<(Option<GoalSnapshot>, Vec<GoalEffect>), GoalError> {
    let mut goal = base.cloned();
    let mut effects = Vec::new();
    for event in events {
        let GoalTransition {
            goal: next,
            effects: more,
        } = state::apply(goal.as_ref(), event, now_ms)?;
        goal = next;
        effects.extend(more);
    }
    Ok((goal, effects))
}

/// Whether a member of the Chat's family below it still works or has work queued; a lineage that
/// cannot be read counts as working.
fn background_children_run(chats: &HashMap<ChatId, Chat>, chat_id: ChatId) -> bool {
    crate::family_tree::descendants(chats, chat_id).map_or(true, |members| {
        members.iter().filter_map(|id| chats.get(id)).any(|member| {
            !member.snapshot.pending_inputs.is_empty()
                || !matches!(
                    member.snapshot.chat_loop.state,
                    LoopState::Idle | LoopState::Interrupted | LoopState::Failed
                )
        })
    })
}

fn background_shells_run(snapshot: &tickloop_agent_protocol::ChatSnapshot) -> bool {
    snapshot.commands.iter().any(|command| {
        matches!(
            command.state,
            CommandState::Running | CommandState::Background
        )
    }) || snapshot.user_shells.iter().any(|shell| {
        matches!(
            shell.state,
            UserShellState::Queued
                | UserShellState::WaitingPermission
                | UserShellState::Running
                | UserShellState::Background
        )
    })
}

/// Whether the Chat's background children or Shells still run, which holds its goal's
/// continuation back where it is delivered and where it is taken.
pub(crate) fn background_work_runs(chats: &HashMap<ChatId, Chat>, chat_id: ChatId) -> bool {
    background_children_run(chats, chat_id)
        || chats
            .get(&chat_id)
            .is_some_and(|chat| background_shells_run(&chat.snapshot))
}

fn goal_store(chat: &Chat) -> Option<GoalStore> {
    chat.tools
        .chat_directory()
        .map(GoalStore::in_chat_directory)
}

impl Agent {
    /// Applies `events` to the Chat's goal as the store keeps it, then carries out what they ask
    /// for. The goal first gets the running time since it was last settled, while a turn runs or
    /// as `turn_ends`. A goal record is written when anything but the running usage changed, and
    /// whenever a turn ends with an unfinished goal; the store keeps the usage meanwhile.
    pub(crate) async fn update_goal(
        &self,
        chat_id: ChatId,
        events: Vec<GoalEvent>,
        turn_ends: bool,
    ) -> Result<(), GoalUpdateError> {
        let now_ms = unix_time_milliseconds();
        let (store, unstored, events) = {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&chat_id).ok_or(GoalUpdateError::NotLoaded)?;
            let is_running = turn_ends || chat.active.is_some() || !chat.parked.is_empty();
            let started_ms = chat
                .snapshot
                .chat_loop
                .run_started_at
                .unwrap_or(now_ms)
                .max(chat.goal_clock_ms);
            let mut settled = Vec::new();
            if is_running && now_ms > started_ms {
                settled.push(GoalEvent::TimeElapsed {
                    ms: now_ms - started_ms,
                });
            }
            settled.extend(events);
            (goal_store(chat), chat.snapshot.goal.clone(), settled)
        };
        let (generation, goal, effects) = match store {
            Some(store) => {
                let (stored, effects) = tokio::task::spawn_blocking(move || {
                    store.update(unstored, |base| apply_events(base, events, now_ms))
                })
                .await
                .map_err(|error| GoalUpdateError::Storage(error.to_string()))?
                .map_err(|error| GoalUpdateError::Storage(error.to_string()))??;
                (Some(stored.generation), stored.goal, effects)
            }
            None => {
                let (goal, effects) = apply_events(unstored.as_ref(), events, now_ms)?;
                (None, goal, effects)
            }
        };
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).ok_or(GoalUpdateError::NotLoaded)?;
        chat.goal_clock_ms = chat.goal_clock_ms.max(now_ms);
        self.adopt_goal(chat, generation, goal, effects, turn_ends);
        Ok(())
    }

    /// Takes on a change of the goal: the goal itself unless a newer stored one was adopted
    /// already, then its effects and its record.
    fn adopt_goal(
        &self,
        chat: &mut Chat,
        generation: Option<u64>,
        goal: Option<GoalSnapshot>,
        effects: Vec<GoalEffect>,
        turn_ends: bool,
    ) {
        let previous = chat.snapshot.goal.clone();
        if generation.is_none_or(|generation| generation > chat.goal_generation) {
            chat.snapshot.goal = goal;
            chat.goal_generation = generation.unwrap_or(chat.goal_generation);
        }
        let mut notices = Vec::new();
        for effect in effects {
            match effect {
                GoalEffect::Record(notice) => notices.push(notice),
                GoalEffect::InsertBudgetLimitPrompt => {
                    self.steer_running_turn(chat, prompts::budget_limit_prompt)
                }
                GoalEffect::InsertObjectiveUpdatedPrompt => {
                    self.steer_running_turn(chat, prompts::objective_updated_prompt)
                }
                GoalEffect::WithdrawContinuation => {
                    self.withdraw_ended_inputs(chat);
                }
                GoalEffect::StaleCheckDiscarded {
                    goal_id,
                    objective_revision,
                } => tracing::info!(
                    chat = %chat.snapshot.id,
                    %goal_id,
                    objective_revision,
                    "discarded a goal completion check for an earlier objective"
                ),
            }
        }
        let is_recorded = without_usage(chat.snapshot.goal.as_ref())
            != without_usage(previous.as_ref())
            || (turn_ends
                && chat
                    .snapshot
                    .goal
                    .as_ref()
                    .is_some_and(|goal| !goal.status.is_finished()));
        if is_recorded || !notices.is_empty() {
            let subject = chat.snapshot.goal.as_ref().or(previous.as_ref());
            self.record_goal(
                chat,
                subject.map(|goal| (goal.id, goal.objective.clone())),
                notices,
            );
        }
    }

    /// Adopts the goal another process stored since this one last did: it may have cleared,
    /// paused or edited it. What waits for a goal no longer active is withdrawn.
    pub(crate) async fn sync_goal_from_store(&self, chat_id: ChatId) {
        let Some(store) = self
            .inner
            .chats
            .lock()
            .await
            .get(&chat_id)
            .and_then(goal_store)
        else {
            return;
        };
        let stored = match tokio::task::spawn_blocking(move || store.read()).await {
            Ok(Ok(Some(stored))) => stored,
            Ok(Ok(None)) => return,
            Ok(Err(error)) => {
                tracing::warn!(chat = %chat_id, %error, "could not read the stored goal");
                return;
            }
            Err(error) => {
                tracing::warn!(chat = %chat_id, %error, "could not read the stored goal");
                return;
            }
        };
        let mut chats = self.inner.chats.lock().await;
        let Some(chat) = chats.get_mut(&chat_id) else {
            return;
        };
        if stored.generation <= chat.goal_generation {
            return;
        }
        let changed =
            without_usage(stored.goal.as_ref()) != without_usage(chat.snapshot.goal.as_ref());
        chat.snapshot.goal = stored.goal;
        chat.goal_generation = stored.generation;
        self.withdraw_ended_inputs(chat);
        self.save(&chat.snapshot);
        if changed {
            let receipt = Receipt {
                chat_id,
                loop_id: chat.snapshot.chat_loop.id,
                run_id: chat.snapshot.chat_loop.run_id.unwrap_or_default(),
            };
            let goal = chat.snapshot.goal.clone();
            self.publish_event(
                chat,
                &receipt,
                EventKind::GoalUpdated { goal, notice: None },
            );
        }
    }

    /// Writes the goal as it is now, one line for each notice, and tells every View.
    fn record_goal(
        &self,
        chat: &mut Chat,
        subject: Option<(Uuid, String)>,
        notices: Vec<GoalNotice>,
    ) {
        let chat_id = chat.snapshot.id;
        self.record_chat_metadata(
            chat_id,
            Line::Goal(GoalEntry {
                chat_id,
                cwd: chat.snapshot.folder.clone(),
                goal: chat.snapshot.goal.clone(),
            }),
        );
        if let Some((goal_id, objective)) = &subject {
            for notice in &notices {
                self.record_chat_metadata(
                    chat_id,
                    Line::GoalEvent(GoalEventEntry {
                        chat_id,
                        goal_id: *goal_id,
                        objective: objective.clone(),
                        notice: notice.clone(),
                        timestamp: format_timestamp(unix_time_milliseconds()),
                    }),
                );
            }
        }
        self.save(&chat.snapshot);
        let receipt = Receipt {
            chat_id,
            loop_id: chat.snapshot.chat_loop.id,
            run_id: chat.snapshot.chat_loop.run_id.unwrap_or_default(),
        };
        let goal = chat.snapshot.goal.clone();
        let notices = if notices.is_empty() {
            vec![None]
        } else {
            notices.into_iter().map(Some).collect()
        };
        for notice in notices {
            self.publish_event(
                chat,
                &receipt,
                EventKind::GoalUpdated {
                    goal: goal.clone(),
                    notice,
                },
            );
        }
    }

    /// Tells a running turn about its goal at its next tool boundary. A Chat without a running
    /// turn has nobody to tell: its next continuation carries the goal as it is.
    fn steer_running_turn(&self, chat: &mut Chat, prompt: fn(&GoalPromptContext<'_>) -> String) {
        if chat.active.is_none() {
            return;
        }
        let Some(goal) = &chat.snapshot.goal else {
            return;
        };
        let message = local_messages::goal_steering(&prompt(&prompt_context(goal)));
        if let Err(error) = self.append_local_messages(chat, [message]) {
            tracing::warn!(chat = %chat.snapshot.id, %error, "could not steer the running turn");
        }
    }

    /// A user's `/goal` command. Pausing and clearing leave the current turn running; setting
    /// or resuming a goal continues it at once when the Loop is idle.
    pub async fn manage_goal(&self, chat_id: ChatId, action: GoalAction) -> Result<(), AgentError> {
        let starts_work = matches!(action, GoalAction::Set { .. } | GoalAction::Resume);
        let event = match action {
            GoalAction::Set {
                objective,
                token_budget,
                replace,
            } => GoalEvent::Set {
                id: Uuid::new_v4(),
                objective,
                token_budget: token_budget.or_else(|| self.default_goal_budget()),
                replace,
            },
            GoalAction::Edit { objective } => GoalEvent::Edit { objective },
            GoalAction::Pause => GoalEvent::Pause,
            GoalAction::Resume => GoalEvent::Resume,
            GoalAction::Clear => GoalEvent::Clear,
            GoalAction::SetBudget { token_budget } => GoalEvent::SetBudget { token_budget },
        };
        if self
            .inner
            .chats
            .lock()
            .await
            .get(&chat_id)
            .ok_or(AgentError::NotFound)?
            .snapshot
            .delegation
            .is_some()
        {
            return Err(AgentError::Invalid(SUBAGENT_HAS_NO_GOAL.into()));
        }
        self.update_goal(chat_id, vec![event], false).await?;
        if starts_work {
            // Nothing ended a turn: the continuation is named by this command alone.
            self.continue_goal(chat_id, Uuid::new_v4()).await;
        }
        Ok(())
    }

    fn default_goal_budget(&self) -> Option<u64> {
        self.inner
            .settings
            .as_ref()
            .and_then(|settings| settings.goal_token_budget())
    }

    /// Applies an event that pauses an active goal: Esc, `/stop` or a history load.
    pub(crate) async fn pause_goal(&self, chat_id: ChatId, event: GoalEvent) {
        let has_goal = self
            .inner
            .chats
            .lock()
            .await
            .get(&chat_id)
            .is_some_and(|chat| chat.snapshot.goal.is_some());
        if has_goal && let Err(error) = self.update_goal(chat_id, vec![event], false).await {
            tracing::warn!(chat = %chat_id, %error, "could not pause the goal");
        }
    }

    /// An ordinary history load pauses the active goal of a root Chat it brought in, as the
    /// store keeps it.
    pub(crate) async fn pause_goal_after_history_load(&self, chat_id: ChatId) {
        self.sync_goal_from_store(chat_id).await;
        self.pause_goal(chat_id, GoalEvent::HistoryLoaded).await;
    }

    /// Charges the tokens a model reported for any member of the family to its root's goal.
    pub(crate) async fn charge_goal_tokens(&self, chat_id: ChatId, tokens: u64) {
        if tokens == 0 {
            return;
        }
        let root = {
            let chats = self.inner.chats.lock().await;
            let root = chats.get(&chat_id).map_or(chat_id, |chat| {
                chat.snapshot
                    .delegation
                    .as_ref()
                    .map_or(chat_id, |delegation| delegation.root_chat_id())
            });
            if chats
                .get(&root)
                .is_none_or(|chat| chat.snapshot.goal.is_none())
            {
                return;
            }
            root
        };
        if let Err(error) = self
            .update_goal(root, vec![GoalEvent::TokensUsed(tokens)], false)
            .await
        {
            tracing::warn!(chat = %root, %error, "could not charge tokens to the goal");
        }
    }

    /// What ending the turn `receipt` tells the root Chat's goal; `None` without a goal.
    pub(crate) fn goal_turn_events(
        &self,
        chat: &Chat,
        receipt: &Receipt,
        ending: GoalTurnEnd,
    ) -> Option<Vec<GoalEvent>> {
        if chat.snapshot.goal.is_none() || chat.snapshot.delegation.is_some() {
            return None;
        }
        Some(match ending {
            GoalTurnEnd::Completed => vec![GoalEvent::TurnEnded(turn_summary(
                chat,
                receipt.run_id,
                None,
            ))],
            GoalTurnEnd::Failed(error) => vec![GoalEvent::TurnEnded(turn_summary(
                chat,
                receipt.run_id,
                Some(error),
            ))],
            GoalTurnEnd::Interrupted => Vec::new(),
        })
    }

    /// The goal's part in ending a turn: the time it ran, what it did, and its usage recorded.
    pub(crate) async fn end_goal_turn(&self, chat_id: ChatId, events: Vec<GoalEvent>) {
        if let Err(error) = self.update_goal(chat_id, events, true).await {
            tracing::warn!(chat = %chat_id, %error, "could not end the goal's turn");
        }
    }

    /// Delivers one goal continuation through the wake-up base when the goal is active, the Loop
    /// receives and is idle with nothing queued, no background work of the Chat is still running
    /// and it is not in Plan mode. `after_run` names it in the ledger: the turn that just ended,
    /// so a repeat after a crash or from another process runs nothing again.
    pub(crate) async fn continue_goal(&self, chat_id: ChatId, after_run: RunId) {
        #[cfg(any(test, feature = "test-support"))]
        if self
            .inner
            .goal_continuations_held
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        self.sync_goal_from_store(chat_id).await;
        let delivery = {
            let chats = self.inner.chats.lock().await;
            let Some(chat) = chats.get(&chat_id) else {
                return;
            };
            let snapshot = &chat.snapshot;
            let conditions = ContinuationConditions {
                receiving: snapshot.chat_loop.receiving.is_receiving(),
                turn_ended: chat.active.is_none()
                    && chat.parked.is_empty()
                    && matches!(
                        snapshot.chat_loop.state,
                        LoopState::Idle | LoopState::Interrupted | LoopState::Failed
                    ),
                input_queue_empty: snapshot.pending_inputs.is_empty(),
                background_agents_running: background_children_run(&chats, chat_id),
                background_shells_running: background_shells_run(snapshot),
                plan_mode: snapshot.policy.mode == ExecutionMode::Plan,
            };
            if snapshot.delegation.is_some()
                || state::continuation_eligibility(snapshot.goal.as_ref(), conditions).is_err()
            {
                return;
            }
            let goal = snapshot.goal.as_ref().expect("an eligible goal exists");
            TriggerDelivery {
                source: TriggerSource::GoalContinuation {
                    goal_id: goal.id,
                    after_run,
                    objective_revision: goal.objective_revision,
                },
                input: prompts::continuation_prompt(&prompt_context(goal)),
                ledger: None,
            }
        };
        if let Err(error) = Box::pin(self.deliver_trigger(chat_id, delivery)).await {
            // The Chat's waiting input is retried with a backoff, and with it its goal.
            tracing::debug!(chat = %chat_id, %error, "the goal continuation waits");
            self.retry_waiting_trigger_input(chat_id);
        }
    }

    /// Considers continuing the goal again after what held it back may have ended: Plan mode
    /// left, a background command ended, a lease came free or a planned upgrade took over. It
    /// continues after the Loop's last ended turn, so a continuation already handed over for
    /// that turn runs nothing again. Its future is named, not inferred: continuing may start a
    /// run whose commands, once they end, consider the goal again.
    pub(crate) fn reconsider_goal(
        &self,
        chat_id: ChatId,
    ) -> futures_util::future::BoxFuture<'static, ()> {
        let agent = self.clone();
        Box::pin(async move {
            let after_run = agent
                .inner
                .chats
                .lock()
                .await
                .get(&chat_id)
                .and_then(|chat| chat.snapshot.chat_loop.run_id);
            agent
                .continue_goal(chat_id, after_run.unwrap_or_else(Uuid::new_v4))
                .await;
        })
    }

    /// The objective an active goal gives a Tick delivery as background.
    pub(crate) async fn goal_background(&self, chat_id: ChatId) -> Option<String> {
        let chats = self.inner.chats.lock().await;
        let goal = chats.get(&chat_id)?.snapshot.goal.as_ref()?;
        (goal.status == GoalStatus::Active).then(|| goal.objective.clone())
    }

    /// Runs GetGoal, CreateGoal or UpdateGoal for the root Agent.
    pub(crate) async fn goal_tool(
        &self,
        receipt: &Receipt,
        operation: &OperationRecord,
        cancel: CancellationToken,
    ) -> Result<ToolResult, String> {
        match operation.name.as_str() {
            GET_GOAL_TOOL_NAME => {
                super::tools::parse_get_goal(&operation.input)?;
                self.sync_goal_from_store(receipt.chat_id).await;
                let chats = self.inner.chats.lock().await;
                let chat = chats.get(&receipt.chat_id).ok_or("chat is unavailable")?;
                Ok(tool_result(
                    chat.snapshot.goal.as_ref().map_or(NO_GOAL.into(), |goal| {
                        serde_json::to_string_pretty(&goal_report(goal)).expect("a goal serializes")
                    }),
                    false,
                ))
            }
            CREATE_GOAL_TOOL_NAME => {
                let NewGoal {
                    objective,
                    token_budget,
                } = super::tools::parse_create_goal(&operation.input)?;
                let event = GoalEvent::Set {
                    id: Uuid::new_v4(),
                    objective,
                    token_budget: token_budget.or_else(|| self.default_goal_budget()),
                    replace: false,
                };
                Ok(match self.update_goal(receipt.chat_id, vec![event], false).await {
                    Ok(()) => tool_result(
                        "Goal set. The host continues it after this turn until a completion check verifies it.".into(),
                        false,
                    ),
                    Err(error) => tool_result(error.to_string(), true),
                })
            }
            UPDATE_GOAL_TOOL_NAME => match super::tools::parse_update_goal(&operation.input)? {
                GoalReport::Blocked { reason } => {
                    let blocked = GoalEvent::ReportedBlocked { reason };
                    Ok(match self.update_goal(receipt.chat_id, vec![blocked], false).await {
                            Ok(()) => tool_result(
                                "The goal is blocked. Stop working on it and tell the user what would unblock it; the user resumes it with /goal resume.".into(),
                                false,
                            ),
                            Err(error) => tool_result(error.to_string(), true),
                        })
                }
                GoalReport::Complete { evidence } => {
                    self.check_goal_completion(receipt, &evidence, cancel).await
                }
            },
            other => Err(format!("{other} is not a goal tool")),
        }
    }

    /// The completion check of an UpdateGoal `complete` report: one model call, without tools,
    /// over the objective, the report and the conversation already recorded. Its verdict applies
    /// only to the goal and objective revision the report was made under.
    async fn check_goal_completion(
        &self,
        receipt: &Receipt,
        evidence: &str,
        cancel: CancellationToken,
    ) -> Result<ToolResult, String> {
        let (goal_id, objective_revision, objective, conversation, checker) = {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&receipt.chat_id).ok_or("chat is unavailable")?;
            let Some(goal) = chat.snapshot.goal.as_ref() else {
                return Ok(tool_result(NO_GOAL.into(), true));
            };
            if goal.status.is_finished() {
                return Ok(tool_result("The goal is already complete.".into(), true));
            }
            let check_model = self
                .inner
                .settings
                .as_ref()
                .and_then(|settings| settings.goal_check_model());
            // A check model that cannot be reached is a failed check, like a failed call.
            let checker = match check_model {
                Some(model) => self
                    .inner
                    .models
                    .client(&model)
                    .map(|client| (client, model))
                    .map_err(|error| error.to_string()),
                None => Ok((chat.client.clone(), chat.snapshot.model.clone())),
            };
            (
                goal.id,
                goal.objective_revision,
                goal.objective.clone(),
                check::conversation_evidence(&chat.snapshot),
                checker,
            )
        };
        self.sync_goal_from_store(receipt.chat_id).await;
        let check_cancel = cancel.clone();
        let checked = match checker {
            Ok((client, model)) => {
                let request = check::check_request(
                    &objective,
                    evidence,
                    conversation,
                    check_budget_bytes(&model),
                );
                self.tracked_model_request(
                    RequestContext::for_run(receipt, ModelRequestKind::GoalCheck),
                    client,
                    request,
                    cancel,
                    None,
                )
                .await
                .map_err(|error| error.to_string())
            }
            Err(error) => Err(error),
        };
        let verdict = match checked {
            Ok(response) => {
                let verdict = check::parse_check_reply(&response.output).map_err(str::to_owned);
                let outcome = if verdict.is_ok() {
                    RequestOutcome::Accepted
                } else {
                    RequestOutcome::HostRejected(ResponseRejection::InvalidVerdict)
                };
                if let Err(error) = self
                    .finish_model_request(receipt.chat_id, response.sequence, outcome)
                    .await
                {
                    tracing::warn!(chat = %receipt.chat_id, %error, "could not settle the goal check request");
                }
                verdict
            }
            Err(error) => Err(error),
        };
        // A check stopped with its turn proves nothing either way.
        if check_cancel.is_cancelled() {
            return Err(
                "the turn was interrupted before the goal completion check finished".into(),
            );
        }
        let event = match &verdict {
            Ok(verdict) => GoalEvent::Checked {
                goal_id,
                objective_revision,
                verdict: verdict.clone(),
            },
            Err(error) => GoalEvent::CheckFailed {
                goal_id,
                objective_revision,
                error: format!("the goal completion check failed: {error}"),
            },
        };
        if let Err(error) = self.update_goal(receipt.chat_id, vec![event], false).await {
            return Ok(tool_result(error.to_string(), true));
        }
        let chats = self.inner.chats.lock().await;
        let chat = chats.get(&receipt.chat_id).ok_or("chat is unavailable")?;
        let still_current = chat.snapshot.goal.as_ref().is_some_and(|goal| {
            (goal.id, goal.objective_revision) == (goal_id, objective_revision)
        });
        Ok(match verdict {
            _ if !still_current => tool_result(
                "The goal changed while its completion was being checked; this report no longer applies. Read the goal with GetGoal.".into(),
                true,
            ),
            Ok(GoalVerdict::Met) => {
                let goal = chat.snapshot.goal.as_ref().expect("the checked goal exists");
                let usage = match goal.token_budget {
                    Some(budget) => format!(" Tokens used: {} of {budget}.", goal.tokens_used),
                    None => String::new(),
                };
                tool_result(
                    format!("The completion check verified the goal; it is complete.{usage}"),
                    false,
                )
            }
            Ok(GoalVerdict::NotMet { reason }) => tool_result(
                format!("The completion check did not accept the report, so the goal stays active. Its reason, to verify against the current state: {reason}"),
                true,
            ),
            Ok(GoalVerdict::Impossible { reason }) => tool_result(
                format!("The completion check found the objective impossible, so the goal is blocked: {reason}"),
                true,
            ),
            Err(error) => tool_result(
                format!("The completion check failed, so the goal is blocked: {error}"),
                true,
            ),
        })
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Agent {
    /// Keeps the host from starting goal continuations, for tests that hand them over through
    /// the wake-up base themselves.
    #[doc(hidden)]
    pub fn hold_goal_continuations_for_tests(&self) {
        self.inner
            .goal_continuations_held
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// The continuation a queued delivery made for `objective_revision` becomes when it is taken:
/// written again from the goal as it is, if its objective changed meanwhile.
pub(crate) fn current_continuation(goal: &GoalSnapshot, objective_revision: u64) -> Option<String> {
    (goal.objective_revision != objective_revision)
        .then(|| prompts::continuation_prompt(&prompt_context(goal)))
}

/// What GetGoal shows of the goal, with the remaining budget worked out.
fn goal_report(goal: &GoalSnapshot) -> serde_json::Value {
    let mut report = serde_json::to_value(goal).expect("a goal serializes");
    if let Some(budget) = goal.token_budget {
        report["tokens_remaining"] = budget.saturating_sub(goal.tokens_used).into();
    }
    report
}

/// The bytes of check payload the check model can read beside its reply.
fn check_budget_bytes(model: &ModelConfig) -> usize {
    let window = model
        .maximum_context_window_tokens()
        .unwrap_or(DEFAULT_CHECK_CONTEXT_WINDOW_TOKENS);
    window
        .saturating_sub(model.max_output_tokens as usize)
        .saturating_sub(REQUEST_OVERHEAD_TOKENS)
        .saturating_mul(crate::context::ESTIMATED_UTF8_BYTES_PER_TOKEN)
        .min(MAX_CHECK_PAYLOAD_BYTES)
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
