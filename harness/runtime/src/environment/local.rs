//! The file operations of the Port for directories on this machine, on the local roots of the
//! tools; [`FolderEnvironment`](super::FolderEnvironment) and Code's Local Environment share them.

use serde_json::{Value, json};
use tickloop_harness_protocol::ToolResult;
use tickloop_harness_tools::{
    local_files::LocalFileError,
    local_roots::{LocalRoots, delete_resolved, execution_error, write_resolved},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    ExecutionAuthority, ExecutionError, ExecutionFacts, ExecutionIntent, ExecutionOutcome,
    ExecutionResult, ExecutionScope, FileCapabilities, FileInspection, FileMetadata, FileQuery,
    PreparedExecution, error_code,
};

/// The Folder and Additional Directories of a scope as directories on this machine
/// ([`LocalRoots::new`]): every root, like every absolute path outside them, can be read and
/// written, and the Harness decides what needs approval.
#[derive(Clone, Debug, Default)]
pub struct LocalFiles {
    roots: LocalRoots,
}

impl LocalFiles {
    /// No roots and nothing outside them for a scope without a Folder, whose Chat touches no file.
    pub fn for_scope(scope: &ExecutionScope) -> Self {
        Self {
            roots: LocalRoots::new(scope.folder.as_deref(), &scope.additional_directories),
        }
    }

    pub fn capabilities(&self) -> &FileCapabilities {
        self.roots.capabilities()
    }

    pub async fn inspect(&self, query: FileQuery) -> Result<FileInspection, ExecutionError> {
        let roots = self.roots.clone();
        // Dropping this future, as a cancelled read does, stops a search still walking.
        let cancel = CancellationToken::new();
        let _stop_on_drop = cancel.clone().drop_guard();
        tokio::task::spawn_blocking(move || roots.inspect_blocking(query, &cancel))
            .await
            .map_err(|error| ExecutionError::new(error_code::IO, error.to_string()))?
    }

    /// Resolves the file a `WriteFile` or `DeleteFile` changes and keeps its real path in the
    /// approval facts, which [`LocalFiles::execute`] then changes without following links.
    pub async fn prepare(
        &self,
        intent: ExecutionIntent,
        port: Uuid,
        authority: &ExecutionAuthority,
    ) -> Result<PreparedExecution, ExecutionError> {
        let path = intent
            .file_path()
            .ok_or_else(|| ExecutionError::new(error_code::INVALID_INPUT, "not a file change"))?
            .clone();
        let roots = self.roots.clone();
        let shown = self.capabilities().display(&path);
        let target = tokio::task::spawn_blocking(move || roots.resolve_change(&path))
            .await
            .map_err(|error| ExecutionError::new(error_code::IO, error.to_string()))??;
        let mut facts =
            ExecutionFacts::for_file(&intent, &shown).expect("a file change has file facts");
        facts.approval.canonical_path = Some(target);
        Ok(PreparedExecution::new(port, authority, facts))
    }

    /// Carries out a change [`LocalFiles::prepare`] prepared and the Harness admitted. Whatever
    /// fails before the file is published changes nothing.
    pub async fn execute(
        &self,
        prepared: &PreparedExecution,
        cancel: CancellationToken,
    ) -> ExecutionOutcome {
        let facts = prepared.facts();
        let (Some(path), Some(target)) = (
            facts.intent.file_path(),
            facts.approval.canonical_path.clone(),
        ) else {
            return ExecutionOutcome::NotDispatched(ExecutionError::new(
                error_code::INVALID_INPUT,
                "this file change was not prepared on this machine",
            ));
        };
        if cancel.is_cancelled() {
            return ExecutionOutcome::NotDispatched(ExecutionError::new(
                error_code::CANCELLED,
                "execution cancelled before dispatch",
            ));
        }
        let shown = self.capabilities().display(path);
        let operation = prepared.operation();
        let change = match facts.intent.clone() {
            ExecutionIntent::WriteFile {
                content, expected, ..
            } => tokio::task::spawn_blocking(move || -> Result<_, LocalFileError> {
                let published =
                    write_resolved(&target, &content, expected.as_ref(), operation, &cancel)?;
                Ok((
                    json!({"status":"written", "path":shown, "bytes":content.len()}),
                    Some(published),
                ))
            }),
            ExecutionIntent::DeleteFile { expected, .. } => {
                tokio::task::spawn_blocking(move || -> Result<_, LocalFileError> {
                    delete_resolved(&target, &expected)?;
                    Ok((json!({"status":"deleted", "path":shown}), None))
                })
            }
            _ => {
                return ExecutionOutcome::NotDispatched(ExecutionError::new(
                    error_code::INVALID_INPUT,
                    "not a file change",
                ));
            }
        };
        let finished: Result<(Value, Option<FileMetadata>), LocalFileError> = match change.await {
            Ok(finished) => finished,
            Err(error) => {
                return ExecutionOutcome::OutcomeUnknown {
                    operation,
                    reason: error.to_string(),
                };
            }
        };
        match finished {
            Ok((content, file)) => ExecutionOutcome::Completed(ExecutionResult {
                result: ToolResult {
                    content: content.to_string(),
                    is_error: false,
                    output_ref: None,
                },
                opened: None,
                shell: None,
                command: None,
                handoff: None,
                file: file.map(Box::new),
            }),
            Err(error @ LocalFileError::Unsettled { .. }) => ExecutionOutcome::OutcomeUnknown {
                operation,
                reason: error.to_string(),
            },
            Err(error) => ExecutionOutcome::NotDispatched(execution_error(error, true)),
        }
    }
}
