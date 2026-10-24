use std::path::PathBuf;

use tickloop_agent_protocol::{
    AccessError, AccessErrorCode, ObservationValue, Page, RecordedMessage, RecordedTranscript,
    SavedChat, TranscriptRef,
};

/// The saved Chats of `folder` under the config `root`, one page of them.
pub async fn saved_records(
    root: PathBuf,
    folder: PathBuf,
    page: Page,
) -> Result<ObservationValue, AccessError> {
    let directory = super::path::project_directory(&root, &folder);
    let records = tokio::task::spawn_blocking(move || super::listing::list_project(&directory))
        .await
        .map_err(|error| AccessError::new(AccessErrorCode::Storage, error.to_string()))?
        .map_err(|error| AccessError::new(AccessErrorCode::Storage, error.to_string()))?;
    Ok(ObservationValue::Records(
        records
            .into_iter()
            .skip(page.offset)
            .take(page.bounded_limit())
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
            .collect(),
    ))
}

/// One page of the messages of the saved Chat `record` names.
pub async fn transcript(
    record: TranscriptRef,
    page: Page,
) -> Result<ObservationValue, AccessError> {
    let requested = record.clone();
    let loaded = tokio::task::spawn_blocking(move || {
        let Some(path) = super::path::locate(&requested.store_root, requested.chat_id)? else {
            return Ok(None);
        };
        super::jsonl::load_file(&path)
    })
    .await
    .map_err(|error| AccessError::new(AccessErrorCode::Storage, error.to_string()))?
    .map_err(|error| AccessError::new(AccessErrorCode::Storage, error.to_string()))?
    .ok_or_else(|| {
        AccessError::new(
            AccessErrorCode::NotFound,
            "Transcript contains no conversation",
        )
    })?;
    let total_messages = loaded.chain.len();
    let folder = loaded
        .chain
        .first()
        .and_then(|entry| entry.cwd.clone())
        .unwrap_or_default();
    Ok(ObservationValue::Transcript(Box::new(RecordedTranscript {
        goal_history: tickloop_agent_transcript::display::goal_events(&[loaded
            .retained
            .snapshot(record.chat_id)])
        .into_iter()
        .filter(|entry| entry.chat_id == record.chat_id)
        .map(|entry| tickloop_agent_protocol::GoalHistoryEntry {
            after_message_id: entry.after_message_id,
            objective: entry.objective,
            notice: entry.notice,
            timestamp: entry.timestamp,
        })
        .collect(),
        record,
        title: loaded.metadata.custom_title.map(|entry| entry.custom_title),
        folder,
        total_messages,
        warnings: loaded.warnings,
        messages: loaded
            .chain
            .into_iter()
            .skip(page.offset)
            .take(page.bounded_limit())
            .map(|entry| RecordedMessage {
                id: entry.uuid,
                message: entry.message,
                loop_id: entry.loop_id,
                run_id: entry.run_id,
                timestamp: entry.timestamp,
            })
            .collect(),
    })))
}
