//! Transcript：每个 Chat 一个只追加的 JSONL 文件，照 Claude Code 的会话记录实现。
//!
//! 链上条目（user、assistant、system）以 `uuid`／`parentUuid` 串联；元数据条目不在链上，
//! 同类以最后一条为准。文件不加锁，多个进程继续同一 Chat 时形成分叉，加载时进入叶子
//! 时间最新的一支。本模块只处理文件格式与读写，不依赖运行时类型。

mod entry;
mod listing;
mod path;
mod reader;
mod writer;

pub(crate) use entry::{
    BranchOriginEntry, COMPACT_BOUNDARY_SUBTYPE, ChainEntry, CompactMetadata, CompactTriggerEntry,
    ContentReplacementEntry, ContentReplacementRecord, CustomTitleEntry, FileHistorySnapshotEntry,
    ForkedFrom, LOOP_CREATED_SUBTYPE, LOOP_ENDED_SUBTYPE, LOOP_STATE_SUBTYPE, LOOP_TRIGGER_SUBTYPE,
    LastPromptEntry, Line, LoopRecord, PartialCompactDirection,
    SummarizeMetadata as SummarizeMetadataEntry, format_timestamp, now_timestamp,
};
pub(crate) use listing::{LITE_READ_BUF_SIZE, list_project};
pub(crate) use path::{
    PROJECTS_DIRECTORY, PrivateDirectory, TRANSCRIPT_FILE_SUFFIX, locate,
    private_project_temp_directory, project_directory, transcript_path,
};
pub(crate) use reader::{LoadedTranscript, load, load_original_user_inputs};
pub(crate) use writer::{CachedMetadata, FLUSH_INTERVAL, TranscriptWriter};
