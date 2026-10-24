use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tickloop_agent_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_agent_protocol::{EventKind, ModelIdentity, Role};
use tickloop_agent_runtime::Agent;
use tickloop_agent_test_support::{ScriptedModel, call, config, next_event, text, transcript_file};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

/// A scripted model that announces each call it returns, as providers do while its input streams.
struct AnnouncingModel(ScriptedModel);

#[async_trait]
impl ModelClient for AnnouncingModel {
    async fn generate_title(
        &self,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        self.0.generate_title(request, events, cancel).await
    }

    fn identity(&self) -> ModelIdentity {
        self.0.identity()
    }

    async fn complete(
        &self,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        let output = self.0.complete(request, events.clone(), cancel).await?;
        for block in &output.content {
            if let tickloop_agent_protocol::Content::ToolUse { id, name, .. } = block {
                let _ = events.send(ModelDelta::ToolUseStarted {
                    id: id.clone(),
                    name: name.clone(),
                });
            }
        }
        Ok(output)
    }
}

/// What each request asked, without the host state's ids and times.
fn asked(requests: &[ModelRequest]) -> Vec<String> {
    requests
        .iter()
        .map(|request| {
            let messages = request
                .messages
                .iter()
                .map(|message| {
                    let shown = format!("{message:?}");
                    match shown.split_once("Current host state:") {
                        Some((before, _)) if message.role == Role::System => before.to_owned(),
                        _ => shown,
                    }
                })
                .collect::<Vec<_>>();
            format!("{:?} {messages:?}", request.tools)
        })
        .collect()
}

#[tokio::test]
async fn a_started_tool_use_streams_before_its_commit_and_never_reaches_the_transcript_or_the_model()
 {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    std::fs::write(directory.path().join("notes.txt"), "note body").unwrap();
    let mut reply = text("Reading the notes.");
    reply.extend(call("read-1", "Read", json!({"file_path":"notes.txt"})));
    let model = Arc::new(AnnouncingModel(ScriptedModel::new(vec![
        reply.clone(),
        text("Done."),
    ])));
    let agent = Agent::open_embedded(root.clone(), config(), model.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, "Read the notes".into()).await.unwrap();
    let mut order = Vec::new();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != receipt.run_id {
            continue;
        }
        match event.kind {
            EventKind::Text(text) => order.push(format!("text {text}")),
            EventKind::ToolUseStarted { call_id, name } => {
                order.push(format!("started {call_id} {name}"))
            }
            EventKind::AssistantMessageCommitted { tool_calls, .. } => {
                order.push(format!("committed {} calls", tool_calls.len()))
            }
            EventKind::ToolStarted { call_id, .. } => order.push(format!("running {call_id}")),
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("turn failed: {error}"),
            _ => {}
        }
    }
    assert_eq!(
        order,
        [
            "text Reading the notes.",
            "started read-1 Read",
            "committed 1 calls",
            "running read-1",
            "text Done.",
            "committed 0 calls",
        ]
    );
    agent.shutdown().await;

    let transcript = std::fs::read_to_string(transcript_file(&root, chat.uuid())).unwrap();
    assert!(!transcript.contains("ToolUseStarted"), "{transcript}");
    let requests = model.0.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    assert!(!format!("{:?}", requests[1]).contains("ToolUseStarted"));
    let answer = requests[1]
        .messages
        .iter()
        .find(|message| message.role == Role::Assistant)
        .unwrap();
    assert_eq!(answer.content, reply);

    // The same turn without the announcements asks the model the same.
    let silent = Arc::new(ScriptedModel::new(vec![reply, text("Done.")]));
    let agent = Agent::open_embedded(root.clone(), config(), silent.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, "Read the notes".into()).await.unwrap();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id == receipt.run_id && matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
    }
    agent.shutdown().await;
    let silent_requests = silent.requests.lock().unwrap().clone();
    assert_eq!(asked(&silent_requests), asked(&requests));
}
