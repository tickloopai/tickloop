use super::{ExecutionAuthority, ExecutionError, ExecutionOutcome, ExecutionResult};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use tickloop_harness::protocol::ChatId;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

type DispatchValidation = Box<dyn Fn() -> Result<(), ExecutionError> + Send + Sync>;

#[derive(Clone)]
pub(crate) struct ExecutionDispatch {
    authority: ExecutionAuthority,
    chat: ChatId,
    cancel: CancellationToken,
    sent: Arc<AtomicBool>,
    opened: Arc<Mutex<Option<ExecutionResult>>>,
    validation: Arc<Mutex<Option<DispatchValidation>>>,
}

impl ExecutionDispatch {
    pub(crate) fn new(
        authority: ExecutionAuthority,
        chat: ChatId,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            authority,
            chat,
            cancel,
            sent: Arc::new(AtomicBool::new(false)),
            opened: Arc::new(Mutex::new(None)),
            validation: Arc::default(),
        }
    }
    pub(crate) fn check(&self) -> Result<(), ExecutionError> {
        self.authority.check(self.chat)?;
        if self.cancel.is_cancelled() {
            return Err(ExecutionError::new(
                "Cancelled",
                "operation cancelled before dispatch",
            ));
        }
        Ok(())
    }
    pub(crate) fn begin(&self) -> Result<(), ExecutionError> {
        self.check()?;
        if let Some(validate) = self.validation.lock().unwrap().as_ref() {
            validate()?;
        }
        self.check()?;
        self.sent.store(true, Ordering::SeqCst);
        Ok(())
    }
    pub(crate) fn revalidate_before_send(
        &self,
        validation: impl Fn() -> Result<(), ExecutionError> + Send + Sync + 'static,
    ) {
        *self.validation.lock().unwrap() = Some(Box::new(validation));
    }
    pub(crate) fn was_sent(&self) -> bool {
        self.sent.load(Ordering::SeqCst)
    }
    pub(crate) fn not_dispatched(&self) {
        self.sent.store(false, Ordering::SeqCst);
    }
    pub(crate) fn confirm_opened(&self, result: ExecutionResult) {
        *self.opened.lock().unwrap() = Some(result);
    }
    pub(crate) async fn interrupted(&self) {
        tokio::select! { _ = self.authority.revoked() => {}, _ = self.cancel.cancelled() => {} }
    }
    pub(crate) fn interruption(&self, operation: Uuid) -> ExecutionOutcome {
        let error = self
            .authority
            .check(self.chat)
            .err()
            .unwrap_or_else(|| ExecutionError::new("Cancelled", "operation cancelled"));
        if let Some(opened) = self.opened.lock().unwrap().clone() {
            return ExecutionOutcome::Completed(opened);
        }
        if self.sent.load(Ordering::SeqCst) {
            ExecutionOutcome::OutcomeUnknown {
                operation,
                reason: format!(
                    "{error} after dispatch; do not repeat operation {}",
                    tickloop_ids::base62::encode(operation)
                ),
            }
        } else {
            ExecutionOutcome::NotDispatched(error)
        }
    }
}
