use std::panic::AssertUnwindSafe;

use futures_util::FutureExt;
use tickloop_agent_model::ModelRequest;
use tickloop_agent_protocol::{EventKind, LoopState, Receipt, RequestFailure, RunId};
use tokio_util::sync::CancellationToken;

use super::{Agent, Chat, LoopSnapshotExt, RunStart, recovery};
use tickloop_agent_transcript::ResponseEnding;

const INTERNAL_RUN_ERROR: &str = "Agent internal error: execution panicked. The run stopped without automatically retrying. Review any operations with unknown outcomes before continuing.";
const INTERNAL_COMPLETION_ERROR: &str = "Agent internal error while finishing the run. The run stopped without automatically retrying. Review any operations with unknown outcomes before continuing.";

pub(super) fn clear_run_state(chat: &mut Chat, run: RunId) {
    if chat
        .upgrade_wait
        .as_ref()
        .is_some_and(|receipt| receipt.run_id == run)
    {
        chat.upgrade_wait = None;
    }
    chat.snapshot.pending_thinking.remove(&run);
    if chat.active_run == Some(run) {
        chat.active = None;
        chat.active_run = None;
    }
    chat.parked.remove(&run);
    chat.releases.remove(&run);
    chat.ready_runs.retain(|id| *id != run);
}

impl Agent {
    pub(super) fn run_task(&self, first: (Receipt, ModelRequest, CancellationToken)) {
        let agent = self.clone();
        self.inner.tasks.spawn(Box::pin(async move {
            let mut next = Some(first);
            while let Some((receipt, request, cancel)) = next {
                next = agent.run(receipt, request, cancel).await;
            }
        }));
    }

    pub(super) async fn run(
        &self,
        receipt: Receipt,
        start: impl Into<RunStart>,
        cancel: CancellationToken,
    ) -> Option<(Receipt, ModelRequest, CancellationToken)> {
        let mut is_awaiting_model = true;
        // Drop execution and its guards before reconciling shared state; panic payloads may contain secrets.
        let execution = AssertUnwindSafe(self.execute_run(
            receipt.clone(),
            start.into(),
            cancel.clone(),
            &mut is_awaiting_model,
        ))
        .catch_unwind()
        .await;
        if self.leave_if_released(&receipt).await {
            return None;
        }
        let internal_failure = execution.is_err();
        let result = execution.unwrap_or_else(|_| Err(INTERNAL_RUN_ERROR.into()));
        if internal_failure {
            cancel.cancel();
        }
        let completion = AssertUnwindSafe(self.finish_run(
            receipt.clone(),
            cancel.clone(),
            result,
            is_awaiting_model,
            internal_failure,
        ))
        .catch_unwind()
        .await;
        match completion {
            Ok(following) => following,
            Err(_) => {
                self.fail_run_completion(&receipt, &cancel).await;
                None
            }
        }
    }

    pub(super) async fn clear_run_interactions(&self, chat: &mut Chat, run: RunId) {
        let permissions = chat
            .snapshot
            .permissions
            .iter()
            .filter(|request| request.execution.run_id() == Some(run))
            .map(|request| request.id)
            .collect::<Vec<_>>();
        let questions = chat
            .snapshot
            .questions
            .iter()
            .filter(|request| request.run_id == run)
            .map(|request| request.id)
            .collect::<Vec<_>>();
        chat.snapshot
            .permissions
            .retain(|request| request.execution.run_id() != Some(run));
        chat.snapshot
            .questions
            .retain(|request| request.run_id != run);
        let mut approvals = self.inner.approvals.lock().await;
        for id in permissions {
            approvals.remove(&id);
        }
        drop(approvals);
        let mut answers = self.inner.question_answers.lock().await;
        for id in questions {
            answers.remove(&id);
        }
    }

