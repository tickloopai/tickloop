use async_trait::async_trait;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use tickloop_agent_model::*;
use tickloop_agent_protocol::{Content, EventKind, LoopState, Role, Usage};
use tickloop_agent_runtime::Agent;
use tickloop_agent_test_support::*;
use tokio::sync::{Notify, mpsc::UnboundedSender};
use tokio_util::sync::CancellationToken;

struct ConcurrentModel {
    first_alpha_seen: AtomicBool,
    alpha_started: Notify,
    release_alpha: CancellationToken,
    requests: Mutex<Vec<ModelRequest>>,
}

#[async_trait]
impl ModelClient for ConcurrentModel {
    async fn generate_title(
        &self,
        request: tickloop_agent_model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
        Ok(tickloop_agent_test_support::title_output(request))
    }

    fn identity(&self) -> tickloop_agent_protocol::ModelIdentity {
        tickloop_agent_protocol::ModelIdentity {
            provider: tickloop_agent_protocol::ModelProvider::Api,
            profile: "fixture".into(),
            protocol: tickloop_agent_protocol::Protocol::Messages,
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
            call("read", "Read", serde_json::json!({"file_path":"value.txt"}))
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
async fn two_chats_progress_independently_while_two_goals_share_only_their_own_chat_history() {
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
    let first = agent
        .submit(alpha, None, "alpha-first".into())
        .await
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        model.alpha_started.notified(),
    )
    .await
    .unwrap();
    let second = agent
        .submit(alpha, None, "alpha-second".into())
        .await
        .unwrap();
    agent
        .set_directories(beta, moved_beta_directory.path().into(), Vec::new())
        .await
        .unwrap();
    let beta_receipt = agent.submit(beta, None, "beta-only".into()).await.unwrap();
    loop {
        let event = next_event(&mut events).await;
        match event.kind {
            EventKind::Completed { .. } if event.run_id == beta_receipt.run_id => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    let snapshot = agent.snapshot(alpha).await.unwrap();
    assert_eq!(snapshot.loops[0].state, LoopState::Running);
    assert_eq!(snapshot.loops[1].state, LoopState::Queued);
    assert_eq!(
        snapshot.folder,
        Some(alpha_directory.path().canonicalize().unwrap())
    );
    let beta_history =
        serde_json::to_string(&agent.snapshot(beta).await.unwrap().messages).unwrap();
    assert!(beta_history.contains("beta-private-file"));
    assert!(!beta_history.contains("alpha"));
    model.release_alpha.cancel();
    let mut completed = Vec::new();
    while completed.len() < 2 {
        let event = next_event(&mut events).await;
        match event.kind {
            EventKind::Completed { .. } if event.chat_id == alpha => completed.push(event.run_id),
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert_eq!(completed, vec![first.run_id, second.run_id]);
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
