//! A Chat's status is the state of its snapshot without the history.

use serde_json::json;
use tickloop_harness_protocol::{
    ChatSnapshot, ChatStatus, CommandSnapshot, CommandState, Event, EventKind, PermissionExecution,
    PermissionRequest, QuestionRequest, chat_preview,
};
use uuid::Uuid;

fn id(value: u128) -> Uuid {
    Uuid::from_u128(value)
}

fn event(sequence: u64, kind: EventKind) -> Event {
    Event {
        sequence,
        chat_id: tickloop_ids::Id::from(id(0xc1)),
        loop_id: tickloop_ids::Id::from(id(0x10)),
        run_id: tickloop_ids::Id::from(id(0x20)),
        kind,
    }
}

fn command(updated_at_ms: u64) -> CommandSnapshot {
    CommandSnapshot {
        operation_id: id(0x60),
        loop_id: id(0x10),
        command: "cargo test".into(),
        description: None,
        cwd: "/work".into(),
        pid: Some(7),
        state: CommandState::Background,
        was_background: true,
        backgrounded_on_timeout: false,
        exit_code: None,
        signal: None,
        reason: None,
        output_ref: "/tmp/out".into(),
        output_bytes: 12,
        updated_at_ms: Some(updated_at_ms),
    }
}

/// A Chat with a long history and every kind of state a status carries.
fn busy_chat() -> ChatSnapshot {
    let pasted = |id: u32| json!([{"kind":"image","origin":{"type":"pasted","id":id},"image":{"media_type":"image/png","data":"iVBORw0KGgo=","width":1,"height":1}}]);
    let mut snapshot: ChatSnapshot = serde_json::from_value(json!({
        "id": id(0xc1),
        "folder": "/work",
        "messages": (0..200).map(|index| json!({
            "role": if index % 2 == 0 { "user" } else { "assistant" },
            "content": [{"type": "text", "text": format!("message {index} {}", "history ".repeat(50))}],
        })).collect::<Vec<_>>(),
        "message_runs": {"4": id(0x21), "6": id(0x22)},
        "message_ids": {"4": id(0x91), "6": id(0x92)},
        "message_contexts": {"0": pasted(3)},
        "agent_message_origins": {id(0x91).to_string(): {"from": "child"}},
        "chat_loop": {"id": id(0x10), "state": "Running", "run_id": id(0x20)},
        "plans": [{"loop_id": id(0x10), "version": 2, "markdown": "Plan", "approved_execution_mode": null, "approval_pending": true}],
    }))
    .unwrap();
    let permission = PermissionRequest {
        mcp_server: None,
        id: id(0x70),
        loop_id: tickloop_ids::Id::from(id(0x10)),
        execution: PermissionExecution::ToolCall {
            run_id: tickloop_ids::Id::from(id(0x20)),
            call_id: "call-bash".into(),
        },
        name: "Bash".into(),
        input: json!({"command": "rm -rf build"}),
        file_change: None,
        approval_reason: None,
        approval_source: Default::default(),
    };
    let question = QuestionRequest {
        id: id(0x71),
        chat_id: tickloop_ids::Id::from(id(0xc1)),
        loop_id: tickloop_ids::Id::from(id(0x10)),
        run_id: tickloop_ids::Id::from(id(0x20)),
        call_id: "call-ask".into(),
        questions: Vec::new(),
    };
    snapshot.events = vec![
        event(1, EventKind::Started),
        event(
            2,
            EventKind::CommandUpdated {
                command: command(500),
            },
        ),
        event(
            3,
            EventKind::QuestionAsked {
                request: question.clone(),
            },
        ),
        event(4, EventKind::Thinking("weighing".into())),
        event(
            5,
            EventKind::CommandUpdated {
                command: command(200),
            },
        ),
        event(
            6,
            EventKind::PermissionRequested {
                request: permission.clone(),
            },
        ),
    ];
    snapshot.last_sequence = 6;
    snapshot.permissions = vec![permission];
    snapshot.questions = vec![question];
    snapshot.commands = vec![command(500)];
    snapshot.pending_inputs = vec![
        serde_json::from_value(json!({
            "receipt": {"chat_id": id(0xc1), "loop_id": id(0x10), "run_id": id(0x23)},
            "input": "next",
            "contexts": pasted(5),
            "queued_sequence": 1,
        }))
        .unwrap(),
    ];
    snapshot
        .pending_thinking
        .insert(tickloop_ids::Id::from(id(0x20)), "thinking ".repeat(1_000));
    snapshot
}

#[test]
fn a_status_derives_the_facts_it_needs_from_the_history_it_leaves_out() {
    let status = ChatStatus::of(&busy_chat());
    assert_eq!(status.highest_pasted_image_id, Some(5));
    assert_eq!(
        status.request_order,
        [id(0x71), id(0x70)],
        "the requests in the order they were asked"
    );
    assert_eq!(status.command_started_at_ms[&id(0x60)], 200);
    assert_eq!(
        status.family_input_runs,
        [tickloop_harness_protocol::RunId::from(id(0x21))].into()
    );
    assert!(status.context_tokens.is_some_and(|tokens| tokens > 0));
    assert_eq!(status.last_sequence, 6);
    assert_eq!(status.queued_inputs().len(), 1);

    let wire = serde_json::to_value(&status).unwrap();
    for history in ["messages", "events", "operations", "pending_thinking"] {
        assert!(
            wire.get(history).is_none(),
            "{history} stays out of a status"
        );
    }
    let status_bytes = serde_json::to_vec(&status).unwrap().len();
    let snapshot_bytes = serde_json::to_vec(&busy_chat()).unwrap().len();
    assert!(
        status_bytes * 10 < snapshot_bytes,
        "status {status_bytes} bytes against snapshot {snapshot_bytes} bytes"
    );
}

#[test]
fn a_preview_shows_the_latest_prompt_and_never_a_host_note() {
    let snapshot: ChatSnapshot = serde_json::from_value(json!({
        "id": id(0xc1),
        "folder": "/work",
        "chat_loop": {"id": id(0x10), "state": "Idle"},
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "Build the page"}]},
            {"role": "user", "content": [{"type": "text", "text": "Reference guidance from tickloop active in this Chat"}]},
        ],
        "host_notes": {"1": {"guidance": {"kind": "skill", "name": "tickloop", "digest": "sha256:00"}}},
    }))
    .unwrap();
    assert_eq!(
        chat_preview(&snapshot, "Untitled").as_deref(),
        Some("Build the page")
    );
}

#[test]
fn scheduled_tasks_are_available_without_reading_the_conversation() {
    let mut snapshot = busy_chat();
    let task = serde_json::from_value(serde_json::json!({
        "id":"01980123-abcd-7000-8000-000000000001", "kind":{"type":"recurring", "cron":"* * * * *"},
        "prompt":"weather", "creator":"user", "time_zone":"UTC", "created_at_ms":0,
        "next_fire_at_ms":60000, "jitter_ms":0, "skipped":0
    }))
    .unwrap();
    snapshot.scheduled_tasks = vec![task];
    let status = ChatStatus::of(&snapshot);
    assert_eq!(status.scheduled_tasks, snapshot.scheduled_tasks);
    let wire = serde_json::to_value(&status).unwrap();
    let decoded: ChatStatus = serde_json::from_value(wire).unwrap();
    assert_eq!(decoded.scheduled_tasks, snapshot.scheduled_tasks);
}
