use std::path::PathBuf;

use tickloop_code_access::{
    AccessError, ObservationValue, Page, RecordedMessage, RecordedTranscript, TranscriptRef,
};
use tickloop_harness::runtime::{AgentError, transcript::records};

use crate::access_error::agent_access_error;

pub(crate) async fn saved_records(
    root: PathBuf,
    folder: PathBuf,
    page: Page,
) -> Result<ObservationValue, AccessError> {
    records::saved_records(root, folder, page.offset, page.bounded_limit())
        .await
        .map(ObservationValue::Records)
        .map_err(agent_access_error)
}

pub(crate) async fn transcript(
    record: TranscriptRef,
    page: Page,
) -> Result<ObservationValue, AccessError> {
    let transcript = records::transcript(
        record.store_root.clone(),
        record.chat_id,
        page.offset,
        page.bounded_limit(),
    )
    .await
    .map_err(|error| match error {
        AgentError::NotFound => AccessError::not_found("Transcript contains no conversation"),
        error => agent_access_error(error),
    })?;
    Ok(ObservationValue::Transcript(Box::new(RecordedTranscript {
        record,
        goal_history: transcript.goal_history,
        title: transcript.title,
        folder: transcript.folder,
        total_messages: transcript.total_messages,
        warnings: transcript.warnings,
        messages: transcript
            .messages
            .into_iter()
            .map(|message| RecordedMessage {
                id: message.id,
                message: message.message,
                loop_id: message.loop_id,
                run_id: message.run_id,
                timestamp: message.timestamp,
            })
            .collect(),
    })))
}
