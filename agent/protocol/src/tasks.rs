use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{ChatId, LoopId, RunId};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SharedTaskStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
    NeedsAttention,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TaskOwner {
    pub chat_id: ChatId,
    pub name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TaskClaim {
    pub run_id: RunId,
    pub generation: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SharedTask {
    pub id: Uuid,
    pub title: String,
    pub description: String,
    pub dependencies: Vec<Uuid>,
    pub status: SharedTaskStatus,
    pub owner: Option<TaskOwner>,
    pub claim: Option<TaskClaim>,
    #[serde(default)]
    pub delegated_runs: Vec<TaskDelegate>,
    pub revision: u64,
    pub result: Option<String>,
    #[serde(default)]
    pub report: Option<TaskReport>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TaskMutationReceipt {
    pub actor: ChatId,
    pub input: String,
    pub task_id: Uuid,
    #[serde(default)]
    pub task_cleared: bool,
    #[serde(default)]
    pub notification: Option<TaskAssignment>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TaskBoard {
    #[serde(default = "legacy_team_board")]
    pub is_team: bool,
    pub root_chat_id: ChatId,
    pub root_loop_id: LoopId,
    pub revision: u64,
    #[serde(default)]
    pub task_number_offset: u64,
    pub tasks: Vec<SharedTask>,
    #[serde(default)]
    pub mutations: BTreeMap<Uuid, TaskMutationReceipt>,
}

fn legacy_team_board() -> bool {
    true
}

impl TaskBoard {
    pub fn new(root_chat_id: ChatId, root_loop_id: LoopId) -> Self {
        Self {
            is_team: false,
            root_chat_id,
            root_loop_id,
            revision: 0,
            task_number_offset: 0,
            tasks: Vec::new(),
            mutations: BTreeMap::new(),
        }
    }

    pub fn task_number(&self, index: usize) -> Option<u64> {
        self.tasks.get(index)?;
        self.task_number_offset
            .checked_add(u64::try_from(index).ok()?)?
            .checked_add(1)
    }

    pub fn blocked_by(&self, task: &SharedTask) -> Vec<Uuid> {
        task.dependencies
            .iter()
            .copied()
            .filter(|id| {
                !self.tasks.iter().any(|dependency| {
                    dependency.id == *id && dependency.status == SharedTaskStatus::Completed
                })
            })
            .collect()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TaskExecutionBinding {
    pub root_chat_id: ChatId,
    pub root_loop_id: LoopId,
    pub task_id: Uuid,
    pub revision: u64,
    pub run_id: RunId,
    pub generation: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TaskReport {
    pub chat_id: ChatId,
    pub run_id: RunId,
    pub call_id: String,
    pub review: Option<crate::ApprovalAssessmentDetails>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TaskAssignment {
    pub target: ChatId,
    pub message: String,
    pub delivered: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TaskDelegate {
    pub chat_id: ChatId,
    pub loop_id: LoopId,
    pub run_id: RunId,
}
