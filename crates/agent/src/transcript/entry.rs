use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::model::{Message, ModelIdentity, PrivateHistorySource, Role, Usage};
use crate::private_fs::unix_time_milliseconds;

/// Transcript 中的一行。未知类型原样跳过，保证旧版本能读取新版本写入的文件。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub(crate) enum Line {
    User(ChainEntry),
    Assistant(ChainEntry),
    System(ChainEntry),
    ArtifactTicks(ArtifactTicksEntry),
    CustomTitle(CustomTitleEntry),
    LastPrompt(LastPromptEntry),
    BranchOrigin(BranchOriginEntry),
    Delegation(DelegationEntry),
    /// 照 Claude Code：文件回滚快照，按消息 id 关联到链上消息。
    FileHistorySnapshot(FileHistorySnapshotEntry),
    /// 照 Claude Code：大工具结果的替换决定，恢复时重建，同一 Chat 的条目逐条累积。
    ContentReplacement(ContentReplacementEntry),
    #[serde(other)]
    Unknown,
}

impl Line {
    pub(crate) fn chain(role: Role, entry: ChainEntry) -> Self {
        match role {
            Role::User => Self::User(entry),
            Role::Assistant => Self::Assistant(entry),
            Role::System => Self::System(entry),
        }
    }

    pub(crate) fn chain_entry(&self) -> Option<&ChainEntry> {
        match self {
            Self::User(entry) | Self::Assistant(entry) | Self::System(entry) => Some(entry),
            _ => None,
        }
    }

    pub(crate) fn is_conversation_message(&self) -> bool {
        matches!(self, Self::User(_) | Self::Assistant(_))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ChainEntry {
    pub uuid: Uuid,
    pub parent_uuid: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logical_parent_uuid: Option<Uuid>,
    #[serde(default)]
    pub is_sidechain: bool,
    pub chat_id: Uuid,
    pub timestamp: String,
    pub cwd: PathBuf,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    pub message: Message,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub invoked_skills: Vec<crate::skills::InvokedSkill>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_meta: bool,
    /// The execution mode at a user checkpoint stays raw so an unknown legacy value cannot
    /// invalidate the entire Transcript entry.
    #[serde(
        default,
        alias = "permissionMode",
        skip_serializing_if = "Option::is_none"
    )]
    pub execution_mode: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_inherited_prompt: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_user_message_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_message_origin: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_call_recoveries: Vec<crate::runtime::ToolCallRecovery>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contexts: Option<serde_json::Value>,
    /// Tick Loop 扩展：同一条内存消息拆成多个条目时（向结果消息追加的并行工具结果），后续条目
    /// 标记为接续父条目所在的消息，加载时合并回同一条消息。Claude Code 的每个工具结果本来就是
    /// 独立的 user 消息，Tick Loop 在内存中把并行结果合在一条消息里，所以保留这个字段。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub continues_message: bool,
    /// 照 Claude Code system 条目的 `subtype`；压缩边界为 [`COMPACT_BOUNDARY_SUBTYPE`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtype: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_metadata: Option<CompactMetadata>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_compact_summary: bool,
    /// 照 Claude Code：定向摘要保留了消息时，摘要消息记录被摘要的消息数、用户补充说明和方向。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summarize_metadata: Option<SummarizeMetadata>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_visible_in_transcript_only: bool,
    /// assistant 条目的用量（Claude Code 记在 `message.usage`，Tick Loop 的消息结构没有该字段）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// assistant 条目生成该消息的模型（Claude Code 记在 `message.model`，Tick Loop 的消息结构
    /// 没有该字段）。恢复时据此判断历史中的私有推理是否来自当前模型。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_source: Option<PrivateHistorySource>,
    /// 条目所属 run 的 Loop（有 `runId` 时记录）。恢复时据此把计划与进度工具调用归到各自的 Loop。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loop_id: Option<Uuid>,
    /// 照 Claude Code 消息的 `slug`：Chat 的计划文件名前缀，生成之后写在每个条目上；
    /// 恢复时按链上第一个 slug 关联计划文件。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    /// Loop 条目（system，subtype 为 `loop_*`）记录的 Loop 完整状态；run id 记在 `runId`。
    /// 这类条目挂在对话链上，但不是对话消息，不进入发给模型的消息。
    #[serde(rename = "loop", default, skip_serializing_if = "Option::is_none")]
    pub loop_record: Option<LoopRecord>,
}

