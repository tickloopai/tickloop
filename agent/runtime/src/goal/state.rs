//! A Chat's goal as a pure state machine. The runtime feeds it the user's `/goal` actions, the
//! Agent's reports and what happened to each turn, and carries out the effects it returns; the
//! continuation conditions are judged here too, so every goal rule lives in one place.

use tickloop_agent_protocol::{
    GoalAuthor, GoalBlock, GoalCheck, GoalNotice, GoalPause, GoalSnapshot, GoalStatus, GoalVerdict,
};
use uuid::Uuid;

use super::tools::MAX_OBJECTIVE_CHARACTERS;

/// Consecutive quiet continuations after which the goal is blocked.
pub(crate) const MAX_QUIET_CONTINUATIONS: u32 = 3;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GoalEvent {
    /// `/goal <objective>` or CreateGoal, with the Settings default budget already resolved.
    Set {
        id: Uuid,
        objective: String,
        token_budget: Option<u64>,
        /// Whether the user confirmed replacing an unfinished goal.
        replace: bool,
        set_by: GoalAuthor,
    },
    Edit {
        objective: String,
    },
    Pause,
    Resume,
    Clear,
    SetBudget {
        token_budget: Option<u64>,
    },
    TurnEnded(TurnSummary),
    /// Tokens any model of the Agent family reported; only an active goal counts them.
    TokensUsed(u64),
    /// Running time since the last report.
    TimeElapsed {
        ms: u64,
    },
    /// The completion check's verdict on an UpdateGoal `complete` report, for the goal and
    /// objective revision the report was made under.
    Checked {
        goal_id: Uuid,
        objective_revision: u64,
        verdict: GoalVerdict,
    },
    /// The completion check itself failed, which counts as an execution error.
    CheckFailed {
        goal_id: Uuid,
        objective_revision: u64,
        error: String,
    },
    /// UpdateGoal `blocked`.
    ReportedBlocked {
        reason: String,
    },
    /// Esc.
    Interrupted,
    /// `/stop`.
    Stopped,
    /// An ordinary history load; a planned upgrade's live handover is not one.
    HistoryLoaded,
}

/// How a turn that ended while the goal existed went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TurnSummary {
    /// Whether a goal continuation started the turn, rather than the user.
    pub continuation: bool,
    pub called_tools: bool,
    pub replied_substantively: bool,
    /// The execution error that ended the turn abnormally. A failed tool call is not one.
    pub failure: Option<String>,
}

/// What the runtime does after a transition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GoalEffect {
    /// Writes a goal line to the Transcript and shows it in the conversation.
    Record(GoalNotice),
    /// Lets the current turn wrap up with the budget-limit prompt.
    InsertBudgetLimitPrompt,
    /// Tells the current turn that the objective changed.
    InsertObjectiveUpdatedPrompt,
    /// Withdraws a goal continuation that is queued but not yet taken.
    WithdrawContinuation,
    /// Records that a completion check finished after its objective was edited, replaced or
    /// cleared, and was ignored.
    StaleCheckDiscarded {
        goal_id: Uuid,
        objective_revision: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GoalTransition {
    pub goal: Option<GoalSnapshot>,
    pub effects: Vec<GoalEffect>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum GoalError {
    #[error("this Chat has no goal")]
    NoGoal,
    #[error("this Chat already has an unfinished goal; replace it explicitly or clear it first")]
    UnfinishedGoal,
    #[error("invalid objective: {0}")]
    InvalidObjective(String),
    #[error("a token budget must be positive")]
    InvalidBudget,
    #[error("cannot {action} a goal that is {status}")]
    Inapplicable {
        action: &'static str,
        status: &'static str,
    },
    #[error("the goal's token budget is exhausted; raise or remove the budget before resuming")]
    BudgetExhausted,
}

/// Why the host does not continue the goal now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NoContinuation {
    NoGoal,
    NotActive,
    NotReceiving,
    TurnRunning,
    InputQueued,
    BackgroundAgentsRunning,
    BackgroundShellsRunning,
    PlanMode,
}

/// The Loop's state when the host considers continuing the goal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ContinuationConditions {
    pub receiving: bool,
    pub turn_ended: bool,
    pub input_queue_empty: bool,
    pub background_agents_running: bool,
    pub background_shells_running: bool,
    pub plan_mode: bool,
}

