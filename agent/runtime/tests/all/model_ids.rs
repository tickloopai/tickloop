//! Tickloop ids as the model reads and writes them: in the Loop's model form in what it reads,
//! and in that form or as a UUID from an older session in what it passes back.

use std::sync::Arc;

use serde_json::{Value, json};
use tickloop_agent_model::ModelRequest;
use tickloop_agent_protocol::{ChatId, ChatKind, Content, EventKind, Role};
use tickloop_agent_runtime::Agent;
use tickloop_agent_test_support::{
    call, config, next_event,
    routed::{RoutedModel, has_tool_result, state, tool_result},
    text,
};
use tickloop_ids::IdContext;

fn child_of(request: &ModelRequest) -> ChatId {
    let launched = tool_result(&request.messages, "spawn").unwrap();
    ChatId::parse(launched["child_chat_id"].as_str().unwrap()).unwrap()
}

fn tool_text(request: &ModelRequest, call_id: &str) -> String {
    request
        .messages
        .iter()
        .filter(|message| message.role == Role::User)
        .flat_map(|message| &message.content)
        .find_map(|content| match content {
            Content::ToolResult {
                tool_use_id,
                content,
                ..
            } if tool_use_id == call_id => Some(content.text().into_owned()),
            _ => None,
        })
        .unwrap()
}

#[tokio::test]
async fn send_message_reaches_a_child_by_the_id_the_model_read_or_by_its_uuid() {
    let model = Arc::new(RoutedModel::new(|request| {
        if !state(request)["delegation"].is_null() {
            return text("child done");
        }
        if !has_tool_result(request, "spawn") {
            return call(
                "spawn",
                "Subagent",
                json!({"name":"worker","task":"wait","run_in_background":true}),
            );
        }
        let ids = IdContext::new();
        let child = child_of(request);
        if !has_tool_result(request, "as-read") {
            return call(
                "as-read",
                "SendMessage",
                json!({"to":ids.model_text(child),"message":"first"}),
            );
        }
        if !has_tool_result(request, "as-uuid") {
            return call(
                "as-uuid",
                "SendMessage",
                json!({"to":child.uuid().to_string(),"message":"second"}),
            );
        }
        if !has_tool_result(request, "unknown") {
            return call(
                "unknown",
                "SendMessage",
                json!({"to":"nobody-here","message":"third"}),
            );
        }
        text("done")
    }));
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, "talk to a worker".into()).await.unwrap();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id == receipt.run_id
            && matches!(
                event.kind,
                EventKind::Completed { .. } | EventKind::Failed(_) | EventKind::Interrupted
            )
        {
            break;
        }
    }
    let requests = model.requests.lock().unwrap().clone();
    let last = requests
        .iter()
        .rev()
        .find(|request| state(request)["delegation"].is_null())
        .unwrap();
    let ids = IdContext::new();
    let child = child_of(last);
    let launched = tool_result(&last.messages, "spawn").unwrap();
    for key in ["child_chat_id", "child_loop_id", "child_run_id"] {
        let written = launched[key].as_str().unwrap();
        assert!(!written.contains('-'), "{key} {written}");
    }
    let members = state(last)["family_members"].clone();
    assert!(
        members
            .as_array()
            .unwrap()
            .iter()
            .any(|member| member["chat_id"] == ids.model_text(child)),
        "{members}"
    );
    for call_id in ["as-read", "as-uuid"] {
        let accepted: Value = serde_json::from_str(&tool_text(last, call_id)).unwrap();
        assert_eq!(accepted["status"], "accepted", "{call_id}: {accepted}");
        assert_eq!(accepted["target_chat_id"], ids.model_text(child));
    }
    let rejected: Value = serde_json::from_str(&tool_text(last, "unknown")).unwrap();
    assert_eq!(
        rejected["rejection"]["message"],
        tickloop_ids::not_found::<ChatKind>("nobody-here")
    );
    agent.shutdown().await;
}
