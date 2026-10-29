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
            execution: self.execution.clone(),
            port: bound.port,
            generation: bound.generation,
            prepared,
        })))
    }

    async fn delete(&self, path: FilePath, expected: FileVersion) -> Result<(), FileWriteError> {
        let bound = self.execution.port().map_err(FileWriteError::NotWritten)?;
        let prepared = bound
            .port
            .prepare(ExecutionIntent::DeleteFile { path, expected })
            .await
            .map_err(FileWriteError::NotWritten)?;
        carry_out(
            &self.execution,
            bound.port.as_ref(),
            bound.generation,
            prepared,
            CancellationToken::new(),
        )
        .await
        .map(|_| ())
    }
}

/// A `WriteFile` the Port prepared, carried out on the same Port once the Chat admits it.
struct PortFileWrite {
    execution: ChatExecution,
    port: Arc<dyn ExecutionPort>,
    generation: u64,
    prepared: PreparedExecution,
}

#[async_trait]
impl PreparedFileWrite for PortFileWrite {
    async fn write(
        self: Box<Self>,
        cancel: CancellationToken,
    ) -> Result<FileMetadata, FileWriteError> {
        let result = carry_out(
            &self.execution,
            self.port.as_ref(),
            self.generation,
            self.prepared,
            cancel,
        )
        .await?;
        result.file.map(|file| *file).ok_or_else(|| {
            FileWriteError::OutcomeUnknown("the environment reported no written file".into())
        })
    }
}

/// Carries out a file change prepared on the Port of binding `generation`, unless the Chat's
/// directories changed since: the roots it was prepared for may no longer be the Chat's.
async fn carry_out(
    execution: &ChatExecution,
    port: &dyn ExecutionPort,
    generation: u64,
    prepared: PreparedExecution,
    cancel: CancellationToken,
) -> Result<ExecutionResult, FileWriteError> {
    if !execution.is_current(generation) {
        return Err(FileWriteError::NotWritten(ExecutionError::new(
            error_code::ACCESS_DENIED,
            "The Chat's directories changed after this file change was prepared, so it was not carried out.",
        )));
    }
    let admission = execution
        .authority()
        .admit(&prepared)
        .map_err(FileWriteError::NotWritten)?;
    match port.execute(prepared, admission, cancel).await {
        ExecutionOutcome::Completed(result) => Ok(result),
        ExecutionOutcome::NotDispatched(error) => Err(FileWriteError::NotWritten(error)),
        ExecutionOutcome::OutcomeUnknown { reason, .. } => {
            Err(FileWriteError::OutcomeUnknown(reason))
        }
    }
}

#[cfg(test)]
mod tests {
    use tickloop_harness_test_support::config;
    use uuid::Uuid;

    use super::*;
    use crate::environment::memory::MemoryEnvironment;

    #[tokio::test]
    async fn a_write_prepared_before_the_directories_change_is_not_carried_out() {
        let memory = MemoryEnvironment::new();
        memory.define_root("folder", "/folder", FileAccess::ReadWrite);
        memory.define_root("notes", "/notes", FileAccess::ReadWrite);
        let environment: Arc<dyn ExecutionEnvironment> = Arc::new(memory.clone());
        let mut snapshot =
            crate::empty_snapshot(Uuid::new_v4().into(), Some("/folder".into()), config());
        snapshot.additional_directories = vec!["/notes".into()];
        let execution = ChatExecution::bind(Some(&environment), &snapshot).await;
        let files = EnvironmentFileHost::new(execution.clone());
        let plan = files.capabilities().resolve("/notes/plan.md").unwrap();
        let stale = files
            .prepare_write(plan, b"stale".to_vec(), None)
            .await
            .unwrap();

        snapshot.additional_directories.clear();
        execution.follow(Some(&environment), &snapshot).await;
        let error = stale.write(CancellationToken::new()).await.unwrap_err();
        assert!(
            matches!(&error, FileWriteError::NotWritten(error) if error.code == error_code::ACCESS_DENIED),
            "{error:?}"
        );
        assert_eq!(memory.file(Some("notes"), "plan.md"), None);

        let report = files.capabilities().resolve("/folder/report.md").unwrap();
        files
            .prepare_write(report, b"current".to_vec(), None)
            .await
            .unwrap()
            .write(CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            memory.file(Some("folder"), "report.md").as_deref(),
            Some(&b"current"[..])
        );
    }
}
