use serde::{Deserialize, Serialize};

use crate::{ExecutionMode, LoopId};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Plan {
    pub loop_id: LoopId,
    pub version: u64,
    pub markdown: String,
    /// The execution mode selected for this process-local plan approval.
    pub approved_execution_mode: Option<ExecutionMode>,
    #[serde(default)]
    pub approval_pending: bool,
    /// The user replaced the model's text in plan review; process-local like the approved mode.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub edited_by_user: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

/// Claude Code's `TodoItemSchema`: `content` is the imperative form and `activeForm` the present
/// continuous form, which names the current activity while the todo is in progress.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
    pub active_form: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Progress {
    pub loop_id: LoopId,
    pub todos: Vec<TodoItem>,
}