    async fn fail_run_completion(&self, receipt: &Receipt, cancel: &CancellationToken) {
        cancel.cancel();
        let mut chats = self.inner.chats.lock().await;
        let Some(chat) = chats.get_mut(&receipt.chat_id) else {
            return;
        };
        clear_run_state(chat, receipt.run_id);
        chat.snapshot
            .request_usage
            .fail_in_flight_requests(receipt.run_id, RequestFailure::Internal);
        self.clear_run_interactions(chat, receipt.run_id).await;
        let history_recovered =
            recovery::close_dangling_tool_calls(&mut chat.snapshot, Some(receipt.run_id)).is_ok();
        let chat_loop = &mut chat.snapshot.chat_loop;
        if chat_loop.run_id == Some(receipt.run_id) {
            chat_loop.set_state(LoopState::Failed);
        }
        chat.feedback_inputs.remove(&receipt.run_id);
        chat.family_finishing.remove(&receipt.run_id);
        let message = if history_recovered {
            INTERNAL_COMPLETION_ERROR
        } else {
            "Agent internal error while finishing the run. Tool history recovery also failed; reconcile pending operations before continuing. The run was not automatically retried."
        };
        self.end_response(
            chat,
            receipt.run_id,
            ResponseEnding::Failed {
                error: message.into(),
            },
            Default::default(),
        );
        self.emit(chat, receipt, EventKind::Failed(message.into()));
        let snapshot = chat.snapshot.clone();
        drop(chats);
        self.inner
            .host_contexts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .runs
            .remove(&receipt.run_id);
        let cleanup = AssertUnwindSafe(async {
            self.settle_task_claims(receipt).await;
            if snapshot.delegation.is_some() {
                self.record_family_completion(receipt, &snapshot).await;
            }
        })
        .catch_unwind()
        .await;
        if cleanup.is_err() {
            let mut chats = self.inner.chats.lock().await;
            if let Some(chat) = chats.get_mut(&receipt.chat_id) {
                self.emit(
                    chat,
                    receipt,
                    EventKind::Failed(INTERNAL_COMPLETION_ERROR.into()),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tickloop_agent_model::ModelClient;
    use tickloop_agent_protocol::{ModelRequestKind, RequestOutcome, RequestOwner};
    use tickloop_agent_test_support::{ScriptedModel, config, text};
    use uuid::Uuid;

    use super::*;

    #[tokio::test]
    async fn a_panic_in_run_completion_still_releases_the_slot_and_records_the_failure() {
        let directory = tempfile::tempdir().unwrap();
        let model = Arc::new(ScriptedModel::new(vec![text("done")]));
        let agent = Agent::new_embedded(config(), model.clone());
        let chat_id = agent.create_chat(directory.path().into()).await.unwrap();
        let receipt = Receipt {
            chat_id,
            loop_id: Uuid::new_v4(),
            run_id: Uuid::new_v4(),
        };
        let cancel = CancellationToken::new();
        {
            let mut chats = agent.inner.chats.lock().await;
            let chat = chats.get_mut(&chat_id).unwrap();
            chat.active = Some(cancel.clone());
            chat.active_run = Some(receipt.run_id);
            chat.snapshot
                .request_usage
                .start(
                    RequestOwner {
                        chat_id,
                        loop_id: Some(receipt.loop_id),
                        run_id: Some(receipt.run_id),
                    },
                    ModelRequestKind::Conversation,
                    model.identity(),
                )
                .unwrap();
        }
        // A receipt for another Loop breaks the one-Loop invariant inside completion, after
        // execution has ended.
        let request = ModelRequest {
            history_sources: vec![],
            messages: vec![],
            tools: vec![],
            structured_output: None,
            temperature: None,
            reasoning_effort: None,
        };
        assert!(agent.run(receipt.clone(), request, cancel).await.is_none());
        let chats = agent.inner.chats.lock().await;
        let chat = chats.get(&chat_id).unwrap();
        assert!(chat.active.is_none());
        assert!(chat.active_run.is_none());
        assert!(chat.snapshot.pending_thinking.is_empty());
        assert!(
            chat.snapshot
                .request_usage
                .entries()
                .iter()
                .filter(|request| request.run_id == Some(receipt.run_id))
                .all(|request| request.outcome != RequestOutcome::InFlight)
        );
        assert!(chat.snapshot.failure_events.iter().any(|event| event.run_id == receipt.run_id && matches!(&event.kind, EventKind::Failed(message) if message.contains("finishing the run"))));
        drop(chats);
        agent.shutdown().await;
    }
}