impl ChainEntry {
    pub(crate) fn is_compact_boundary(&self) -> bool {
        self.message.role == Role::System
            && self.subtype.as_deref() == Some(COMPACT_BOUNDARY_SUBTYPE)
    }

    pub(crate) fn is_loop_record(&self) -> bool {
        self.message.role == Role::System && self.loop_record.is_some()
    }
}

pub(crate) const COMPACT_BOUNDARY_SUBTYPE: &str = "compact_boundary";
/// Loop 条目的 subtype。恢复时每条都按 Loop id 以完整状态覆盖，subtype 只说明写入原因。
pub(crate) const LOOP_CREATED_SUBTYPE: &str = "loop_created";
pub(crate) const LOOP_STATE_SUBTYPE: &str = "loop_state";
pub(crate) const LOOP_TRIGGER_SUBTYPE: &str = "loop_trigger";
pub(crate) const LOOP_ENDED_SUBTYPE: &str = "loop_ended";

/// Loop 在对话链上的完整状态。状态与 Trigger 按原始 JSON 保存，本模块不解析其结构。
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LoopRecord {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failure_events: Vec<serde_json::Value>,
    pub loop_id: Uuid,
    pub goal: String,
    pub state: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<serde_json::Value>,
    /// Trigger 已解除武装、等待用户向该 Loop 提交输入时重新武装（重启恢复或 Branch 复制后）。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub trigger_awaits_resume: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub event_sources: Vec<serde_json::Value>,
    /// 写入时该 Loop 的计划版本。只在对话链上缺少之前的计划工具调用时写出（Loop 首次写入
    /// 当前链段，例如压缩、rewind 截断、Branch 与 `--fork-session` 之后），恢复时与链上的调用取较大值。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_version: Option<u64>,
    /// 与 `plan_version` 同时写出的进度步骤（原始 JSON）；链上之后的进度工具调用覆盖它。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<serde_json::Value>,
    /// Unix milliseconds when the runtime moved the Loop into `state`, kept as raw JSON so an
    /// unreadable value leaves the time unknown instead of dropping the record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_since: Option<serde_json::Value>,
}

impl LoopRecord {
    /// 不含计划与进度的 Loop 状态，用于判断 Loop 本身是否变化。
    pub(crate) fn without_plan(&self) -> Self {
        Self {
            plan_version: None,
            progress: None,
            ..self.clone()
        }
    }
}

