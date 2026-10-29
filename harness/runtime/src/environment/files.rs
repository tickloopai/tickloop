//! Files on the Execution Environment Port: each directory's root name and file capability, paths
//! below a root, and the file queries, intents and results. [`ExecutionPort`](super::ExecutionPort)
//! states their semantics. The types the file tools also use are defined with them.

use serde_json::json;
use tickloop_harness_protocol::PreparedApprovalFacts;
pub use tickloop_harness_tools::file_port::{
    DirectoryEntry, DirectoryListing, FileAccess, FileCapabilities, FileContent, FileInspection,
    FileKind, FileMetadata, FilePath, FileQuery, FileRoot, FileStat, FileVersion,
    ROOT_NAME_MAX_BYTES, ResolvedPath, RootName, error_code,
};
pub use tickloop_harness_tools::file_search::{
    BinaryMatch, FileMatchCount, FileSearchError, FoundFile, GLOB_RESULT_LIMIT, GlobMatches,
    GlobQuery, GrepLine, GrepLineText, GrepMatches, GrepOutput, GrepOutputMode, GrepQuery,
};

use super::{ExecutionFacts, ExecutionIntent};
use crate::host_tools::HostToolPermission;

impl ExecutionIntent {
    /// The file a `WriteFile` or `DeleteFile` changes.
    pub fn file_path(&self) -> Option<&FilePath> {
        match self {
            Self::WriteFile { path, .. } | Self::DeleteFile { path, .. } => Some(path),
            _ => None,
        }
    }
}

impl ExecutionFacts {
    /// The facts of a `WriteFile` or `DeleteFile` shown as `shown_path`; they give the size of the
    /// content, never the content. `None` for any other intent.
    pub fn for_file(intent: &ExecutionIntent, shown_path: &str) -> Option<Self> {
        let (normalized_input, summary) = match intent {
            ExecutionIntent::WriteFile {
                path,
                content,
                expected,
            } => (
                json!({
                    "action": "write_file",
                    "root": path.root().map(RootName::as_str),
                    "path": path.path(),
                    "bytes": content.len(),
                    "expected": expected,
                }),
                format!("Write {shown_path}"),
            ),
            ExecutionIntent::DeleteFile { path, expected } => (
                json!({
                    "action": "delete_file",
                    "root": path.root().map(RootName::as_str),
                    "path": path.path(),
                    "expected": expected,
                }),
                format!("Delete {shown_path}"),
            ),
            _ => return None,
        };
        Some(Self {
            intent: intent.clone(),
            summary,
            permission: HostToolPermission::Reviewable,
            approval: PreparedApprovalFacts {
                normalized_input: normalized_input.clone(),
                ..Default::default()
            },
            normalized_input,
        })
    }
}
