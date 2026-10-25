//! The goal in a running Agent: the state machine's events come from the user's `/goal`
//! commands, the goal tools, Esc, `/stop`, history loads, model usage and each ended turn; its
//! effects reach the Transcript, the conversation, the event stream and the wake-up base, which
//! delivers every continuation.

use std::collections::HashMap;
use std::time::Duration;

use tickloop_harness_base::{private_fs::unix_time_milliseconds, timestamp::format_timestamp};
use tickloop_harness_model::{ModelConfigExt, REQUEST_OVERHEAD_TOKENS};
use tickloop_harness_protocol::{
    ChatId, CommandState, Content, EventKind, ExecutionMode, GoalAction, GoalAuthor, GoalNotice,
    GoalSnapshot, GoalStatus, GoalVerdict, LoopState, ModelConfig, ModelRequestKind,
    OperationRecord, Receipt, RequestOutcome, ResponseRejection, Role, RunId, ToolResult,
    TriggerIdentity, TriggerSource, Usage, UserShellState, local_messages,
};
use tickloop_harness_transcript::{GoalEntry, GoalEventEntry, Line};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::check;
use super::prompts::{self, GoalPromptContext};
use super::state::{
    self, ContinuationConditions, GoalEffect, GoalError, GoalEvent, GoalTransition, TurnSummary,
};
use super::tools::{
    CREATE_GOAL_TOOL_NAME, GET_GOAL_TOOL_NAME, GoalReport, NewGoal, UPDATE_GOAL_TOOL_NAME,
};
use crate::model_request::RequestContext;
use crate::wakeup::{SourceChange, SourceChangeError, TriggerDelivery};
use crate::{Agent, AgentError, Chat};

/// The usage a goal may gain before it is recorded again between turn ends: at most this much
/// goes uncounted when the process crashes.
const USAGE_RECORD_TOKENS: u64 = 20_000;
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