pub(crate) fn continuation_eligibility(
    goal: Option<&GoalSnapshot>,
    conditions: ContinuationConditions,
) -> Result<(), NoContinuation> {
    let goal = goal.ok_or(NoContinuation::NoGoal)?;
    if goal.status != GoalStatus::Active {
        return Err(NoContinuation::NotActive);
    }
    if !conditions.receiving {
        return Err(NoContinuation::NotReceiving);
    }
    if !conditions.turn_ended {
        return Err(NoContinuation::TurnRunning);
    }
    if !conditions.input_queue_empty {
        return Err(NoContinuation::InputQueued);
    }
    if conditions.background_agents_running {
        return Err(NoContinuation::BackgroundAgentsRunning);
    }
    if conditions.background_shells_running {
        return Err(NoContinuation::BackgroundShellsRunning);
    }
    if conditions.plan_mode {
        return Err(NoContinuation::PlanMode);
    }
    Ok(())
}

/// Applies `event` to the Chat's goal at `now_ms`. User actions and Agent reports that do not
/// apply fail; host events that do not apply leave the goal unchanged.
pub(crate) fn apply(
    goal: Option<&GoalSnapshot>,
    event: GoalEvent,
    now_ms: u64,
) -> Result<GoalTransition, GoalError> {
    let Some(current) = goal else {
        return match event {
            GoalEvent::Set {
                id,
                objective,
                token_budget,
                replace,
                set_by,
            } => set(None, id, &objective, token_budget, replace, set_by, now_ms),
            GoalEvent::Checked {
                goal_id,
                objective_revision,
                ..
            }
            | GoalEvent::CheckFailed {
                goal_id,
                objective_revision,
                ..
            } => Ok(GoalTransition {
                goal: None,
                effects: vec![GoalEffect::StaleCheckDiscarded {
                    goal_id,
                    objective_revision,
                }],
            }),
            GoalEvent::Edit { .. }
            | GoalEvent::Pause
            | GoalEvent::Resume
            | GoalEvent::Clear
            | GoalEvent::SetBudget { .. }
            | GoalEvent::ReportedBlocked { .. } => Err(GoalError::NoGoal),
            GoalEvent::TurnEnded(_)
            | GoalEvent::TokensUsed(_)
            | GoalEvent::TimeElapsed { .. }
            | GoalEvent::Interrupted
            | GoalEvent::Stopped
            | GoalEvent::HistoryLoaded => Ok(GoalTransition {
                goal: None,
                effects: Vec::new(),
            }),
        };
    };
    let mut next = current.clone();
    let mut effects = Vec::new();
    let inapplicable = |action| GoalError::Inapplicable {
        action,
        status: status_name(&current.status),
    };
    match event {
        GoalEvent::Set {
            id,
            objective,
            token_budget,
            replace,
            set_by,
        } => {
            return set(
                Some(current),
                id,
                &objective,
                token_budget,
                replace,
                set_by,
                now_ms,
            );
        }
        GoalEvent::Edit { objective } => {
            if current.status.is_finished() {
                return Err(inapplicable("edit"));
            }
            next.objective = valid_objective(&objective)?;
            next.objective_revision = next.objective_revision.saturating_add(1);
            // Only the user edits a goal.
            next.set_by = GoalAuthor::User;
            next.last_check = None;
            next.quiet_turns = 0;
            // `/goal pause` does not interrupt the current turn, so a paused goal's turn may
            // still be working on the old objective; the runtime inserts this only into a
            // running turn.
            effects.push(GoalEffect::InsertObjectiveUpdatedPrompt);
        }
        GoalEvent::Pause => {
            if current.status != GoalStatus::Active {
                return Err(inapplicable("pause"));
            }
            pause(&mut next, GoalPause::User, &mut effects);
        }
        GoalEvent::Resume => match current.status {
            GoalStatus::Paused { .. } | GoalStatus::Blocked { .. } | GoalStatus::BudgetLimited => {
                if current.budget_exhausted() {
                    return Err(GoalError::BudgetExhausted);
                }
                next.status = GoalStatus::Active;
                next.quiet_turns = 0;
            }
            GoalStatus::Active | GoalStatus::Complete => return Err(inapplicable("resume")),
        },
        GoalEvent::Clear => {
            effects.push(GoalEffect::WithdrawContinuation);
            effects.push(GoalEffect::Record(GoalNotice::Cleared));
            return Ok(GoalTransition {
                goal: None,
                effects,
            });
        }
        GoalEvent::SetBudget { token_budget } => {
            if current.status.is_finished() {
                return Err(inapplicable("change the budget of"));
            }
            validate_budget(token_budget)?;
            next.token_budget = token_budget;
            limit_if_exhausted(&mut next, &mut effects);
        }
        GoalEvent::TurnEnded(summary) => {
            if current.status == GoalStatus::Active {
                end_turn(&mut next, summary, &mut effects);
            }
        }
        GoalEvent::TokensUsed(tokens) => {
            if current.status == GoalStatus::Active {
                next.tokens_used = next.tokens_used.saturating_add(tokens);
                limit_if_exhausted(&mut next, &mut effects);
            }
        }
        GoalEvent::TimeElapsed { ms } => {
            if current.status == GoalStatus::Active {
                next.time_used_ms = next.time_used_ms.saturating_add(ms);
            }
        }
        GoalEvent::Checked {
            goal_id,
            objective_revision,
            ..
        }
        | GoalEvent::CheckFailed {
            goal_id,
            objective_revision,
            ..
        } if (goal_id, objective_revision) != (current.id, current.objective_revision) => {
            effects.push(GoalEffect::StaleCheckDiscarded {
                goal_id,
                objective_revision,
            });
        }
        GoalEvent::CheckFailed { error, .. } => {
            if current.status.is_finished() {
                return Err(inapplicable("check"));
            }
            block(
                &mut next,
                GoalBlock::ExecutionFailed { error },
                &mut effects,
            );
        }
        GoalEvent::Checked { verdict, .. } => {
            if current.status.is_finished() {
                return Err(inapplicable("check"));
            }
            match &verdict {
                GoalVerdict::Met => leave_active(
                    &mut next,
                    GoalStatus::Complete,
                    GoalNotice::Completed,
                    &mut effects,
                ),
                GoalVerdict::NotMet { .. } => {}
                GoalVerdict::Impossible { reason } => block(
                    &mut next,
                    GoalBlock::CheckImpossible {
                        reason: reason.clone(),
                    },
                    &mut effects,
                ),
            }
            next.last_check = Some(GoalCheck {
                verdict,
                checked_at_ms: now_ms,
            });
        }
        GoalEvent::ReportedBlocked { reason } => match current.status {
            GoalStatus::Active | GoalStatus::Paused { .. } => {
                block(&mut next, GoalBlock::Reported { reason }, &mut effects)
            }
            GoalStatus::Blocked { .. } | GoalStatus::BudgetLimited | GoalStatus::Complete => {
                return Err(inapplicable("block"));
            }
        },
        GoalEvent::Interrupted => pause_if_active(&mut next, GoalPause::Interrupted, &mut effects),
        GoalEvent::Stopped => pause_if_active(&mut next, GoalPause::Stopped, &mut effects),
        GoalEvent::HistoryLoaded => {
            pause_if_active(&mut next, GoalPause::HistoryLoaded, &mut effects)
        }
    }
    if next != *current {
        next.updated_at_ms = now_ms;
    }
    Ok(GoalTransition {
        goal: Some(next),
        effects,
    })
}

