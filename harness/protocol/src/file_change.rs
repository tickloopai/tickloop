use std::{fs::Metadata, os::unix::fs::MetadataExt, path::PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeAction {
    Create,
    Delete,
    Overwrite,
    Edit,
    ReplaceAll,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FileIdentity {
    pub device: u64,
    pub inode: u64,
    pub byte_len: u64,
    pub modified: (i64, i64),
    pub changed: (i64, i64),
}

impl FileIdentity {
    pub fn from_metadata(metadata: &Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            byte_len: metadata.len(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FileVersion {
    pub exists: bool,
    pub identity: Option<FileIdentity>,
    /// The environment's opaque version of the file, kept when it gives no identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub byte_len: u64,
    pub line_count: u64,
    pub ends_with_newline: bool,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FileChangeReviewSummary {
    pub action: FileChangeAction,
    pub path: PathBuf,
    pub before: FileVersion,
    pub after: FileVersion,
    pub added_lines: u64,
    pub removed_lines: u64,
    pub hunk_count: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FileChangeArtifactRef {
    pub path: PathBuf,
    pub byte_len: u64,
    pub sha256: String,
}

/// A bounded projection of recorded review hunks, not reconstructed whole-file contents.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RecordedFileChangeReview {
    pub path: PathBuf,
    pub unified_diff: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PreparedFileChange {
    pub summary: FileChangeReviewSummary,
    pub artifact: FileChangeArtifactRef,
}
