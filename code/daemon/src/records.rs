use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use tickloop_code_access::{
    AccessError, AccessErrorCode, ObservationValue, Page, RecordedMessage, RecordedTranscript,
    TranscriptRef,
};
use tickloop_harness::runtime::{
    AgentError,
    transcript::{JsonlTranscriptStore, LoadRange, TranscriptStore, records},
};
use tickloop_harness::transcript::{AcpSessionEntry, Line};
use uuid::Uuid;

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

/// The root Chat of the family the saved `record` belongs to, when it is a Subagent's.
pub(crate) async fn family_root(record: &TranscriptRef) -> Result<Option<Uuid>, AccessError> {
    records::saved_family_root(record.store_root.clone(), record.chat_id)
        .await
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

/// Reads only explicit ACP origin metadata. Ordinary legacy Chats never acquire ACP identity.
pub(crate) async fn acp_session(
    record: &TranscriptRef,
) -> Result<Option<AcpSessionEntry>, AccessError> {
    if !record.store_root.is_dir() {
        return Ok(None);
    }
    let store = JsonlTranscriptStore::open(record.store_root.clone())
        .map_err(|e| AccessError::storage(e.to_string()))?;
    let Some(stored) = store
        .load(record.chat_id.into(), LoadRange::Full)
        .await
        .map_err(|e| AccessError::storage(e.to_string()))?
    else {
        return Ok(None);
    };
    let mut latest = None;
    fn absorb(lines: &[Line], latest: &mut Option<AcpSessionEntry>) {
        for line in lines {
            match line {
                Line::AcpSession(entry) => *latest = Some(entry.clone()),
                Line::RetainedSnapshot(snapshot) => absorb(&snapshot.lines, latest),
                _ => {}
            }
        }
    }
    absorb(&stored.retained, &mut latest);
    absorb(&stored.lines, &mut latest);
    Ok(latest.filter(|entry| entry.chat_id == record.chat_id))
}

/// Reads the public history exactly once; managed hosts retain this snapshot across pages.
pub(crate) async fn managed_history_snapshot(
    record: &TranscriptRef,
) -> Result<tickloop_harness::runtime::transcript::DisplayHistorySnapshot, AccessError> {
    let store = JsonlTranscriptStore::open(record.store_root.clone())
        .map_err(|e| AccessError::storage(e.to_string()))?;
    let stored = store
        .load(record.chat_id.into(), LoadRange::Full)
        .await
        .map_err(|e| AccessError::storage(e.to_string()))?
        .ok_or_else(|| AccessError::not_found("ACP history not found"))?;
    stored
        .display_history(None)
        .ok_or_else(|| AccessError::invalid("history branch cannot be reconstructed"))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListCursor {
    root: PathBuf,
    folder: PathBuf,
    fingerprint: u64,
    offset: usize,
}

pub(crate) async fn managed_sessions(
    launch: tickloop_code_access::LaunchSpec,
    cursor: Option<String>,
) -> Result<tickloop_code_access::ManagedSessionPage, AccessError> {
    use std::hash::{Hash, Hasher};
    if launch.profile != tickloop_code_access::ExecutionProfile::Acp || !launch.session_persistence
    {
        return Err(AccessError::invalid(
            "listing requires a persistent ACP scope",
        ));
    }
    let root = launch
        .config_root
        .ok_or_else(|| AccessError::invalid("ACP listing requires a configuration root"))?
        .canonicalize()
        .map_err(|e| AccessError::invalid(e.to_string()))?;
    let folder = launch
        .folder
        .canonicalize()
        .map_err(|e| AccessError::invalid(e.to_string()))?;
    let store = JsonlTranscriptStore::open(root.clone())
        .map_err(|e| AccessError::storage(e.to_string()))?;
    let list_folder = folder.clone();
    let summaries = tokio::task::spawn_blocking(move || store.project_acp_chats(&list_folder))
        .await
        .map_err(|e| AccessError::storage(e.to_string()))?
        .map_err(|e| AccessError::storage(e.to_string()))?;
    let sessions: Vec<_> = summaries
        .into_iter()
        .filter_map(|summary| {
            let origin = summary.acp_session?;
            if origin.chat_id != summary.chat_id
                || !origin.origin.matches_current(&folder, &root).ok()?
            {
                return None;
            }
            Some(tickloop_code_access::ManagedSessionInfo {
                session_id: summary.chat_id,
                cwd: folder.clone(),
                title: summary.title,
                updated_at: tickloop_harness::base::timestamp::format_timestamp(
                    summary
                        .modified
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis()
                        .min(u64::MAX as u128) as u64,
                ),
            })
        })
        .collect();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_vec(&sessions)
        .map_err(|e| AccessError::invalid(e.to_string()))?
        .hash(&mut hasher);
    let fingerprint = hasher.finish();
    let offset = if let Some(cursor) = cursor {
        if cursor.len() > 4096 {
            return Err(AccessError::invalid("listing cursor too large"));
        }
        let previous: ListCursor = serde_json::from_str(&cursor)
            .map_err(|_| AccessError::invalid("invalid listing cursor"))?;
        if previous.root != root || previous.folder != folder || previous.fingerprint != fingerprint
        {
            return Err(AccessError::new(
                AccessErrorCode::VersionConflict,
                "session listing changed; restart listing",
            ));
        }
        previous.offset
    } else {
        0
    };
    if offset > sessions.len() {
        return Err(AccessError::invalid("invalid listing offset"));
    }
    let end = offset.saturating_add(100).min(sessions.len());
    let next_cursor = (end < sessions.len())
        .then(|| {
            serde_json::to_string(&ListCursor {
                root,
                folder,
                fingerprint,
                offset: end,
            })
        })
        .transpose()
        .map_err(|e| AccessError::invalid(e.to_string()))?;
    Ok(tickloop_code_access::ManagedSessionPage {
        sessions: sessions[offset..end].to_vec(),
        next_cursor,
    })
}
