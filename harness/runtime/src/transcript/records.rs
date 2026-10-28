use std::path::PathBuf;

use crate::AgentError;
use tickloop_harness_protocol::{GoalHistoryEntry, Message, SavedChat};
use uuid::Uuid;

pub struct TranscriptPage {
    pub goal_history: Vec<GoalHistoryEntry>,
    pub title: Option<String>,
    pub folder: PathBuf,
    pub messages: Vec<TranscriptMessage>,
    pub total_messages: usize,
    pub warnings: Vec<String>,
}

pub struct TranscriptMessage {
    pub id: Uuid,
    pub message: Message,
    pub loop_id: Option<Uuid>,
    pub run_id: Option<Uuid>,
    pub timestamp: String,
}

/// The saved Chats of `folder` under the config `root`, one page of them.
pub async fn saved_records(
    root: PathBuf,
    folder: PathBuf,
    offset: usize,
    limit: usize,
) -> Result<Vec<SavedChat>, AgentError> {
    let directory = super::path::project_directory(&root, &folder);
    let records = tokio::task::spawn_blocking(move || super::listing::list_project(&directory))
        .await
        .map_err(|error| AgentError::Storage(error.to_string()))?
        .map_err(|error| AgentError::Storage(error.to_string()))?;
    Ok(records
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|summary| SavedChat {
            id: summary.chat_id.into(),
            title: summary.title,
            first_prompt: summary.first_prompt,
            last_prompt: summary.last_prompt,
            modified: summary.modified,
            cwd: summary.cwd,
            branch_source: summary.forked_from.map(|origin| origin.chat_id.into()),
            git_branch: summary.git_branch,
        })
        .collect())
}

/// The root Chat of the family a saved Subagent belongs to; `None` for a Chat of its own or one
/// with no Transcript under the configuration root.
pub async fn saved_family_root(root: PathBuf, chat_id: Uuid) -> Result<Option<Uuid>, AgentError> {
    tokio::task::spawn_blocking(move || match super::path::locate(&root, chat_id)? {
        Some(path) => super::listing::family_root(&path),
        None => Ok(None),
    })
    .await
    .map_err(|error| AgentError::Storage(error.to_string()))?
    .map_err(|error| AgentError::Storage(error.to_string()))
}

/// One page of the messages of a saved Chat under the configuration root.
pub async fn transcript(
    root: PathBuf,
    chat_id: Uuid,
    offset: usize,
    limit: usize,
) -> Result<TranscriptPage, AgentError> {
    let loaded = tokio::task::spawn_blocking(move || {
        let Some(path) = super::path::locate(&root, chat_id)? else {
            return Ok(None);
        };
        super::jsonl::load_file(&path)
    })
    .await
    .map_err(|error| AgentError::Storage(error.to_string()))?
    .map_err(|error| AgentError::Storage(error.to_string()))?
    .ok_or(AgentError::NotFound)?;
    let total_messages = loaded.chain.len();
    let folder = loaded
        .chain
        .first()
        .and_then(|entry| entry.cwd.clone())
        .unwrap_or_default();
    Ok(TranscriptPage {
        goal_history: tickloop_harness_transcript::display::goal_events(&[loaded
            .retained
            .snapshot(chat_id)])
        .into_iter()
        .filter(|entry| entry.chat_id == chat_id)
        .map(|entry| tickloop_harness_protocol::GoalHistoryEntry {
            after_message_id: entry.after_message_id,
            objective: entry.objective,
            notice: entry.notice,
            timestamp: entry.timestamp,
        })
        .collect(),
        title: loaded.metadata.custom_title.map(|entry| entry.custom_title),
        folder,
        total_messages,
        warnings: loaded.warnings,
        messages: loaded
            .chain
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|entry| TranscriptMessage {
                id: entry.uuid,
                message: entry.message,
                loop_id: entry.loop_id,
                run_id: entry.run_id,
                timestamp: entry.timestamp,
            })
            .collect(),
    })
}
