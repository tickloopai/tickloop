use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use tickloop_harness_model::{
    ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest, ModelResponseRejection,
};
use tickloop_harness_protocol::{Content, EventKind, ModelIdentity, Role, Usage};
use tickloop_harness_runtime::Agent;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

enum Reply {
    Truncated(&'static str),
    CompleteWithThinking(&'static str),
    WaitForCancel(&'static str),
}

struct OutputLimitModel {
    replies: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<ModelRequest>>,
}

impl OutputLimitModel {
    fn new(replies: Vec<Reply>) -> Self {
        Self {
            replies: Mutex::new(replies.into()),
            requests: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl ModelClient for OutputLimitModel {
    async fn generate_title(
        &self,
        request: tickloop_harness_model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_harness_model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_harness_model::ModelOutput, tickloop_harness_model::ModelError> {
        Ok(tickloop_harness_test_support::title_output(request))
    }

    fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            provider: tickloop_harness_protocol::ModelProvider::Api,
            profile: "fixture".into(),
            protocol: tickloop_harness_protocol::Protocol::Messages,
            requested_model: "fixture".into(),
        }
    }

    async fn complete(
        &self,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        self.requests.lock().unwrap().push(request);
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected model request");
        match reply {
            Reply::Truncated(text) => {
                if !text.is_empty() {
                    events.send(ModelDelta::Text(text.into())).unwrap();
                }
                Err(ModelError::ResponseRejected(
                    ModelResponseRejection::OutputLimit,
                ))
            }
            Reply::CompleteWithThinking(text) => {
                events.send(ModelDelta::Text(text.into())).unwrap();
                Ok(ModelOutput {
                    private_source: None,
                    content: vec![
                        Content::Thinking {
                            thinking: "private reasoning".into(),
                            signature: Some("provider signature".into()),
                        },
                        Content::Text { text: text.into() },
                    ],
                    usage: Usage::default(),
                    stop_reason: "end_turn".into(),
                })
            }
            Reply::WaitForCancel(text) => {
                events.send(ModelDelta::Text(text.into())).unwrap();
                cancel.cancelled().await;
                Err(ModelError::Cancelled)
            }
        }
    }
}

#[tokio::test]
async fn interruption_during_continuation_preserves_all_streamed_text() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(OutputLimitModel::new(vec![
        Reply::Truncated("First. "),
        Reply::WaitForCancel("Second."),
    ]));
    let agent = Agent::new_embedded(tickloop_harness_test_support::config(), model);
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Answer fully".into()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.chat_id == chat
                && matches!(&event.kind, EventKind::Text(text) if text == "Second.")
            {
                break;
            }
        }
    })
    .await
    .expect("continuation did not stream");
    agent.interrupt(chat).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.chat_id == chat && matches!(event.kind, EventKind::Interrupted) {
                break;
            }
        }
    })
    .await
    .expect("run did not stop");
    let snapshot = agent.snapshot(chat).await.unwrap();
    let assistant_messages = snapshot
        .messages
        .iter()
        .filter(|message| message.role == Role::Assistant)
        .collect::<Vec<_>>();
    assert_eq!(assistant_messages.len(), 1);
    assert_eq!(
        assistant_messages[0].content,
        vec![Content::Text {
            text: "First. Second.".into()
        }]
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn output_limit_continues_with_partial_text_and_commits_one_complete_reply() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(OutputLimitModel::new(vec![
        Reply::Truncated("First half. "),
        Reply::CompleteWithThinking("Second half."),
    ]));
    let agent = Agent::new_embedded(tickloop_harness_test_support::config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Answer fully".into()).await.unwrap();
    let mut streamed_text = String::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.chat_id == chat
                && let EventKind::Text(text) = &event.kind
            {
                streamed_text.push_str(text);
            }
            if event.chat_id == chat && matches!(&event.kind, EventKind::Completed { .. }) {
                break;
            }
            if let EventKind::Failed(error) = event.kind {
                panic!("recovery failed: {error}");
            }
        }
    })
    .await
    .expect("run did not finish");
    assert_eq!(streamed_text, "First half. Second half.");

    {
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].messages.iter().any(|message| {
            message.role == Role::Assistant
                && message.content.iter().any(
                    |content| matches!(content, Content::Text { text } if text == "First half. "),
                )
        }));
        assert!(requests[1].messages.iter().any(|message| {
            message.role == Role::User
                && message.content.iter().any(
                    |content| matches!(content, Content::Text { text } if text.to_ascii_lowercase().contains("continue")),
                )
        }));
    }

    let snapshot = agent.snapshot(chat).await.unwrap();
    let assistant = snapshot
        .messages
        .iter()
        .find(|message| message.role == Role::Assistant)
        .unwrap();
    assert_eq!(
        assistant.content,
        vec![Content::Text {
            text: "First half. Second half.".into()
        }]
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn output_limit_without_complete_text_fails_without_retrying_a_partial_tool_call() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(OutputLimitModel::new(vec![Reply::Truncated("")]));
    let agent = Agent::new_embedded(tickloop_harness_test_support::config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Use a tool".into()).await.unwrap();
    let failure = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.chat_id == chat
                && let EventKind::Failed(error) = event.kind
            {
                return error;
            }
        }
    })
    .await
    .expect("run did not fail");
    assert!(failure.contains("output token limit"));
    assert_eq!(model.requests.lock().unwrap().len(), 1);
    assert!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .messages
            .iter()
            .all(|message| message.role != Role::Assistant)
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn output_limit_recovery_stops_after_three_continuations() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(OutputLimitModel::new(vec![
        Reply::Truncated("One. "),
        Reply::Truncated("Two. "),
        Reply::Truncated("Three. "),
        Reply::Truncated("Four. "),
    ]));
    let agent = Agent::new_embedded(tickloop_harness_test_support::config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Answer fully".into()).await.unwrap();
    let failure = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.chat_id == chat
                && let EventKind::Failed(error) = event.kind
            {
                return error;
            }
        }
    })
    .await
    .expect("run did not finish");
    assert!(failure.contains("output token limit"));
    assert_eq!(model.requests.lock().unwrap().len(), 4);
    assert!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .messages
            .iter()
            .all(|message| message.role != Role::Assistant)
    );
    agent.shutdown().await;
}
