//! Where a root Chat's task boards stood along its conversation. When an answer of the root ends,
//! and when its boards change between answers, the boards are recorded at the conversation's last
//! message. The boards at any place on a chain are the ones recorded nearest above it, none being
//! no tasks: answering a prompt again starts from the boards before that prompt, and moving the
//! conversation elsewhere shows the boards where it now ends.

use std::collections::{BTreeMap, HashMap};

use tickloop_harness_protocol::{
    ChatId, ChatSnapshot, LoopId, RunId, SharedTaskStatus, TaskBoard, TaskMutationReceipt,
};
use tickloop_harness_transcript::display::{self, DisplayTaskBoard};
use tickloop_harness_transcript::{ChainEntry, Line, TaskBoardCheckpointEntry};
use uuid::Uuid;

use crate::{Agent, Chat, ChatBookkeeping};

/// A root Chat's boards by root Loop.
pub(crate) type TaskBoards = BTreeMap<LoopId, TaskBoard>;

/// The Runs that still run or wait to, by owner Chat and Run, with their family generation.
pub(crate) type LiveRuns = HashMap<(ChatId, RunId), u64>;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TaskBoardQueryError {
    #[error("task board leaf {0} is not present in the supplied Transcript")]
    LeafNotFound(Uuid),
    #[error("the supplied Transcript does not contain the ancestors of task board leaf {0}")]
    IncompleteHistory(Uuid),
}

/// The root Chat's task boards as they stood at a physical entry of its ordered Transcript Lines,
/// shown as [`display::task_boards`] shows the latest ones: boards without tasks are left out,
/// and nothing shows while Tasks are disabled. A Transcript that records no boards by place gives
/// its latest boards wherever the entry is.
/// This reads only the supplied values: it performs no I/O and acquires no Agent locks.
pub fn task_boards_at(
    lines: &[Line],
    physical_leaf: Uuid,
    enabled: bool,
) -> Result<Vec<DisplayTaskBoard>, TaskBoardQueryError> {
    let position = lines
        .iter()
        .position(|line| {
            line.chain_entry()
                .is_some_and(|entry| entry.uuid == physical_leaf)
        })
        .ok_or(TaskBoardQueryError::LeafNotFound(physical_leaf))?;
    let stored = crate::transcript::StoredTranscript {
        lines: lines[..=position].to_vec(),
        ..Default::default()
    };
    let (loaded, complete) = crate::transcript::assemble(&stored, Some(physical_leaf));
    let loaded = loaded
        .filter(|_| complete)
        .ok_or(TaskBoardQueryError::IncompleteHistory(physical_leaf))?;
    if !enabled {
        return Ok(Vec::new());
    }
    let mut checkpoints = HashMap::new();
    collect_checkpoints(lines, &mut checkpoints);
    if checkpoints.is_empty() {
        return Ok(display::task_boards(lines, enabled));
    }
    let boards = loaded
        .chain
        .iter()
        .rev()
        .find_map(|entry| checkpoints.get(&entry.uuid))
        .map(|checkpoint| checkpoint.boards.values().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    Ok(boards
        .into_iter()
        .filter(|board| !board.tasks.is_empty())
        .map(DisplayTaskBoard::from)
        .collect())
}

/// The latest checkpoint of each chain entry, in the Lines or the retained snapshots among them.
fn collect_checkpoints<'a>(
    lines: &'a [Line],
    checkpoints: &mut HashMap<Uuid, &'a TaskBoardCheckpointEntry>,
) {
    for line in lines {
        match line {
            Line::TaskBoardCheckpoint(entry) => {
                checkpoints.insert(entry.leaf, entry);
            }
            Line::RetainedSnapshot(snapshot) => collect_checkpoints(&snapshot.lines, checkpoints),
            _ => {}
        }
    }
}

/// The checkpoints `chat_id` recorded on a restored chain, by the message they stand at: a
/// checkpoint of an entry that is not a kept message stands at the kept message before it.
pub(crate) fn checkpoints_by_message(
    chain: &[ChainEntry],
    message_of_entry: &HashMap<Uuid, usize>,
    checkpoints: &HashMap<Uuid, TaskBoardCheckpointEntry>,
    chat_id: Uuid,
) -> BTreeMap<usize, TaskBoards> {
    let mut by_message = BTreeMap::new();
    let mut message = None;
    for entry in chain {
        if let Some(index) = message_of_entry.get(&entry.uuid) {
            message = Some(*index);
        }
        if let (Some(index), Some(checkpoint)) = (
            message,
            checkpoints
                .get(&entry.uuid)
                .filter(|checkpoint| checkpoint.chat_id == chat_id),
        ) {
            by_message.insert(
                index,
                checkpoint
                    .boards
                    .iter()
                    .map(|(loop_id, board)| (LoopId::from(*loop_id), board.clone()))
                    .collect(),
            );
        }
    }
    by_message
}