/// Sets a new goal, replacing an unfinished one only when the user confirmed it. Usage starts
/// from zero.
fn set(
    current: Option<&GoalSnapshot>,
    id: Uuid,
    objective: &str,
    token_budget: Option<u64>,
    replace: bool,
    set_by: GoalAuthor,
    now_ms: u64,
) -> Result<GoalTransition, GoalError> {
    let objective = valid_objective(objective)?;
    validate_budget(token_budget)?;
    let mut effects = Vec::new();
    if current.is_some_and(|goal| !goal.status.is_finished()) {
        if !replace {
            return Err(GoalError::UnfinishedGoal);
        }
        effects.push(GoalEffect::WithdrawContinuation);
    }
    effects.push(GoalEffect::Record(GoalNotice::Set));
    Ok(GoalTransition {
        goal: Some(GoalSnapshot {
            id,
            objective,
            objective_revision: current.map_or(1, |goal| goal.objective_revision.saturating_add(1)),
            set_by,
            status: GoalStatus::Active,
            token_budget,
            tokens_used: 0,
            time_used_ms: 0,
            last_check: None,
            quiet_turns: 0,
            last_continued_at_ms: None,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        }),
        effects,
    })
}

fn end_turn(goal: &mut GoalSnapshot, summary: TurnSummary, effects: &mut Vec<GoalEffect>) {
    if let Some(error) = summary.failure {
        block(goal, GoalBlock::ExecutionFailed { error }, effects);
        return;
    }
    // A user-triggered turn never counts as quiet, but it breaks a run of quiet continuations.
    if !summary.continuation || summary.called_tools || summary.replied_substantively {
        goal.quiet_turns = 0;
        return;
    }
    goal.quiet_turns += 1;
    if goal.quiet_turns >= MAX_QUIET_CONTINUATIONS {
        block(goal, GoalBlock::NoProgress, effects);
    }
}