/// Why a goal change did not happen, or was not recorded.
#[derive(Debug, thiserror::Error)]
pub(crate) enum GoalUpdateError {
    #[error(transparent)]
    Goal(#[from] GoalError),
    #[error("the Chat is no longer loaded")]
    NotLoaded,
    /// Another process holds the Chat, or this one cannot take it now.
    #[error("{0}")]
    Elsewhere(String),
    /// Made here, and not recorded: this process keeps the Chat, and its next change writes the
    /// record again.
    #[error("the goal changed here but could not be recorded: {0}")]
    Unrecorded(String),
}

/// What a goal change refused for the lease another live process holds tells the user.
const GOAL_HELD_ELSEWHERE: &str = "this Chat is running in another Tickloop process; change its goal there, or once that process leaves it";

impl From<SourceChangeError> for GoalUpdateError {
    fn from(error: SourceChangeError) -> Self {
        match error {
            SourceChangeError::HeldElsewhere => Self::Elsewhere(GOAL_HELD_ELSEWHERE.into()),
            SourceChangeError::NotLoaded => Self::NotLoaded,
            SourceChangeError::Unrecorded(error) => Self::Unrecorded(error),
            error @ (SourceChangeError::ForeignHost(_)
            | SourceChangeError::Closing
            | SourceChangeError::Busy) => Self::Elsewhere(error.to_string()),
        }
    }
}

impl From<GoalUpdateError> for AgentError {
    fn from(error: GoalUpdateError) -> Self {
        match error {
            GoalUpdateError::Goal(error) => invalid(error),
            GoalUpdateError::NotLoaded => AgentError::NotFound,
            error @ (GoalUpdateError::Elsewhere(_) | GoalUpdateError::Unrecorded(_)) => {
                AgentError::Invalid(error.to_string())
            }
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

fn background_shells_run(snapshot: &tickloop_harness_protocol::ChatSnapshot) -> bool {
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

impl Agent {
    /// A change of the goal by the user, the Agent's report or the host: made through the wake-up
    /// base under the Chat's lease within its Trigger boundary, so it applies to the goal the
    /// last holder left, no other process changes it meanwhile, and its record is written before
    /// the lease goes back. While another live process holds the Chat, the change is refused
    /// with why.
    pub(crate) async fn change_goal(
        &self,
        chat_id: ChatId,
        events: Vec<GoalEvent>,
    ) -> Result<(), GoalUpdateError> {
        self.run_source_change(chat_id, move |change, chat| {
            change.agent().update_goal(change, chat, events, false)
        })
        .await?
    }

    /// Whether this process keeps the Chat's goal accounts: it holds the Chat's lease and took
    /// over what its last holder left, or its host has no other process to share the Chat with.
    /// The usage and time of a turn the user ran in another process than the holder are not the
    /// goal's, and this process does not take the lease for them.
    fn keeps_goal_accounts(&self, chat_id: ChatId) -> bool {
        self.inner.recorder.is_none()
            || self.inner.waits == crate::WaitHosting::Durable
            || self.inner.chat_leases.holds_synced(chat_id)
    }

    /// Accounts for the goal of a turn this process keeps the accounts of, through the wake-up
    /// base: a lease that went in the meantime is taken again with the goal its holder left.
    async fn account_goal(&self, chat_id: ChatId, events: Vec<GoalEvent>, turn_ends: bool) {
        if !self.keeps_goal_accounts(chat_id) {
            tracing::info!(chat = %chat_id.uuid(), "not counted: another process keeps the goal");
            return;
        }
        let accounted = self
            .run_source_change(chat_id, move |change, chat| {
                change.agent().update_goal(change, chat, events, turn_ends)
            })
            .await;
        if let Err(error) = accounted
            .map_err(GoalUpdateError::from)
            .and_then(|result| result)
        {
            tracing::warn!(chat = %chat_id.uuid(), %error, "could not account for the goal");
        }
    }

    /// Applies `events` to the Chat's goal, then carries out what they ask for, under the lease
    /// `change` proves. The goal first gets the running time since it was last settled, while a
    /// turn runs or as `turn_ends`.
    pub(crate) fn update_goal(
        &self,
        change: &SourceChange,
        chat: &mut Chat,
        events: Vec<GoalEvent>,
        turn_ends: bool,
    ) -> Result<(), GoalUpdateError> {
        let now_ms = unix_time_milliseconds();
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
        let (goal, effects) = apply_events(chat.snapshot.goal.as_ref(), settled, now_ms)?;
        chat.goal_clock_ms = now_ms;
        self.adopt_goal(chat, goal, effects, turn_ends, &|line| {
            change.record_source_state(line)
        });
        Ok(())
    }

    /// Takes on a change of the goal, then its effects and its record. A record is written when
    /// anything but the running usage changed, when a turn ends with an unfinished goal, and
    /// once the usage grew by [`USAGE_RECORD_TOKENS`] since the last record, which bounds what a
    /// crash leaves uncounted. `record` writes the goal's record as the source's state.
    fn adopt_goal(
        &self,
        chat: &mut Chat,
        goal: Option<GoalSnapshot>,
        effects: Vec<GoalEffect>,
        turn_ends: bool,
        record: &dyn Fn(Line),
    ) {
        let previous = std::mem::replace(&mut chat.snapshot.goal, goal);
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
                    chat = %chat.snapshot.id.uuid(),
                    %goal_id,
                    objective_revision,
                    "discarded a goal completion check for an earlier objective"
                ),
            }
        }
        let goal = chat.snapshot.goal.as_ref();
        let is_unfinished = goal.is_some_and(|goal| !goal.status.is_finished());
        let unrecorded_tokens = goal.map_or(0, |goal| {
            goal.tokens_used.saturating_sub(chat.goal_recorded_tokens)
        });
        let is_recorded = without_usage(goal) != without_usage(previous.as_ref())
            || (turn_ends && is_unfinished)
            || unrecorded_tokens >= USAGE_RECORD_TOKENS;
        if !is_recorded && notices.is_empty() {
            return;
        }
        let subject = goal.or(previous.as_ref());
        let subject = subject.map(|goal| (goal.id, goal.objective.clone()));
        self.record_goal(chat, subject, notices, record);
    }

    /// Queues the goal as it is now for the Transcript through `record`, one line for each
    /// notice, and tells every View. Its usage counts as recorded from now on: the base writes
    /// the record before it lets the Chat go, and one it could not write stays queued for the
    /// next change of the Chat to write.
    fn record_goal(
        &self,
        chat: &mut Chat,
        subject: Option<(Uuid, String)>,
        notices: Vec<GoalNotice>,
        record: &dyn Fn(Line),
    ) {
        let chat_id = chat.snapshot.id;
        chat.goal_recorded_tokens = chat
            .snapshot
            .goal
            .as_ref()
            .map_or(0, |goal| goal.tokens_used);
        record(goal_line(chat));
        if let Some((goal_id, objective)) = &subject {
            let after_message_id = chat
                .snapshot
                .message_ids
                .last_key_value()
                .map(|(_, id)| *id);
            for notice in &notices {
                let timestamp = format_timestamp(unix_time_milliseconds());
                chat.snapshot
                    .goal_history
                    .push(tickloop_harness_protocol::GoalHistoryEntry {
                        after_message_id,
                        objective: objective.clone(),
                        notice: notice.clone(),
                        timestamp: timestamp.clone(),
                    });
                self.record_chat_metadata(
                    chat_id,
                    Line::GoalEvent(GoalEventEntry {
                        after_message_id,
                        chat_id: chat_id.into(),
                        goal_id: *goal_id,
                        objective: objective.clone(),
                        notice: notice.clone(),
                        timestamp,
                    }),
                );
            }
        }
        self.save(&chat.snapshot);
        let receipt = Receipt {
            chat_id,
            loop_id: chat.snapshot.chat_loop.id,
            run_id: chat.snapshot.chat_loop.run_id.unwrap_or(RunId::nil()),
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
            tracing::warn!(chat = %chat.snapshot.id.uuid(), %error, "could not steer the running turn");
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
                id: tickloop_ids::generate(),
                objective,
                token_budget: token_budget.or_else(|| self.default_goal_budget()),
                replace,
                set_by: GoalAuthor::User,
            },
            GoalAction::Edit { objective } => GoalEvent::Edit { objective },
            GoalAction::Pause => GoalEvent::Pause,
            GoalAction::Resume => GoalEvent::Resume,
            GoalAction::Clear => GoalEvent::Clear,
            GoalAction::SetBudget { token_budget } => GoalEvent::SetBudget { token_budget },
        };
        {
            let mut chats = self.inner.chats.lock().await;
            let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
            if chat.snapshot.delegation.is_some() {
                return Err(AgentError::Invalid(SUBAGENT_HAS_NO_GOAL.into()));
            }
            // The user's own command here decides the goal; the history-load rule no longer
            // applies to it.
            chat.loaded_from_history = false;
        }
        self.change_goal(chat_id, vec![event]).await?;
        if starts_work {
            // Nothing ended a turn: the continuation is named by this command alone.
            self.continue_goal(chat_id, tickloop_ids::generate().into())
                .await;
        }
        Ok(())
    }