/// How the root Chat's conversation changed before its board follows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConversationChange {
    /// A prompt is answered again: regenerated, or edited and sent again.
    NewAnswer,
    /// The conversation now ends elsewhere: on another stored branch, at an earlier node, or
    /// rewound.
    Moved,
}

/// Puts the board of the root Chat's Loop where the conversation, which now stands before
/// message `before`, recorded it. Retries of mutations the board no longer holds report their
/// task cleared, its revision never goes back, and a restored claim whose Run no longer runs
/// needs attention. A board with no record there has no tasks; a Transcript that records no
/// boards by place keeps its board when the conversation moves. Tells whether the board changed.
pub(crate) fn follow_conversation(
    snapshot: &mut ChatSnapshot,
    bookkeeping: &mut ChatBookkeeping,
    before: usize,
    change: ConversationChange,
    live: &LiveRuns,
) -> Result<bool, String> {
    if snapshot.delegation.is_some()
        || (change == ConversationChange::Moved && !bookkeeping.has_task_board_checkpoints)
    {
        return Ok(false);
    }
    bookkeeping.task_board_checkpoints.split_off(&before);
    let loop_id = snapshot.chat_loop.id;
    let target = bookkeeping
        .task_board_checkpoints
        .values()
        .next_back()
        .and_then(|boards| boards.get(&loop_id));
    let current = snapshot.task_boards.get(&loop_id);
    let board = match (current, target) {
        (None, None) => return Ok(false),
        (Some(current), _) if target == Some(current) => return Ok(false),
        (Some(current), None) if current.tasks.is_empty() => return Ok(false),
        (Some(current), None) => {
            let mut board = current.clone();
            crate::shared_tasks::clear_board(&mut board)?;
            board
        }
        (current, Some(target)) => restored_board(current, target, live)?,
    };
    snapshot.task_boards.insert(loop_id, board);
    Ok(true)
}

fn restored_board(
    current: Option<&TaskBoard>,
    target: &TaskBoard,
    live: &LiveRuns,
) -> Result<TaskBoard, String> {
    let mut board = target.clone();
    board.revision = current
        .map_or(0, |current| current.revision)
        .max(target.revision)
        .checked_add(1)
        .ok_or("task board revision exhausted")?;
    if let Some(current) = current {
        board.is_team = current.is_team;
        for (id, mutation) in &current.mutations {
            board
                .mutations
                .entry(*id)
                .or_insert_with(|| TaskMutationReceipt {
                    task_cleared: true,
                    notification: None,
                    ..mutation.clone()
                });
        }
    }
    for task in &mut board.tasks {
        if task.status != SharedTaskStatus::InProgress {
            continue;
        }
        let generation = task
            .owner
            .as_ref()
            .zip(task.claim.as_ref())
            .and_then(|(owner, claim)| live.get(&(owner.chat_id, claim.run_id)));
        match (generation, task.claim.as_mut()) {
            (Some(generation), Some(claim)) => claim.generation = *generation,
            _ => {
                task.status = SharedTaskStatus::NeedsAttention;
                task.result = Some(
                    "The conversation moved to another point; reconcile the prior execution before explicit release."
                        .into(),
                );
                task.revision += 1;
            }
        }
    }
    Ok(board)
}

impl Agent {
    /// Records the root Chat's boards at its last message, unless they already stand there.
    pub(crate) fn record_task_board_checkpoint(&self, chat: &mut Chat) {
        if chat.snapshot.delegation.is_some() {
            return;
        }
        let Some(index) = chat.snapshot.messages.len().checked_sub(1) else {
            return;
        };
        let boards = &chat.snapshot.task_boards;
        let stands = match chat
            .bookkeeping
            .task_board_checkpoints
            .range(..=index)
            .next_back()
        {
            Some((_, recorded)) => recorded == boards,
            None => boards.values().all(|board| board.tasks.is_empty()),
        };
        if stands {
            return;
        }
        chat.bookkeeping
            .task_board_checkpoints
            .insert(index, boards.clone());
        chat.bookkeeping.has_task_board_checkpoints = true;
        self.record_task_board_line(&chat.snapshot, &chat.bookkeeping, index);
    }
}

#[cfg(test)]
#[path = "task_board_history_tests.rs"]
mod tests;