/// 照 Claude Code `compactMetadata`。
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CompactMetadata {
    pub trigger: CompactTriggerEntry,
    pub pre_tokens: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_context: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub messages_summarized: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preserved_segment: Option<PreservedSegmentEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_user_inputs: Option<Vec<crate::runtime::approval_context::OriginalUserInput>>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum CompactTriggerEntry {
    Manual,
    Auto,
}

/// 照 Claude Code `preservedSegment`：保留的消息不重写，加载时据此重接父指针。
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PreservedSegmentEntry {
    pub head_uuid: Uuid,
    pub anchor_uuid: Uuid,
    pub tail_uuid: Uuid,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SummarizeMetadata {
    pub messages_summarized: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_context: Option<String>,
    pub direction: PartialCompactDirection,
}

/// 照 Claude Code `PartialCompactDirection`。
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PartialCompactDirection {
    From,
    UpTo,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CustomTitleEntry {
    pub chat_id: Uuid,
    pub custom_title: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LastPromptEntry {
    pub chat_id: Uuid,
    pub last_prompt: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BranchOriginEntry {
    pub chat_id: Uuid,
    pub forked_from: ForkedFrom,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DelegationEntry {
    pub chat_id: Uuid,
    pub delegation: crate::runtime::DelegationMetadata,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ForkedFrom {
    pub chat_id: Uuid,
    #[serde(default)]
    pub message_uuid: Option<Uuid>,
}

/// 照 Claude Code `file-history-snapshot`：`message_id` 是关联的链上消息，更新条目
/// （`is_snapshot_update`）以发起编辑的消息为键，替换 `snapshot.messageId` 对应的快照。
/// 快照本体按原始 JSON 保存，本模块不解析其结构。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FileHistorySnapshotEntry {
    pub message_id: Uuid,
    pub snapshot: serde_json::Value,
    pub is_snapshot_update: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ContentReplacementEntry {
    pub chat_id: Uuid,
    pub replacements: Vec<ContentReplacementRecord>,
}

/// Claude Code `ContentReplacementRecord`：`replacement` 是模型看到的原文，恢复时直接套用，
/// 不按当前代码重新生成。
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub(crate) enum ContentReplacementRecord {
    #[serde(rename_all = "camelCase")]
    ToolResult {
        tool_use_id: String,
        replacement: String,
    },
    #[serde(other)]
    Unknown,
}

impl ContentReplacementRecord {
    pub(crate) fn tool_use_id(&self) -> &str {
        match self {
            Self::ToolResult { tool_use_id, .. } => tool_use_id,
            Self::Unknown => "",
        }
    }
}

const MILLISECONDS_PER_DAY: i64 = 86_400_000;

/// 当前 UTC 时间，RFC 3339 毫秒精度，与 Claude Code 的 `new Date().toISOString()` 一致。
pub(crate) fn now_timestamp() -> String {
    format_timestamp(unix_time_milliseconds())
}

pub(crate) fn format_timestamp(milliseconds: u64) -> String {
    let milliseconds = i64::try_from(milliseconds).unwrap_or(i64::MAX);
    let days = milliseconds.div_euclid(MILLISECONDS_PER_DAY);
    let within_day = milliseconds.rem_euclid(MILLISECONDS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        within_day / 3_600_000,
        within_day / 60_000 % 60,
        within_day / 1000 % 60,
        within_day % 1000
    )
}

/// 解析 `YYYY-MM-DDTHH:MM:SS[.fff]Z`；格式不符时返回 `None`。
pub(crate) fn parse_timestamp_ms(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() < 20 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let number = |range: std::ops::Range<usize>| -> Option<i64> {
        let slice = text.get(range)?;
        slice
            .bytes()
            .all(|byte| byte.is_ascii_digit())
            .then(|| slice.parse().ok())?
    };
    let year = number(0..4)?;
    let month = number(5..7)?;
    let day = number(8..10)?;
    let hour = number(11..13)?;
    let minute = number(14..16)?;
    let second = number(17..19)?;
    let rest = text.get(19..)?;
    let fraction = match rest.strip_prefix('.') {
        Some(fraction) => {
            let digits = fraction.strip_suffix('Z')?;
            if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            let padded = format!("{digits:0<3}");
            padded[..3].parse::<i64>().ok()?
        }
        None if rest == "Z" => 0,
        None => return None,
    };
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some(days * MILLISECONDS_PER_DAY + ((hour * 60 + minute) * 60 + second) * 1000 + fraction)
}

// Howard Hinnant 的 civil-from-days / days-from-civil 算法（公历，1970-01-01 为第 0 天）。
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let month_index = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_round_trip_with_millisecond_precision() {
        for milliseconds in [0, 951_782_400_123, 1_789_000_000_999, 4_102_444_799_000] {
            let text = format_timestamp(milliseconds);
            assert_eq!(
                parse_timestamp_ms(&text),
                Some(milliseconds as i64),
                "{text}"
            );
        }
        assert_eq!(
            format_timestamp(951_782_400_123),
            "2000-02-29T00:00:00.123Z"
        );
        assert_eq!(
            parse_timestamp_ms("2000-02-29T00:00:00Z"),
            Some(951_782_400_000)
        );
        assert_eq!(parse_timestamp_ms("not a time"), None);
    }

    #[test]
    fn unknown_line_types_are_accepted_and_ignored() {
        let line: Line = serde_json::from_str(r#"{"type":"future-kind","value":1}"#).unwrap();
        assert!(matches!(line, Line::Unknown));
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ArtifactTicksEntry {
    pub chat_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<std::path::PathBuf>,
    pub artifact_ticks: Vec<crate::artifacts::ArtifactTick>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub removed_artifact_ticks: std::collections::BTreeSet<String>,
}
