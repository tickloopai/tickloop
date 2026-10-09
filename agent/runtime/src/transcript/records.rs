use std::path::PathBuf;

use tickloop_agent_protocol::{
    AccessError, AccessErrorCode, ObservationValue, Page, RecordedMessage, RecordedTranscript,
    SavedChat, TranscriptRef,
};

/// The saved Chats of `workdir` under the config `root`, one page of them.
pub async fn saved_records(
    root: PathBuf,
    workdir: PathBuf,
    page: Page,
) -> Result<ObservationValue, AccessError> {
    let directory = super::project_directory(&root, &workdir);
    let records = tokio::task::spawn_blocking(move || super::list_project(&directory))
        .await
        .map_err(|error| AccessError::new(AccessErrorCode::Storage, error.to_string()))?
        .map_err(|error| AccessError::new(AccessErrorCode::Storage, error.to_string()))?;
    Ok(ObservationValue::Records(
        records
            .into_iter()
            .skip(page.offset)
            .take(page.bounded_limit())
            .map(|summary| SavedChat {
                id: summary.chat_id,
                title: summary.title,
                first_prompt: summary.first_prompt,
                last_prompt: summary.last_prompt,
                modified: summary.modified,
                cwd: summary.cwd,
                branch_source: summary.forked_from.map(|origin| origin.chat_id),
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
        let Some(path) = super::locate(&requested.store_root, requested.chat_id)? else {
            return Ok(None);
        };
        super::load(&path)
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
    let workdir = loaded
        .chain
        .first()
        .map_or_else(PathBuf::new, |entry| entry.cwd.clone());
    Ok(ObservationValue::Transcript(Box::new(RecordedTranscript {
        record,
        title: loaded.metadata.custom_title.map(|entry| entry.custom_title),
        workdir,
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
