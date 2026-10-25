use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A Chat's goal: the completion condition the root Agent keeps pursuing across turns until a
/// completion check verifies it. A Chat has at most one.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct GoalSnapshot {
    pub id: Uuid,
    pub objective: String,
    /// Grows whenever the objective is set, replaced or edited, so a completion check started
    /// for an earlier objective can be recognized and ignored.
    pub objective_revision: u64,
    /// Who wrote the objective as it is: only the user's own `/goal` speaks for the user.
    #[serde(default)]
    pub set_by: GoalAuthor,
    pub status: GoalStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<u64>,
    /// Tokens the whole Agent family's models reported while the goal was active since it was
    /// set; pausing and resuming do not reset it.
    pub tokens_used: u64,
    /// Time spent while the goal was active.
    pub time_used_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_check: Option<GoalCheck>,
    /// Consecutive goal continuations that neither called a tool nor gave a substantive reply.
    pub quiet_turns: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_continued_at_ms: Option<u64>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

impl GoalSnapshot {
    pub fn budget_exhausted(&self) -> bool {
        self.token_budget
            .is_some_and(|budget| self.tokens_used >= budget)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalAuthor {
    /// The user set or edited it with `/goal`.
    User,
    /// The Agent set it with CreateGoal; it is the Agent's account of what the user asked.
    #[default]
    Agent,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum GoalStatus {
    Active,
    Paused {
        by: GoalPause,
    },
    Blocked {
        by: GoalBlock,
    },
    /// The token budget ran out; resuming needs a larger budget or none.
    BudgetLimited,
    Complete,
}

impl GoalStatus {
    /// Whether a new goal can be set without replacing this one.
    pub fn is_finished(&self) -> bool {
        matches!(self, Self::Complete)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalPause {
    /// `/goal pause`.
    User,
    /// Esc interrupted the current turn.
    Interrupted,
    /// `/stop`.
    Stopped,
    /// An ordinary history load reopened the Chat.
    HistoryLoaded,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GoalBlock {
    /// The Agent reported a blocker with UpdateGoal.
    Reported { reason: String },
    /// Three consecutive continuations neither called a tool nor replied substantively.
    NoProgress,
    /// An execution error ended a turn abnormally.
    ExecutionFailed { error: String },
    /// The completion check found the objective impossible.
    CheckImpossible { reason: String },
}

/// A goal change the conversation shows as one line.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GoalNotice {
    Set,
    Completed,
    Blocked { by: GoalBlock },
    BudgetLimited,
    Paused { by: GoalPause },
    Cleared,
}

/// The completion check's latest conclusion about a reported completion.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct GoalCheck {
    pub verdict: GoalVerdict,
    pub checked_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GoalVerdict {
    Met,
    /// The goal stays active; the reason guides the next continuation.
    NotMet {
        reason: String,
    },
    Impossible {
        reason: String,
    },
}

/// A user's `/goal` command.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum GoalAction {
    Set {
        objective: String,
        /// `None` takes the Settings default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token_budget: Option<u64>,
        /// Confirms replacing an unfinished goal.
        #[serde(default)]
        replace: bool,
    },
    Edit {
        objective: String,
    },
    Pause,
    Resume,
    Clear,
    SetBudget {
        /// `None` removes the budget.
        token_budget: Option<u64>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct GoalHistoryEntry {
    pub after_message_id: Option<Uuid>,
    pub objective: String,
    pub notice: GoalNotice,
    pub timestamp: String,
}