    fn default_goal_budget(&self) -> Option<u64> {
        self.inner
            .settings
            .as_ref()
            .and_then(|settings| settings.read(None).goal_token_budget)
    }

    /// Applies Esc's pause of an active goal; a failure is told with the Chat's Trigger notice.
    pub(crate) async fn pause_goal(&self, chat_id: ChatId, event: GoalEvent) {
        let has_goal = self
            .inner
            .chats
            .lock()
            .await
            .get(&chat_id)
            .is_some_and(|chat| chat.snapshot.goal.is_some());
        if has_goal && let Err(error) = self.change_goal(chat_id, vec![event]).await {
            tracing::warn!(chat = %chat_id.uuid(), %error, "could not pause the goal");
            self.inner.chat_leases.set_notice(
                chat_id,
                tickloop_harness_protocol::LoopTriggerNoticeKind::Warning,
                format!("the goal was not paused: {error}"),
            );
        }
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
        self.account_goal(root, vec![GoalEvent::TokensUsed(tokens)], false)
            .await;
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
        self.account_goal(chat_id, events, true).await;
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
                // The goal's usage so far is recorded with the acceptance, before its turn.
                on_accept: Some(Box::new(
                    |change: &SourceChange, chat: &mut Chat, _: &Receipt| {
                        if let Some(goal) = &mut chat.snapshot.goal {
                            goal.last_continued_at_ms = Some(
                                unix_time_milliseconds().max(
                                    goal.last_continued_at_ms
                                        .unwrap_or_default()
                                        .saturating_add(1),
                                ),
                            );
                        }
                        change.agent().record_goal(chat, None, Vec::new(), &|line| {
                            change.record_source_state(line)
                        });
                    },
                )),
            }
        };
        if let Err(error) = Box::pin(self.deliver_trigger(chat_id, delivery)).await {
            // The Chat's waiting input is retried with a backoff, and with it its goal.
            tracing::debug!(chat = %chat_id.uuid(), %error, "the goal continuation waits");
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
                .continue_goal(chat_id, after_run.unwrap_or_else(RunId::new))
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
                    id: tickloop_ids::generate(),
                    objective,
                    token_budget: token_budget.or_else(|| self.default_goal_budget()),
                    replace: false,
                    set_by: GoalAuthor::Agent,
                };
                Ok(match self.change_goal(receipt.chat_id, vec![event]).await {
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
                    Ok(match self.change_goal(receipt.chat_id, vec![blocked]).await {
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
                .and_then(|settings| settings.read(None).goal_check_model);
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
                    tracing::warn!(chat = %receipt.chat_id.uuid(), %error, "could not settle the goal check request");
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
        if let Err(error) = self.change_goal(receipt.chat_id, vec![event]).await {
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

/// The goal as a Trigger source's own state: a process that takes the Chat's lease over
/// rebuilds it from the Transcript, so it goes on from what the last holder recorded.
pub(crate) struct GoalSourceState;

impl crate::wakeup::TriggerSourceState for GoalSourceState {
    fn take_over(&self, change: &SourceChange, chat: &mut Chat, stored: &[Line]) {
        let agent = change.agent();
        if chat.snapshot.delegation.is_some() {
            return;
        }
        let mut retained = crate::transcript::RetainedFold::default();
        for line in stored {
            retained.apply(line);
        }
        chat.snapshot.goal_history = tickloop_harness_transcript::display::goal_events(&[
            retained.snapshot(chat.snapshot.id.into())
        ])
        .into_iter()
        .map(|entry| tickloop_harness_protocol::GoalHistoryEntry {
            after_message_id: entry.after_message_id,
            objective: entry.objective,
            notice: entry.notice,
            timestamp: entry.timestamp,
        })
        .collect();
        let mut goal = tickloop_harness_transcript::display::goal(stored);
        chat.goal_recorded_tokens = goal.as_ref().map_or(0, |goal| goal.tokens_used);
        // What this process counted for the same goal and has not recorded yet still counts.
        if let (Some(goal), Some(held)) = (goal.as_mut(), chat.snapshot.goal.as_ref())
            && goal.id == held.id
        {
            goal.tokens_used = goal.tokens_used.max(held.tokens_used);
            goal.time_used_ms = goal.time_used_ms.max(held.time_used_ms);
        }
        let changed = goal != chat.snapshot.goal;
        chat.snapshot.goal = goal;
        agent.withdraw_ended_inputs(chat);
        // The history-load rule: the holder that left an active goal is gone, and this process
        // took the Chat over from its history.
        if std::mem::take(&mut chat.loaded_from_history)
            && chat
                .snapshot
                .goal
                .as_ref()
                .is_some_and(|goal| goal.status == GoalStatus::Active)
        {
            match apply_events(
                chat.snapshot.goal.as_ref(),
                vec![GoalEvent::HistoryLoaded],
                unix_time_milliseconds(),
            ) {
                Ok((goal, effects)) => agent.adopt_goal(chat, goal, effects, false, &|line| {
                    change.record_source_state(line)
                }),
                Err(error) => {
                    tracing::warn!(chat = %chat.snapshot.id.uuid(), %error, "could not pause the goal")
                }
            }
        } else if changed {
            let receipt = Receipt {
                chat_id: chat.snapshot.id,
                loop_id: chat.snapshot.chat_loop.id,
                run_id: chat.snapshot.chat_loop.run_id.unwrap_or(RunId::nil()),
            };
            let goal = chat.snapshot.goal.clone();
            agent.publish_event(
                chat,
                &receipt,
                EventKind::GoalUpdated { goal, notice: None },
            );
        }
    }

    fn records(&self, chat: &Chat) -> Vec<Line> {
        if chat.snapshot.delegation.is_some() || chat.snapshot.goal.is_none() {
            return Vec::new();
        }
        vec![goal_line(chat)]
    }

    /// A stop pauses an active goal and withdraws its continuation, where the lease is.
    fn on_stop(&self, change: &SourceChange, chat: &mut Chat) {
        if chat.snapshot.delegation.is_some() || chat.snapshot.goal.is_none() {
            return;
        }
        if let Err(error) =
            change
                .agent()
                .update_goal(change, chat, vec![GoalEvent::Stopped], false)
        {
            tracing::warn!(chat = %chat.snapshot.id.uuid(), %error, "could not pause the goal");
        }
    }
}

/// The record of the Chat's goal as it is now.
fn goal_line(chat: &Chat) -> Line {
    Line::Goal(GoalEntry {
        chat_id: chat.snapshot.id.into(),
        cwd: chat.snapshot.folder.clone(),
        goal: chat.snapshot.goal.clone(),
    })
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
