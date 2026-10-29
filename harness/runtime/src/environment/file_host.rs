//! The file tools of a Chat on the Port the Chat bound for its directories.

use std::sync::Arc;

use async_trait::async_trait;
use tickloop_harness_tools::file_port::{
    FileHost, FileWriteError, FileWritePreparation, PreparedFileWrite,
};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::chat_execution::ChatExecution;

pub(crate) struct EnvironmentFileHost {
    execution: ChatExecution,
}

impl EnvironmentFileHost {
    pub(crate) fn new(execution: ChatExecution) -> Self {
        Self { execution }
    }
}

#[async_trait]
impl FileHost for EnvironmentFileHost {
    fn capabilities(&self) -> FileCapabilities {
        self.execution.files()
    }

    async fn inspect(&self, query: FileQuery) -> Result<FileInspection, ExecutionError> {
        let bound = self.execution.port()?;
        match bound.port.inspect(ExecutionQuery::File(query)).await? {
            ExecutionInspection::File(inspection) => Ok(inspection),
            _ => Err(ExecutionError::new(
                "ProtocolViolation",
                "execution environment answered a file query with something else",
            )),
        }
    }

    async fn prepare_write(
        &self,
        path: FilePath,
        content: Vec<u8>,
        expected: Option<FileVersion>,
    ) -> Result<FileWritePreparation, ExecutionError> {
        let bound = self.execution.port()?;
        let prepared = bound
            .port
            .prepare(ExecutionIntent::WriteFile {
                path,
                content,
                expected,
            })
            .await?;
        Ok(FileWritePreparation::new(Box::new(PortFileWrite {
            port: bound.port,
            authority: self.execution.authority().clone(),
            prepared,
        })))
    }
}

/// A `WriteFile` the Port prepared, carried out on the same Port once the Chat admits it.
struct PortFileWrite {
    port: Arc<dyn ExecutionPort>,
    authority: ExecutionAuthority,
    prepared: PreparedExecution,
}

#[async_trait]
impl PreparedFileWrite for PortFileWrite {
    async fn write(
        self: Box<Self>,
        cancel: CancellationToken,
    ) -> Result<FileMetadata, FileWriteError> {
        let admission = self
            .authority
            .admit(&self.prepared)
            .map_err(FileWriteError::NotWritten)?;
        match self.port.execute(self.prepared, admission, cancel).await {
            ExecutionOutcome::Completed(result) => result.file.map(|file| *file).ok_or_else(|| {
                FileWriteError::OutcomeUnknown("the environment reported no written file".into())
            }),
            ExecutionOutcome::NotDispatched(error) => Err(FileWriteError::NotWritten(error)),
            ExecutionOutcome::OutcomeUnknown { reason, .. } => {
                Err(FileWriteError::OutcomeUnknown(reason))
            }
        }
    }
}
