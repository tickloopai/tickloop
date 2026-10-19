use super::*;
use serde::{Deserializer, Serializer};

#[derive(Serialize, Deserialize)]
struct AttachedRead {
    path: PathBuf,
    content: String,
    modified: std::time::SystemTime,
    offset: usize,
    limit: Option<usize>,
    is_full_read: bool,
}

pub(super) fn checkpoint_reads(
    reads: &[(PathBuf, tickloop_agent_protocol::FileReadRecord)],
) -> Value {
    serde_json::to_value(
        reads
            .iter()
            .map(|(path, read)| AttachedRead {
                path: path.clone(),
                content: read.content.clone(),
                modified: read.modified,
                offset: read.offset,
                limit: read.limit,
                is_full_read: read.is_full_read,
            })
            .collect::<Vec<_>>(),
    )
    .expect("file read checkpoint serializes")
}

pub(super) fn restore_reads(
    value: Value,
) -> Result<Vec<(PathBuf, tickloop_agent_protocol::FileReadRecord)>, String> {
    let reads: Vec<AttachedRead> =
        serde_json::from_value(value).map_err(|error| error.to_string())?;
    Ok(reads
        .into_iter()
        .map(|read| {
            (
                read.path,
                tickloop_agent_protocol::FileReadRecord {
                    content: read.content,
                    modified: read.modified,
                    offset: read.offset,
                    limit: read.limit,
                    is_full_read: read.is_full_read,
                },
            )
        })
        .collect())
}

