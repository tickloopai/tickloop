mod chunked;
mod embedded;
mod ipc;
mod owner;
mod protocol;
mod receipts;
mod records;
mod recovering;
mod views;

pub use embedded::EmbeddedAgentAccess;
pub use ipc::{
    LocalAgentClient, serve_connection, serve_listener, serve_owner_listener, serve_worker_listener,
};
pub use owner::{OwnerServer, WorkerFactory};
pub use protocol::*;
pub use recovering::{ReconnectFactory, RecoveringAgentClient, worker_link};

use async_trait::async_trait;
use tokio::sync::mpsc;

#[derive(Debug)]
pub struct Observation {
    pub initial: ObservationValue,
    pub cursor: Cursor,
    pub reset: Option<ResetReason>,
    pub(crate) changes: Option<mpsc::Receiver<ObservationUpdate>>,
    pub(crate) terminal_error: Option<std::sync::Arc<std::sync::Mutex<Option<AccessError>>>>,
}

impl Observation {
    pub async fn recv(&mut self) -> Result<ObservationUpdate, AccessError> {
        match self.changes.as_mut() {
            Some(changes) => changes.recv().await.ok_or_else(|| {
                self.terminal_error
                    .as_ref()
                    .and_then(|error| error.lock().unwrap().clone())
                    .unwrap_or_else(|| AccessError::unavailable("observation disconnected"))
            }),
            None => Err(AccessError::invalid("this observation is a single query")),
        }
    }
}

#[async_trait]
pub trait AgentAccess: Send + Sync {
    async fn observe(&self, query: ObservationRequest) -> Result<Observation, AccessError>;
    async fn execute(&self, command: CommandEnvelope) -> Result<CommandReceipt, AccessError>;
    async fn show_view(&self, update: DisplayUpdate) -> Result<DisplayReceipt, AccessError>;
}

pub(crate) fn summary_run_id(
    loops: &[crate::runtime::LoopSnapshot],
    state: &crate::runtime::LoopState,
) -> Option<crate::runtime::RunId> {
    loops
        .iter()
        .filter(|goal| &goal.state == state)
        .max_by_key(|goal| goal.state_since)
        .and_then(|goal| goal.run_id)
}

/// The Loop state an Agent row reports: the goal most in need of attention, so a later completed
/// Loop cannot mask an earlier goal that still waits for input, plan review or its Trigger, and an
/// active user Shell keeps the Agent working. Without Loops, the latest user Shell decides between
/// a confirmed failure and Idle; an unknown Shell outcome is not reported as a failure.
/// Whether an execution ever received work. Configuration changes alone leave it empty.
pub(crate) fn has_work(
    conversation: &crate::runtime::ChatSnapshot,
    user_shells: &[crate::runtime::UserShellRecord],
    queued: usize,
) -> bool {
    !conversation.loops.is_empty()
        || !conversation.messages.is_empty()
        || !user_shells.is_empty()
        || queued > 0
}

/// The latest moment a Loop or user Shell of the execution entered its current state. Reading,
/// displaying or configuring an Agent never moves it; Loops and Shells without a recorded time
/// leave it unknown when nothing else is known.
pub(crate) fn updated_at(
    loops: &[crate::runtime::LoopSnapshot],
    user_shells: &[crate::runtime::UserShellRecord],
) -> Option<u64> {
    loops
        .iter()
        .map(|goal| goal.state_since)
        .chain(user_shells.iter().map(|shell| shell.state_since))
        .flatten()
        .max()
}

const PREVIEW_MAX_CHARACTERS: usize = 160;

/// The latest conversation content as one bounded line: the newest reply, or the newest typed
/// prompt with its text (an image-only prompt reads as an image cue). Tool traffic, thinking and
/// local notices are skipped; text that only repeats `title` adds nothing.
pub(crate) fn preview(conversation: &crate::runtime::ChatSnapshot, title: &str) -> Option<String> {
    use crate::model::{Content, Role};
    let text = conversation
        .messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, message)| {
            if conversation
                .message_ids
                .get(&index)
                .is_some_and(|id| conversation.agent_message_origins.contains_key(id))
            {
                return None;
            }
            let texts = message
                .content
                .iter()
                .filter_map(|content| match content {
                    Content::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            let text = match message.role {
                Role::Assistant => texts.last().copied()?.to_owned(),
                Role::User
                    if message
                        .content
                        .iter()
                        .any(|content| matches!(content, Content::ToolResult { .. })) =>
                {
                    return None;
                }
                Role::User if texts.iter().all(|text| text.trim().is_empty()) => {
                    let images = message
                        .content
                        .iter()
                        .filter(|content| matches!(content, Content::Image { .. }))
                        .count();
                    match images {
                        0 => return None,
                        1 => "[image]".to_owned(),
                        images => format!("[{images} images]"),
                    }
                }
                Role::User => texts.join("\n"),
                _ => return None,
            };
            (!text.trim().is_empty()
                && matches!(
                    crate::runtime::LocalMessage::parse(&text),
                    None | Some(crate::runtime::LocalMessage::AgentCommunication { .. })
                ))
            .then_some(text)
        })?;
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line == title.split_whitespace().collect::<Vec<_>>().join(" ") {
        return None;
    }
    Some(line.chars().take(PREVIEW_MAX_CHARACTERS).collect())
}

pub(crate) fn bounded_update(mut update: ObservationUpdate) -> ObservationUpdate {
    if serde_json::to_vec(&update).map_or(true, |bytes| {
        bytes.len() > crate::protocol::FEED_CHANGE_BYTES
    }) {
        update.change = ObservationChange::Resync {
            reason: ResetReason::Lagged,
        };
    }
    update
}
