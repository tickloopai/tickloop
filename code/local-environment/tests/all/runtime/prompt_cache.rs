//! The prompt cache prefix across what a Chat does: none of the Agent's own work, nor the state
//! it changes, alters a byte of the listed tools or of the system part that leads each request.

use std::sync::Arc;

use serde_json::json;
use tick_protocol::InstanceRef;
use tickloop_harness::model::ModelRequest;
use tickloop_harness::protocol::{
    ChatId, Content, EventKind, ExecutionMode, Role, TickSubscriptionChange,
};
use tickloop_harness::runtime::Agent;
use tickloop_harness::runtime::tick_stream::InMemoryTickStream;
use tickloop_harness::test_support::routed::{RoutedModel, has_tool_result, state};
use tickloop_harness::test_support::subagents::{task_notifications, wait_for_snapshot};
use tickloop_harness::test_support::{call, config, is_host_note, next_event, text};

fn instance() -> InstanceRef {
    InstanceRef::new("calendar", "fixture-run-7").unwrap()
}

/// The user's prompt a request answers: its latest user text that is neither a host note nor a
/// notification.
fn prompt(request: &ModelRequest) -> Option<&str> {
    request
        .messages
        .iter()
        .rev()
        .filter(|message| message.role == Role::User && !is_host_note(message))
        .find_map(|message| match message.content.first() {
            Some(Content::Text { text }) => Some(text.as_str()),
            _ => None,
        })
        .filter(|text| !text.starts_with('<'))
}

async fn complete(agent: &Agent, chat: ChatId, prompt: &str) {
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, prompt.into()).await.unwrap();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != receipt.run_id {
            continue;
        }
        match event.kind {
            EventKind::Completed { .. } => return,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
}

/// A tool call, turning web search off and on, changing the execution mode, subscribing and the
/// user cancelling the subscription, a Subagent's creation and completion and a new day: every
/// request of the Chat sends the same tools and the same system part.
#[tokio::test(start_paused = true)]
async fn the_agents_own_work_never_changes_the_prompt_cache_prefix() {
    use chrono::TimeZone;
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("notes.txt"), "Delivery is Friday.").unwrap();
    let notes = directory.path().join("notes.txt");
    let model = Arc::new(RoutedModel::new(move |request| {
        if !state(request)["delegation"].is_null() {
            return text("CHILD-FINAL-REPORT");
        }
        match prompt(request) {
            Some("Read the notes") if !has_tool_result(request, "read") => {
                call("read", "Read", json!({"file_path": notes}))
            }
            Some("Watch the calendar") if !has_tool_result(request, "subscribe") => call(
                "subscribe",
                "TickManage",
                json!({"action": "subscribe", "tick_instance_id": instance()}),
            ),
            Some("Delegate the check") if !has_tool_result(request, "delegate") => call(
                "delegate",
                "Subagent",
                json!({"task": "Check the notes", "name": "helper", "run_in_background": true}),
            ),
            _ => text("Done."),
        }
    }));
    let agent = Agent::new_embedded(config(), model.clone());
    agent
        .install_tick_stream(Arc::new(InMemoryTickStream::new()))
        .await;
    agent.set_scheduled_now_for_tests(
        chrono::Utc
            .with_ymd_and_hms(2026, 10, 26, 10, 0, 0)
            .unwrap(),
    );
    let chat = agent.create_chat(directory.path().into()).await.unwrap();

    complete(&agent, chat, "Read the notes").await;
    agent.set_search(chat, None).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Plan)
        .await
        .unwrap();
    complete(&agent, chat, "Only plan for now").await;
    agent
        .set_execution_mode(chat, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    agent
        .set_search(
            chat,
            tickloop_harness::protocol::SearchConfig::enabled_default(),
        )
        .await
        .unwrap();
    complete(&agent, chat, "Watch the calendar").await;
    let subscription = agent.tick_subscriptions(chat).await.unwrap()[0].id;
    agent
        .update_tick_subscription(chat, subscription, TickSubscriptionChange::Cancel)
        .await
        .unwrap();
    complete(&agent, chat, "Delegate the check").await;
    wait_for_snapshot(
        &agent,
        chat.uuid(),
        "the Subagent's completion",
        |snapshot| {
            task_notifications(snapshot).len() == 1
                && snapshot.chat_loop.state == tickloop_harness::protocol::LoopState::Idle
        },
    )
    .await;
    agent.set_scheduled_now_for_tests(
        chrono::Utc
            .with_ymd_and_hms(2026, 10, 27, 10, 0, 0)
            .unwrap(),
    );
    complete(&agent, chat, "A new day").await;
    agent.shutdown().await;

    let requests = model
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| !request.tools.is_empty() && state(request)["delegation"].is_null())
        .cloned()
        .collect::<Vec<_>>();
    // Read and its answer, Plan, the subscription and its answer, the delegation and its answer,
    // the completion and the new day.
    assert_eq!(requests.len(), 8);
    for (index, request) in requests.iter().enumerate() {
        assert_eq!(
            serde_json::to_vec(&request.tools).unwrap(),
            serde_json::to_vec(&requests[0].tools).unwrap(),
            "the listed tools of request {index}"
        );
        assert_eq!(
            serde_json::to_vec(&request.messages[..2]).unwrap(),
            serde_json::to_vec(&requests[0].messages[..2]).unwrap(),
            "the system part of request {index}"
        );
    }
    // Each change reached the model at the end of the conversation instead.
    let last = serde_json::to_string(&requests.last().unwrap().messages).unwrap();
    for told in [
        "Plan mode is active.",
        "## Exited Plan Mode",
        "Tick Subscriptions changed outside your tool calls",
        "<task-notification>",
        "The date has changed",
    ] {
        assert!(last.contains(told), "{told}");
    }
}
