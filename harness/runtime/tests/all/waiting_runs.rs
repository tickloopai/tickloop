use std::sync::Arc;
use tickloop_harness_protocol::{EventKind, LoopState, QuestionAnswer};
use tickloop_harness_runtime::Agent;
use tickloop_harness_test_support::*;
use tickloop_harness_tools::ASK_USER_QUESTION_TOOL_NAME;

struct GatedSecondModel {
    model: Arc<ScriptedModel>,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    calls: std::sync::atomic::AtomicUsize,
}

#[tokio::test]
async fn a_queued_follow_up_preserves_the_running_turns_ability_to_ask_for_clarification() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(GatedSecondModel {
        model: Arc::new(ScriptedModel::new(vec![
            text("Original requirement noted."),
            call(
                "question",
                ASK_USER_QUESTION_TOOL_NAME,
                single_question("Which format?"),
            ),
            text("Clarification received."),
            text("Additional input applied."),
        ])),
        entered: Arc::new(tokio::sync::Notify::new()),
        release: Arc::new(tokio::sync::Notify::new()),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Prepare a report.".into())
        .await
        .unwrap();
    loop {
        if matches!(
            next_event(&mut events).await.kind,
            EventKind::Completed { .. }
        ) {
            break;
        }
    }
    let running = agent
        .submit(chat, "Clarify the format.".into())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), model.entered.notified())
        .await
        .unwrap();
    let queued = agent
        .submit(chat, "Also keep the report brief.".into())
        .await
        .unwrap();
    assert_eq!(
        agent.snapshot(chat).await.unwrap().chat_loop.state,
        LoopState::Running
    );
    model.release.notify_one();
    let mut completed = Vec::new();
    let mut merged = false;
    while completed.is_empty() || !merged {
        let event = next_event(&mut events).await;
        match event.kind {
            EventKind::QuestionAsked { request } => {
                assert_eq!(request.run_id, running.run_id);
                agent
                    .answer_question(chat, request.id, vec![QuestionAnswer::other("Markdown")])
                    .await
                    .unwrap();
            }
            EventKind::Completed { .. } => completed.push(event.run_id),
            EventKind::InputMerged { into_run_id } if event.run_id == queued.run_id => {
                assert_eq!(into_run_id, running.run_id);
                merged = true;
            }
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert_eq!(completed, vec![running.run_id]);
    agent.shutdown().await;
}

#[async_trait::async_trait]
impl tickloop_harness_model::ModelClient for GatedSecondModel {
    async fn generate_title(
        &self,
        request: tickloop_harness_model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_harness_model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_harness_model::ModelOutput, tickloop_harness_model::ModelError> {
        Ok(tickloop_harness_test_support::title_output(request))
    }

    fn identity(&self) -> tickloop_harness_protocol::ModelIdentity {
        self.model.identity()
    }

    async fn complete(
        &self,
        request: tickloop_harness_model::ModelRequest,
        events: tokio::sync::mpsc::UnboundedSender<tickloop_harness_model::ModelDelta>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_harness_model::ModelOutput, tickloop_harness_model::ModelError> {
        use std::sync::atomic::Ordering::SeqCst;
        if self.calls.fetch_add(1, SeqCst) == 1 {
            self.entered.notify_one();
            tokio::select! {_=cancel.cancelled()=>{},_=self.release.notified()=>{}}
        }
        self.model.complete(request, events, cancel).await
    }
}