pub fn serialize<S: Serializer>(snapshot: &ChatSnapshot, serializer: S) -> Result<S::Ok, S::Error> {
    let mut live = serde_json::Map::new();
    live.insert(
        "history".into(),
        serde_json::to_value(snapshot).map_err(serde::ser::Error::custom)?,
    );
    live.insert(
        "tool_call_recoveries".into(),
        serde_json::to_value(&snapshot.tool_call_recoveries).map_err(serde::ser::Error::custom)?,
    );
    live.insert(
        "removed_artifact_ticks".into(),
        serde_json::to_value(&snapshot.removed_artifact_ticks)
            .map_err(serde::ser::Error::custom)?,
    );
    live.insert(
        "request_usage".into(),
        snapshot.request_usage.upgrade_checkpoint(),
    );
    live.insert(
        "model".into(),
        serde_json::to_value(&snapshot.model).map_err(serde::ser::Error::custom)?,
    );
    live.insert(
        "reasoning_effort".into(),
        serde_json::to_value(snapshot.reasoning_effort).map_err(serde::ser::Error::custom)?,
    );
    live.insert(
        "thinking_enabled".into(),
        serde_json::to_value(snapshot.thinking_enabled).map_err(serde::ser::Error::custom)?,
    );
    live.insert(
        "resume_triggers".into(),
        serde_json::to_value(&snapshot.resume_triggers).map_err(serde::ser::Error::custom)?,
    );
    live.insert(
        "last_sequence".into(),
        serde_json::to_value(snapshot.last_sequence).map_err(serde::ser::Error::custom)?,
    );
    live.insert(
        "permissions".into(),
        serde_json::to_value(&snapshot.permissions).map_err(serde::ser::Error::custom)?,
    );
    live.insert(
        "questions".into(),
        serde_json::to_value(&snapshot.questions).map_err(serde::ser::Error::custom)?,
    );
    live.insert(
        "pending_inputs".into(),
        serde_json::to_value(&snapshot.pending_inputs).map_err(serde::ser::Error::custom)?,
    );
    live.insert(
        "pending_commands".into(),
        serde_json::to_value(&snapshot.pending_commands).map_err(serde::ser::Error::custom)?,
    );
    live.insert(
        "last_queue_sequence".into(),
        serde_json::to_value(snapshot.last_queue_sequence).map_err(serde::ser::Error::custom)?,
    );
    live.insert(
        "operations".into(),
        serde_json::to_value(&snapshot.operations).map_err(serde::ser::Error::custom)?,
    );
    live.insert(
        "policy".into(),
        serde_json::to_value(&snapshot.policy).map_err(serde::ser::Error::custom)?,
    );
    live.insert(
        "additional_directories".into(),
        serde_json::to_value(&snapshot.additional_directories)
            .map_err(serde::ser::Error::custom)?,
    );
    live.insert(
        "events".into(),
        serde_json::to_value(&snapshot.events).map_err(serde::ser::Error::custom)?,
    );
    live.insert(
        "pending_thinking".into(),
        serde_json::to_value(&snapshot.pending_thinking).map_err(serde::ser::Error::custom)?,
    );
    live.insert(
        "context_window_tokens".into(),
        serde_json::to_value(snapshot.context_window_tokens).map_err(serde::ser::Error::custom)?,
    );
    live.insert(
        "pending_model".into(),
        serde_json::to_value(&snapshot.pending_model).map_err(serde::ser::Error::custom)?,
    );
    live.insert(
        "commands".into(),
        serde_json::to_value(&snapshot.commands).map_err(serde::ser::Error::custom)?,
    );
    live.insert(
        "user_shells".into(),
        serde_json::to_value(&snapshot.user_shells).map_err(serde::ser::Error::custom)?,
    );
    live.insert(
        "last_model_run".into(),
        serde_json::to_value(snapshot.last_model_run).map_err(serde::ser::Error::custom)?,
    );
    live.insert(
        "input_authorizations".into(),
        serde_json::to_value(
            snapshot
                .pending_inputs
                .iter()
                .map(|input| {
                    (
                        input.receipt.run_id,
                        input.is_skill_invocation,
                        input
                            .attached_file_reads
                            .iter()
                            .map(|(path, read)| AttachedRead {
                                path: path.clone(),
                                content: read.content.clone(),
                                modified: read.modified,
                                offset: read.offset,
                                limit: read.limit,
                                is_full_read: read.is_full_read,
                            })
                            .collect::<Vec<_>>(),
                    )
                })
                .collect::<Vec<_>>(),
        )
        .map_err(serde::ser::Error::custom)?,
    );
    live.insert(
        "compaction_models".into(),
        serde_json::to_value(
            snapshot
                .message_compactions
                .iter()
                .filter_map(|(index, marker)| match marker {
                    tickloop_agent_protocol::CompactionMarker::Boundary(boundary) => {
                        Some((*index, boundary.model.clone()))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>(),
        )
        .map_err(serde::ser::Error::custom)?,
    );
    live.serialize(serializer)
}

pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<ChatSnapshot, D::Error> {
    let mut live = serde_json::Map::<String, Value>::deserialize(deserializer)?;
    let take = |live: &mut serde_json::Map<String, Value>, key: &str| {
        live.remove(key)
            .ok_or_else(|| serde::de::Error::custom(format!("missing live Chat field {key}")))
    };
    let mut snapshot: ChatSnapshot =
        serde_json::from_value(take(&mut live, "history")?).map_err(serde::de::Error::custom)?;
    snapshot.tool_call_recoveries =
        serde_json::from_value(take(&mut live, "tool_call_recoveries")?)
            .map_err(serde::de::Error::custom)?;
    snapshot.removed_artifact_ticks =
        serde_json::from_value(take(&mut live, "removed_artifact_ticks")?)
            .map_err(serde::de::Error::custom)?;
    snapshot.request_usage = tickloop_agent_protocol::RequestUsageLedger::from_upgrade_checkpoint(
        take(&mut live, "request_usage")?,
    )
    .map_err(serde::de::Error::custom)?;
    snapshot.model =
        serde_json::from_value(take(&mut live, "model")?).map_err(serde::de::Error::custom)?;
    snapshot.reasoning_effort = serde_json::from_value(take(&mut live, "reasoning_effort")?)
        .map_err(serde::de::Error::custom)?;
    snapshot.thinking_enabled = serde_json::from_value(take(&mut live, "thinking_enabled")?)
        .map_err(serde::de::Error::custom)?;
    snapshot.resume_triggers = serde_json::from_value(take(&mut live, "resume_triggers")?)
        .map_err(serde::de::Error::custom)?;
    snapshot.last_sequence = serde_json::from_value(take(&mut live, "last_sequence")?)
        .map_err(serde::de::Error::custom)?;
    snapshot.permissions = serde_json::from_value(take(&mut live, "permissions")?)
        .map_err(serde::de::Error::custom)?;
    snapshot.questions =
        serde_json::from_value(take(&mut live, "questions")?).map_err(serde::de::Error::custom)?;
    snapshot.pending_inputs = serde_json::from_value(take(&mut live, "pending_inputs")?)
        .map_err(serde::de::Error::custom)?;
    snapshot.pending_commands = serde_json::from_value(take(&mut live, "pending_commands")?)
        .map_err(serde::de::Error::custom)?;
    snapshot.last_queue_sequence = serde_json::from_value(take(&mut live, "last_queue_sequence")?)
        .map_err(serde::de::Error::custom)?;
    snapshot.operations =
        serde_json::from_value(take(&mut live, "operations")?).map_err(serde::de::Error::custom)?;
    snapshot.policy =
        serde_json::from_value(take(&mut live, "policy")?).map_err(serde::de::Error::custom)?;
    snapshot.additional_directories =
        serde_json::from_value(take(&mut live, "additional_directories")?)
            .map_err(serde::de::Error::custom)?;
    snapshot.events =
        serde_json::from_value(take(&mut live, "events")?).map_err(serde::de::Error::custom)?;
    snapshot.pending_thinking = serde_json::from_value(take(&mut live, "pending_thinking")?)
        .map_err(serde::de::Error::custom)?;
    snapshot.context_window_tokens =
        serde_json::from_value(take(&mut live, "context_window_tokens")?)
            .map_err(serde::de::Error::custom)?;
    snapshot.pending_model = serde_json::from_value(take(&mut live, "pending_model")?)
        .map_err(serde::de::Error::custom)?;
    snapshot.commands =
        serde_json::from_value(take(&mut live, "commands")?).map_err(serde::de::Error::custom)?;
    snapshot.user_shells = serde_json::from_value(take(&mut live, "user_shells")?)
        .map_err(serde::de::Error::custom)?;
    snapshot.last_model_run = serde_json::from_value(take(&mut live, "last_model_run")?)
        .map_err(serde::de::Error::custom)?;
    let inputs: Vec<(RunId, bool, Vec<AttachedRead>)> =
        serde_json::from_value(take(&mut live, "input_authorizations")?)
            .map_err(serde::de::Error::custom)?;
    for (run, skill, reads) in inputs {
        let input = snapshot
            .pending_inputs
            .iter_mut()
            .find(|input| input.receipt.run_id == run)
            .ok_or_else(|| serde::de::Error::custom("input authorization has no pending Run"))?;
        input.is_skill_invocation = skill;
        input.attached_file_reads = reads
            .into_iter()
            .map(|read| {
                (
                    read.path,
                    tickloop_agent_protocol::FileReadRecord {
                        content: read.content,
                        modified: read.modified,
                        offset: read.offset,
                        limit: read.limit,
                        is_full_read: read.is_full_read,
                    },
                )
            })
            .collect();
    }
    let compactions: Vec<(usize, Option<Box<ModelConfig>>)> =
        serde_json::from_value(take(&mut live, "compaction_models")?)
            .map_err(serde::de::Error::custom)?;
    for (index, model) in compactions {
        match snapshot.message_compactions.get_mut(&index) {
            Some(tickloop_agent_protocol::CompactionMarker::Boundary(boundary)) => {
                boundary.model = model
            }
            _ => return Err(serde::de::Error::custom("compaction model has no boundary")),
        }
    }
    Ok(snapshot)
}
