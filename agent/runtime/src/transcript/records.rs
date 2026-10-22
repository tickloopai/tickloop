use super::{JsonlTranscriptStore, LoadRange, TranscriptStore};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tickloop_agent_transcript::{AcpSessionEntry, Line};

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
    let store = JsonlTranscriptStore::open(record.store_root.clone())
        .map_err(|e| AccessError::storage(e.to_string()))?;
    let stored = store
        .load(record.chat_id, LoadRange::Full)
        .await
        .map_err(|e| AccessError::storage(e.to_string()))?
        .ok_or_else(|| AccessError::not_found("Transcript not found"))?;
    let loaded = super::reader::assemble(&stored, None)
        .0
        .ok_or_else(|| AccessError::not_found("Transcript contains no conversation"))?;
    let total_messages = loaded.chain.len();
    let folder = loaded
        .chain
        .first()
        .and_then(|entry| entry.cwd.clone())
        .unwrap_or_default();
    Ok(ObservationValue::Transcript(Box::new(RecordedTranscript {
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

/// Reads only explicit ACP origin metadata. Ordinary legacy Chats never acquire ACP identity.
pub async fn acp_session(record: &TranscriptRef) -> Result<Option<AcpSessionEntry>, AccessError> {
    if !record.store_root.is_dir() {
        return Ok(None);
    }
    let store = JsonlTranscriptStore::open(record.store_root.clone())
        .map_err(|e| AccessError::storage(e.to_string()))?;
    let Some(stored) = store
        .load(record.chat_id, LoadRange::Full)
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

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryCursor {
    record: TranscriptRef,
    position: u64,
    offset: usize,
}

/// Reads the public history exactly once; managed hosts retain this snapshot across pages.
pub async fn managed_history_snapshot(
    record: &TranscriptRef,
) -> Result<super::DisplayHistorySnapshot, AccessError> {
    let store = JsonlTranscriptStore::open(record.store_root.clone())
        .map_err(|e| AccessError::storage(e.to_string()))?;
    let stored = store
        .load(record.chat_id, LoadRange::Full)
        .await
        .map_err(|e| AccessError::storage(e.to_string()))?
        .ok_or_else(|| AccessError::not_found("ACP history not found"))?;
    stored
        .display_history(None)
        .ok_or_else(|| AccessError::invalid("history branch cannot be reconstructed"))
}

pub async fn managed_history(
    record: TranscriptRef,
    cursor: Option<String>,
) -> Result<tickloop_agent_protocol::ManagedHistoryPage, AccessError> {
    let store = JsonlTranscriptStore::open(record.store_root.clone())
        .map_err(|e| AccessError::storage(e.to_string()))?;
    let stored = store
        .load(record.chat_id, LoadRange::Full)
        .await
        .map_err(|e| AccessError::storage(e.to_string()))?
        .ok_or_else(|| AccessError::not_found("ACP history not found"))?;
    let offset = if let Some(cursor) = cursor {
        if cursor.len() > 4096 {
            return Err(AccessError::invalid("history cursor too large"));
        }
        let previous: HistoryCursor = serde_json::from_str(&cursor)
            .map_err(|_| AccessError::invalid("invalid history cursor"))?;
        if previous.record != record || previous.position != stored.position.0 {
            return Err(AccessError::new(
                AccessErrorCode::VersionConflict,
                "history snapshot changed; restart replay",
            ));
        }
        previous.offset
    } else {
        0
    };
    let history = stored
        .display_history(None)
        .ok_or_else(|| AccessError::invalid("history branch cannot be reconstructed"))?;
    let page = history
        .page(offset, 100)
        .map_err(|e| AccessError::invalid(e.to_string()))?;
    let next_cursor = page
        .next_offset
        .map(|offset| {
            serde_json::to_string(&HistoryCursor {
                record: record.clone(),
                position: page.position.0,
                offset,
            })
        })
        .transpose()
        .map_err(|e| AccessError::invalid(e.to_string()))?;
    Ok(tickloop_agent_protocol::ManagedHistoryPage {
        session_id: record.chat_id,
        items: page
            .items
            .into_iter()
            .map(serde_json::to_value)
            .collect::<Result<_, _>>()
            .map_err(|e| AccessError::invalid(e.to_string()))?,
        next_cursor,
    })
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListCursor {
    root: PathBuf,
    folder: PathBuf,
    fingerprint: u64,
    offset: usize,
}

pub async fn managed_sessions(
    launch: tickloop_agent_protocol::LaunchSpec,
    cursor: Option<String>,
) -> Result<tickloop_agent_protocol::ManagedSessionPage, AccessError> {
    use std::hash::{Hash, Hasher};
    if launch.profile != tickloop_agent_protocol::ExecutionProfile::Acp
        || !launch.session_persistence
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
            Some(tickloop_agent_protocol::ManagedSessionInfo {
                session_id: summary.chat_id,
                cwd: folder.clone(),
                title: summary.title,
                updated_at: tickloop_agent_base::timestamp::format_timestamp(
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
    Ok(tickloop_agent_protocol::ManagedSessionPage {
        sessions: sessions[offset..end].to_vec(),
        next_cursor,
    })
}
