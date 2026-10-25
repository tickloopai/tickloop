use async_trait::async_trait;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tickloop_harness_model::*;
use tickloop_harness_protocol::{Content, EventKind, LoopState, Role, Usage};
use tickloop_harness_runtime::Agent;
use tickloop_harness_test_support::*;
use tokio::sync::{Notify, mpsc::UnboundedSender};
use tokio_util::sync::CancellationToken;

struct ConcurrentModel {
    first_alpha_seen: AtomicBool,
    alpha_started: Notify,
    release_alpha: CancellationToken,
    requests: Mutex<Vec<ModelRequest>>,
    calls: AtomicUsize,
}

#[async_trait]
impl ModelClient for ConcurrentModel {
    async fn generate_title(
        &self,
        request: tickloop_harness_model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_harness_model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_harness_model::ModelOutput, tickloop_harness_model::ModelError> {
        Ok(tickloop_harness_test_support::title_output(request))
    }

    fn identity(&self) -> tickloop_harness_protocol::ModelIdentity {
        tickloop_harness_protocol::ModelIdentity {
            provider: tickloop_harness_protocol::ModelProvider::Api,
            profile: "fixture".into(),
            protocol: tickloop_harness_protocol::Protocol::Messages,
            requested_model: "fixture".into(),
        }
    }

    async fn complete(
        &self,
        request: ModelRequest,
        _: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        self.requests.lock().unwrap().push(request.clone());
        let latest = request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == Role::User)
            .unwrap();
        let content = if let Some(Content::ToolResult { content, .. }) = latest.content.first() {
            vec![Content::Text {
                text: content.text().into_owned(),
            }]
        } else {
            if latest
                .content
                .iter()
                .any(|content| matches!(content, Content::Text {text} if text == "alpha-first"))
                && !self.first_alpha_seen.swap(true, Ordering::SeqCst)
            {
                self.alpha_started.notify_one();
                tokio::select! {
                    _ = self.release_alpha.cancelled() => {},
                    _ = cancel.cancelled() => return Err(ModelError::Cancelled),
                }
            }
            let id = format!("read-{}", self.calls.fetch_add(1, Ordering::SeqCst));
            call(&id, "Read", serde_json::json!({"file_path":"value.txt"}))
        };
        Ok(ModelOutput {
            private_source: None,
            content,
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

#[tokio::test]
async fn two_chats_progress_independently_and_share_only_their_own_history() {
    let alpha_directory = tempfile::tempdir().unwrap();
    let beta_directory = tempfile::tempdir().unwrap();
    let moved_beta_directory = tempfile::tempdir().unwrap();
    std::fs::write(
        alpha_directory.path().join("value.txt"),
        "alpha-private-file",
    )
    .unwrap();
    std::fs::write(beta_directory.path().join("value.txt"), "unused-beta-file").unwrap();
    std::fs::write(
        moved_beta_directory.path().join("value.txt"),
        "beta-private-file",
    )
    .unwrap();
    let model = Arc::new(ConcurrentModel {
        first_alpha_seen: AtomicBool::new(false),
        alpha_started: Notify::new(),
        release_alpha: CancellationToken::new(),
        requests: Mutex::new(Vec::new()),
        calls: AtomicUsize::new(0),
    });
    let agent = Agent::new_embedded(config(), model.clone());
    let alpha = agent
        .create_chat(alpha_directory.path().into())
        .await
        .unwrap();
    let beta = agent
        .create_chat(beta_directory.path().into())
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let first = agent.submit(alpha, "alpha-first".into()).await.unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        model.alpha_started.notified(),
    )
    .await
    .unwrap();
    let second = agent.submit(alpha, "alpha-second".into()).await.unwrap();
    agent
        .set_directories(beta, moved_beta_directory.path().into(), Vec::new())
        .await
        .unwrap();
    let beta_receipt = agent.submit(beta, "beta-only".into()).await.unwrap();
    loop {
        let event = next_event(&mut events).await;
        match event.kind {
            EventKind::Completed { .. } if event.run_id == beta_receipt.run_id => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    let snapshot = agent.snapshot(alpha).await.unwrap();
    assert_eq!(snapshot.chat_loop.state, LoopState::Running);
    assert_eq!(snapshot.pending_inputs[0].receipt.run_id, second.run_id);
    assert_eq!(
        snapshot.folder,
        Some(alpha_directory.path().canonicalize().unwrap())
    );
    let beta_history =
        serde_json::to_string(&agent.snapshot(beta).await.unwrap().messages).unwrap();
    assert!(beta_history.contains("beta-private-file"));
    assert!(!beta_history.contains("alpha"));
    model.release_alpha.cancel();
    let mut merged = false;
    loop {
        let event = next_event(&mut events).await;
        match event.kind {
            EventKind::InputMerged { into_run_id } if event.run_id == second.run_id => {
                assert_eq!(into_run_id, first.run_id);
                merged = true;
            }
            EventKind::Completed { .. } if event.chat_id == alpha => {
                assert_eq!(event.run_id, first.run_id);
                break;
            }
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert!(
        merged,
        "the second alpha input joins the running turn of its Chat's Loop"
    );
    let history = serde_json::to_string(&agent.snapshot(alpha).await.unwrap().messages).unwrap();
    assert!(history.contains("alpha-private-file"));
    assert!(!history.contains("beta"));
    let requests = model.requests.lock().unwrap().clone();
    for request in requests.iter().filter(|request| {
        serde_json::to_string(&request.messages)
            .unwrap()
            .contains("beta-only")
    }) {
        assert!(
            !serde_json::to_string(&request.messages)
                .unwrap()
                .contains("alpha")
        );
    }
    assert!(requests.iter().any(|request| {
        let text = serde_json::to_string(&request.messages).unwrap();
        text.contains("alpha-second") && text.contains("alpha-private-file")
    }));
    agent.shutdown().await;
}
