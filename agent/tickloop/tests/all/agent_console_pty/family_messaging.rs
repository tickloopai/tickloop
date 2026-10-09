use super::*;

fn request_state(request: &Value) -> Value {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|message| {
            (message["role"] == "system")
                .then(|| message["content"].as_str())
                .flatten()
                .and_then(|text| text.rsplit_once("Current host state: "))
                .and_then(|(_, state)| serde_json::from_str(state).ok())
        })
        .expect("real model host state")
}

fn has_result(request: &Value, id: &str) -> bool {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|message| message["tool_call_id"] == id)
}

async fn run_family_messaging(fixture: &PrivateFixture) {
    let (url, mut requests) = model_http_support::serve_router(|request| {
        let state = request_state(request);
        let history = request["messages"].to_string();
        if state["delegation"].is_null() {
            if history.contains("STOP-WORKER") {
                if !has_result(request, "stop") {
                    return chat_reply(Some(("StopSubagent", "stop", json!({"to":"audit"}))), "");
                }
                return chat_reply(None, "Child stop confirmed");
            }
            if !has_result(request, "audit-delegate") {
                return chat_reply(Some(("Subagent", "audit-delegate", json!({"task":"Audit bounded evidence","name":"audit","description":"Audit bounded evidence"}))), "");
            }
            if !has_result(request, "echo-delegate") {
                return chat_reply(Some(("Subagent", "echo-delegate", json!({"task":"Echo bounded evidence","name":"echo","description":"Echo bounded evidence"}))), "");
            }
            chat_reply(None, "Root family ready")
        } else if state["delegation"]["parent_call_id"] == "audit-delegate" {
            if history.contains("DIRECT-HELLO") {
                if !has_result(request, "sibling-send") {
                    return chat_reply(Some(("SendMessage", "sibling-send", json!({"to":"echo","message":"PEER-CONTEXT"}))), "");
                }
                chat_reply(None, "Direct human and sibling send accepted")
            } else {
                chat_reply(None, "Audit child ready")
            }
        } else if history.contains("PEER-CONTEXT") {
            chat_reply(None, "Sibling context received")
        } else {
            chat_reply(None, "Echo child ready")
        }
    }).await;
    fixture.set_chat_model(&url);
    let mut pty = PtyView::start_with_flags(fixture, &["--dangerously-skip-permissions"]);
    pty.wait_for("new family input", new_input_is_visible).await;
    let client = fixture.client().await;
    pty.send(b"Delegate two bounded children\r");
    pty.wait_for("root and real delegated children", |frame| {
        frame.contains("Root family ready") && frame.contains("Subagents 2")
    })
    .await;
    let rows = catalog(&client).await;
    let root = rows
        .iter()
        .find(|row| row.parent.is_none())
        .unwrap()
        .execution;
    let audit = rows
        .iter()
        .find(|row| {
            row.subagent
                .as_ref()
                .is_some_and(|entry| entry.handle == "audit")
        })
        .unwrap()
        .execution;
    let echo = rows
        .iter()
        .find(|row| {
            row.subagent
                .as_ref()
                .is_some_and(|entry| entry.handle == "echo")
        })
        .unwrap()
        .execution;
    assert!(
        !String::from_utf8_lossy(&pty.output.lock().unwrap()).contains("Subagent @audit finished"),
        "foreground audit must not print a separate live completion card"
    );
    let before = snapshot(&client, root).await;
    while requests.try_recv().is_ok() {}
    pty.send(b"@a");
    pty.wait_for("Agent completion group before Files", |frame| {
        frame.contains("Agents") && frame.contains("Audit bounded evidence")
    })
    .await;
    pty.send(b"\x0e\r");
    pty.wait_for("selected handle chip", |frame| frame.contains("@audit"))
        .await;
    pty.send(b"DIRECT-HELLO\r");
    pty.wait_for("source receipt and response preview", |frame| {
        frame.contains("You → @audit") && frame.contains("Reply from @audit")
    })
    .await;
    let target = wait_snapshot(
        &client,
        audit,
        "genuine human recipient completes",
        |current| {
            current
                .conversation
                .human_family_origins
                .values()
                .any(|source| source.original_submission.contains("@audit DIRECT-HELLO"))
                && current.conversation.loops[0].state
                    == tickloop_agent_protocol::LoopState::Completed
        },
    )
    .await;
    assert!(target.conversation.messages.iter().any(|message| message.role == tickloop_agent_protocol::Role::User && message.content.iter().any(|content| matches!(content, tickloop_agent_protocol::Content::Text { text } if text == "DIRECT-HELLO"))));
    let sibling = wait_snapshot(&client, echo, "model sibling delivery completes", |current| !current.conversation.agent_message_origins.is_empty() && current.conversation.messages.iter().any(|message| message.content.iter().any(|content| matches!(content, tickloop_agent_protocol::Content::Text { text } if text == "Sibling context received")))).await;
    assert!(
        sibling
            .conversation
            .agent_message_origins
            .values()
            .any(|source| source["sender_relationship"] == "sibling")
    );
    let after = snapshot(&client, root).await;
    assert_eq!(
        after.conversation.prompt_execution_modes, before.conversation.prompt_execution_modes,
        "directed input is not a source human checkpoint"
    );
    assert!(after.conversation.messages.iter().flat_map(|message|&message.content).any(|content|matches!(content,tickloop_agent_protocol::Content::Text {text} if text.starts_with("<task-notification>") && text.contains("@echo"))),"the sibling's background completion must reach the root");
    let mut child_requests = 0;
    while let Ok((_, request)) = requests.try_recv() {
        if request_state(&request)["delegation"].is_null() {
            assert!(
                request["messages"]
                    .to_string()
                    .contains("<task-notification>"),
                "only the independent background completion wakes the source"
            );
        } else {
            child_requests += 1;
        }
    }
    assert!(child_requests >= 3);
    pty.send(b"STOP-WORKER\r");
    pty.wait_for("root-only stop tool in actual daemon", |frame| {
        frame.contains("Child stop confirmed")
    })
    .await;
    assert_eq!(
        snapshot(&client, audit).await.conversation.loops[0].state,
        tickloop_agent_protocol::LoopState::Stopped
    );
    assert_eq!(
        snapshot(&client, echo).await.conversation.loops[0].state,
        tickloop_agent_protocol::LoopState::Completed
    );
    pty.close().await;
}

#[tokio::test]
async fn actual_cli_at_handle_and_sibling_send_have_receipts_previews_and_root_only_stop() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(run_family_messaging(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
