use super::*;
use crate::host_tools::HostCallContext;

#[derive(Default)]
pub(super) struct HostContexts {
    pub(super) work: HashMap<ChatId, HostCallContext>,
    pub(super) runs: HashMap<RunId, HostCallContext>,
}

impl Agent {
    pub async fn set_host_work_context(
        &self,
        chat_id: ChatId,
        mut context: HostCallContext,
    ) -> Result<(), AgentError> {
        let chats = self.inner.chats.lock().await;
        let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
        if let Some(delegation) = &chat.snapshot.delegation {
            let inherited = delegation.ceiling.host_context.as_ref();
            if inherited.is_none_or(|inherited| inherited.workspace_id != context.workspace_id) {
                return Err(AgentError::Invalid(
                    "delegated host Work Context cannot expand the inherited Workspace scope"
                        .into(),
                ));
            }
        }
        context.workdir = chat.snapshot.workdir.clone();
        context.interaction = None;
        self.inner
            .host_contexts
            .lock()
            .unwrap()
            .work
            .insert(chat_id, context);
        Ok(())
    }

    pub(super) fn host_call_context(
        &self,
        receipt: &Receipt,
        workdir: &std::path::Path,
    ) -> Option<HostCallContext> {
        let contexts = self.inner.host_contexts.lock().unwrap();
        let mut context = contexts
            .runs
            .get(&receipt.run_id)
            .cloned()
            .or_else(|| contexts.work.get(&receipt.chat_id).cloned())?;
        context.workdir = workdir.into();
        Some(context)
    }

    pub(super) fn remember_host_context(
        &self,
        receipt: &Receipt,
        submitted: Option<HostCallContext>,
    ) {
        let mut contexts = self.inner.host_contexts.lock().unwrap();
        let context = submitted.or_else(|| contexts.work.get(&receipt.chat_id).cloned());
        if let Some(context) = context {
            contexts.runs.insert(receipt.run_id, context);
        }
    }

    pub(super) fn merge_host_context(&self, active: &Receipt, consumed: &Receipt) {
        let mut contexts = self.inner.host_contexts.lock().unwrap();
        let context = contexts
            .runs
            .get(&consumed.run_id)
            .cloned()
            .or_else(|| contexts.work.get(&active.chat_id).cloned());
        if let Some(context) = context {
            contexts.runs.insert(active.run_id, context);
        } else {
            contexts.runs.remove(&active.run_id);
        }
    }
}