fn limit_if_exhausted(goal: &mut GoalSnapshot, effects: &mut Vec<GoalEffect>) {
    if goal.status == GoalStatus::Active && goal.budget_exhausted() {
        effects.push(GoalEffect::InsertBudgetLimitPrompt);
        leave_active(
            goal,
            GoalStatus::BudgetLimited,
            GoalNotice::BudgetLimited,
            effects,
        );
    }
}

fn pause_if_active(goal: &mut GoalSnapshot, by: GoalPause, effects: &mut Vec<GoalEffect>) {
    if goal.status == GoalStatus::Active {
        pause(goal, by, effects);
    }
}

fn pause(goal: &mut GoalSnapshot, by: GoalPause, effects: &mut Vec<GoalEffect>) {
    leave_active(
        goal,
        GoalStatus::Paused { by },
        GoalNotice::Paused { by },
        effects,
    );
}

fn block(goal: &mut GoalSnapshot, by: GoalBlock, effects: &mut Vec<GoalEffect>) {
    leave_active(
        goal,
        GoalStatus::Blocked { by: by.clone() },
        GoalNotice::Blocked { by },
        effects,
    );
}

fn leave_active(
    goal: &mut GoalSnapshot,
    status: GoalStatus,
    notice: GoalNotice,
    effects: &mut Vec<GoalEffect>,
) {
    goal.status = status;
    effects.push(GoalEffect::WithdrawContinuation);
    effects.push(GoalEffect::Record(notice));
}

fn valid_objective(objective: &str) -> Result<String, GoalError> {
    let objective = objective.trim();
    if objective.is_empty() {
        return Err(GoalError::InvalidObjective("the objective is empty".into()));
    }
    if objective.chars().count() > MAX_OBJECTIVE_CHARACTERS {
        return Err(GoalError::InvalidObjective(format!(
            "the objective exceeds {MAX_OBJECTIVE_CHARACTERS} characters; put longer material in a file or Artifact and reference it"
        )));
    }
    Ok(objective.into())
}

fn validate_budget(token_budget: Option<u64>) -> Result<(), GoalError> {
    if token_budget == Some(0) {
        return Err(GoalError::InvalidBudget);
    }
    Ok(())
}

fn status_name(status: &GoalStatus) -> &'static str {
    match status {
        GoalStatus::Active => "active",
        GoalStatus::Paused { .. } => "paused",
        GoalStatus::Blocked { .. } => "blocked",
        GoalStatus::BudgetLimited => "budget_limited",
        GoalStatus::Complete => "complete",
    }
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;
