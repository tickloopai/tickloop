use super::*;
use crate::terminal::{
    agent_messaging::AgentColors,
    console::Console,
    display::DisplayProfile,
    image_cache::ImageLinks,
    presentation::{self, CallFacts, ToolPresentation},
    workflows,
};
use serde_json::json;
use tickloop_harness::protocol::{
    OperationRecord, PermissionExecution, PermissionRequest, ToolResult,
};

fn event(kind: EventKind) -> Event {
    Event {
        sequence: 1,
        chat_id: Uuid::nil().into(),
        loop_id: Uuid::nil().into(),
        run_id: Uuid::from_u128(1).into(),
        kind,
    }
}

fn record(call: &str, path: ApprovalPath, outcome: ApprovalResult) -> ApprovalRecord {
    serde_json::from_value(json!({
        "id":Uuid::from_u128(2),"chat_id":Uuid::nil(),"loop_id":Uuid::nil(),"run_id":Uuid::from_u128(1),
        "operation_id":Uuid::from_u128(3),"call_id":call,"at_ms":1,"tool":"Bash",
        "input_summary":{"text":"operation","truncated":false},"outcome":outcome,"path":path,
        "reason":"Outside this task","policy_revision":1,"matching_rules":[],"review":null,
        "approval_source":"explicit_ask"
    })).unwrap()
}

fn completed(call: &str, error: bool) -> Event {
    event(EventKind::ToolCompleted {
        call_id: call.into(),
        result: ToolResult {
            content: "{}".into(),
            is_error: error,
            output_ref: None,
        },
        sources: Vec::new(),
    })
}

fn started(call: &str, name: &str, input: serde_json::Value) -> Event {
    event(EventKind::ToolStarted {
        call_id: call.into(),
        name: name.into(),
        input,
    })
}

fn permission(call: &str) -> Event {
    event(EventKind::PermissionRequested {
        request: PermissionRequest {
            parent_call: None,
            mcp_server: None,
            id: Uuid::from_u128(4),
            loop_id: Uuid::nil().into(),
            execution: PermissionExecution::ToolCall {
                run_id: Uuid::from_u128(1).into(),
                call_id: call.into(),
            },
            name: "Bash".into(),
            input: json!({"command":"printf ok"}),
            file_change: None,
            approval_reason: Some("The scope needs your confirmation".into()),
            approval_source: PermissionApprovalSource::AutoReviewer,
        },
    })
}

fn present(tools: &mut ToolPresentation, console: &Console, event: &Event) {
    tools
        .present_event(console, event, &CallFacts::default())
        .unwrap();
}

#[test]
fn ordinary_tool_rows_show_each_final_approval_and_direct_stays_unlabelled() {
    for (path, outcome, label) in [
        (
            ApprovalPath::AutoJev,
            ApprovalResult::Allow,
            "Allowed by Auto review (Jev)",
        ),
        (
            ApprovalPath::AutoLlm,
            ApprovalResult::Allow,
            "Allowed by Auto review (LLM)",
        ),
        (
            ApprovalPath::Human,
            ApprovalResult::Allow,
            "Approved by you",
        ),
        (
            ApprovalPath::AutoJev,
            ApprovalResult::Deny,
            "Denied by Auto review (Jev): Outside this task",
        ),
        (
            ApprovalPath::AutoLlm,
            ApprovalResult::Deny,
            "Denied by Auto review (LLM): Outside this task",
        ),
        (ApprovalPath::Human, ApprovalResult::Deny, "Denied by you"),
        (ApprovalPath::Direct, ApprovalResult::Allow, ""),
    ] {
        let console = Console::test_interactive_for_chat(Uuid::nil());
        let mut tools = ToolPresentation::new(false);
        present(
            &mut tools,
            &console,
            &started("call", "Bash", json!({"command":"printf ok"})),
        );
        let mut approval = record("call", path, outcome);
        approval.human_reason =
            (path == ApprovalPath::Human).then(|| "Ask rule requires confirmation".into());
        present(
            &mut tools,
            &console,
            &event(EventKind::ApprovalRecorded {
                record: Box::new(approval),
            }),
        );
        present(
            &mut tools,
            &console,
            &completed("call", outcome == ApprovalResult::Deny),
        );
        let text = console.test_conversation_text();
        assert!(text.contains("Bash"), "{text}");
        if label.is_empty() {
            assert!(
                !text.contains("approved")
                    && !text.contains("Approved")
                    && !text.contains("Allowed by"),
                "{text}"
            );
        } else {
            assert_eq!(text.matches(label).count(), 1, "{text}");
            if outcome == ApprovalResult::Allow {
                assert!(
                    text.lines()
                        .any(|line| line.trim() == format!("⎿  {label}")),
                    "{text}"
                );
                assert!(!text.contains(&format!(" · {label}")), "{text}");
            }
        }
        if path == ApprovalPath::Human {
            assert_eq!(
                text.matches("Ask rule requires confirmation").count(),
                1,
                "{text}"
            );
        }
        assert!(!text.contains("Running…"), "{text}");
    }
}

#[test]
fn actual_reviewer_and_human_wait_replace_the_same_result_rows() {
    let console = Console::test_interactive_for_chat(Uuid::nil());
    let mut tools = ToolPresentation::new(false);
    present(
        &mut tools,
        &console,
        &started("call", "Bash", json!({"command":"printf ok"})),
    );
    for reviewer in [ApprovalReviewerKind::Jev, ApprovalReviewerKind::Llm] {
        present(
            &mut tools,
            &console,
            &event(EventKind::ApprovalReviewStarted {
                call_id: "call".into(),
                reviewer,
            }),
        );
        let text = console.test_conversation_text();
        assert_eq!(text.matches("Auto review (").count(), 1, "{text}");
        assert!(
            text.contains(&format!("Auto review ({})…", reviewer_name(reviewer))),
            "{text}"
        );
    }
    present(&mut tools, &console, &permission("call"));
    let text = console.test_conversation_text();
    assert!(text.contains("Waiting for your approval…"), "{text}");
    assert!(
        text.contains("Auto review (LLM) asked you: The scope needs your confirmation"),
        "{text}"
    );
    assert!(
        !text.contains("Auto review (Jev)…") && !text.contains("Auto review (LLM)…"),
        "{text}"
    );
}

fn snapshot() -> ChatSnapshot {
    serde_json::from_value(json!({"id":Uuid::nil(),"folder":"/fixture","messages":[],"chat_loop":{"id":Uuid::nil(),"state":"Idle"}})).unwrap()
}

#[test]
fn retained_approval_records_restore_ordinary_tool_labels_and_human_reason() {
    for (path, label) in [
        (ApprovalPath::AutoJev, "Allowed by Auto review (Jev)"),
        (ApprovalPath::AutoLlm, "Allowed by Auto review (LLM)"),
        (ApprovalPath::Human, "Approved by you"),
        (ApprovalPath::Direct, ""),
    ] {
        let mut snapshot = snapshot();
        snapshot.messages=serde_json::from_value(json!([
            {"role":"assistant","content":[{"type":"tool_use","id":"call","name":"Bash","input":{"command":"printf ok"}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"call","content":"{}","is_error":false}]}
        ])).unwrap();
        snapshot.message_runs = [
            (0, Uuid::from_u128(1).into()),
            (1, Uuid::from_u128(1).into()),
        ]
        .into();
        let mut approval = record("call", path, ApprovalResult::Allow);
        approval.human_reason =
            (path == ApprovalPath::Human).then(|| "Ask rule requires confirmation".into());
        snapshot.approval_records = vec![approval];
        let restored: ChatSnapshot =
            serde_json::from_value(serde_json::to_value(&snapshot).unwrap()).unwrap();
        for text in [
            workflows::public_history(&restored, DisplayProfile::default()),
            presentation::transcript_document(
                &restored,
                DisplayProfile::default(),
                &ImageLinks::default(),
                &AgentColors::default(),
            )
            .plain_text(150),
        ] {
            assert!(text.contains(label), "{text}");
            if path == ApprovalPath::Human {
                assert!(text.contains("Ask rule requires confirmation"), "{text}");
            }
            if path == ApprovalPath::Direct {
                assert!(
                    !text.contains("approved")
                        && !text.contains("Approved")
                        && !text.contains("Allowed by"),
                    "{text}"
                );
            }
        }
    }
}

fn nested(call: &str, result: Option<ToolResult>) -> OperationRecord {
    serde_json::from_value(json!({
        "id":Uuid::new_v4(),"loop_id":Uuid::nil(),"run_id":Uuid::from_u128(1),"call_id":call,
        "name":"mcp__fixture__write","input":{},"parent_call_id":"batch","state":if result.is_some() {"Completed"} else {"Proposed"},"result":result
    })).unwrap()
}

#[test]
fn nested_calls_label_each_child_and_update_parent_wait_and_summary() {
    let console = Console::test_interactive_for_chat(Uuid::nil());
    let mut tools = ToolPresentation::new(false);
    let input = json!({"calls":[{"name":"mcp__fixture__write","arguments":{}},{"name":"mcp__fixture__write","arguments":{}},{"name":"mcp__fixture__write","arguments":{}}]});
    present(
        &mut tools,
        &console,
        &started("batch", "ExecuteTools", input.clone()),
    );
    for (index, path, outcome) in [
        (0, ApprovalPath::AutoJev, ApprovalResult::Allow),
        (1, ApprovalPath::Human, ApprovalResult::Deny),
        (2, ApprovalPath::Direct, ApprovalResult::Allow),
    ] {
        let call = format!("nested-{index}");
        let start = started(&call, "mcp__fixture__write", json!({}));
        tools.learn_nested_call(&start, &nested(&call, None));
        present(&mut tools, &console, &start);
        if index == 1 {
            present(&mut tools, &console, &permission(&call));
            let text = console.test_conversation_text();
            assert!(
                text.contains("Waiting for your approval: call 2 of 3"),
                "{text}"
            );
        }
        present(
            &mut tools,
            &console,
            &event(EventKind::ApprovalRecorded {
                record: Box::new(record(&call, path, outcome)),
            }),
        );
        present(
            &mut tools,
            &console,
            &completed(&call, outcome == ApprovalResult::Deny),
        );
    }
    present(&mut tools, &console, &completed("batch", false));
    let text = console.test_conversation_text();
    assert_eq!(
        text.matches("Allowed by Auto review (Jev)").count(),
        1,
        "{text}"
    );
    assert_eq!(text.matches("Denied by you").count(), 1, "{text}");
    assert_eq!(
        text.matches("2 completed, 1 denied · 1 auto-approved")
            .count(),
        1,
        "{text}"
    );
    assert!(!text.contains("Waiting for your approval"), "{text}");
    assert!(
        text.find("Denied by you").unwrap() < text.find("2 completed").unwrap(),
        "{text}"
    );
}

#[test]
fn collapsed_read_opens_its_own_row_when_approval_is_needed() {
    let console = Console::test_interactive_for_chat(Uuid::nil());
    let mut tools = ToolPresentation::new(false);
    let start = started("read", "Read", json!({"file_path":"/fixture/a"}));
    present(&mut tools, &console, &start);
    present(&mut tools, &console, &permission("read"));
    let text = console.test_conversation_text();
    assert!(text.contains("Waiting for your approval…"), "{text}");
    assert!(text.contains("Read"), "{text}");
}

#[test]
fn denial_and_human_reason_are_single_clipped_lines_with_full_details_retained() {
    let console = Console::test_interactive_for_chat(Uuid::nil());
    let mut tools = ToolPresentation::new(false);
    present(
        &mut tools,
        &console,
        &started("call", "Bash", json!({"command":"printf ok"})),
    );
    let mut approval = record("call", ApprovalPath::AutoLlm, ApprovalResult::Deny);
    approval.reason = format!("{}\n\x1bextra", "确认范围".repeat(100));
    present(
        &mut tools,
        &console,
        &event(EventKind::ApprovalRecorded {
            record: Box::new(approval),
        }),
    );
    present(&mut tools, &console, &completed("call", true));
    let text = console.test_conversation_text();
    assert_eq!(
        text.lines()
            .filter(|line| line.contains("Denied by Auto review"))
            .count(),
        1,
        "{text}"
    );
    assert!(text.contains('…'), "{text}");
    assert!(!text.contains("extra"), "{text}");
    assert!(!text.contains('\x1b'), "{text}");
}

#[test]
fn waiting_batch_position_uses_original_index_when_events_arrive_out_of_order() {
    let console = Console::test_interactive_for_chat(Uuid::nil());
    let mut tools = ToolPresentation::new(false);
    present(
        &mut tools,
        &console,
        &started("batch", "ExecuteTools", json!({"calls":[{}, {}, {}]})),
    );
    let start = started("third", "mcp__fixture__write", json!({}));
    let mut operation = nested("third", None);
    operation.parent_call_index = Some(2);
    tools.learn_nested_call(&start, &operation);
    present(&mut tools, &console, &start);
    present(&mut tools, &console, &permission("third"));
    assert!(
        console
            .test_conversation_text()
            .contains("Waiting for your approval: call 3 of 3")
    );
}

#[test]
fn retained_records_label_nested_history_and_keep_one_parent_summary() {
    let mut snapshot = snapshot();
    snapshot.messages=serde_json::from_value(json!([
        {"role":"assistant","content":[{"type":"tool_use","id":"batch","name":"ExecuteTools","input":{"calls":[{},{}]}}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"batch","content":"{}","is_error":false}]}
    ])).unwrap();
    snapshot.message_runs = [
        (0, Uuid::from_u128(1).into()),
        (1, Uuid::from_u128(1).into()),
    ]
    .into();
    snapshot.operations = vec![
        nested(
            "auto",
            Some(ToolResult {
                content: "{}".into(),
                is_error: false,
                output_ref: None,
            }),
        ),
        nested(
            "human",
            Some(ToolResult {
                content: "denied".into(),
                is_error: true,
                output_ref: None,
            }),
        ),
    ];
    snapshot.approval_records = vec![
        record("auto", ApprovalPath::AutoJev, ApprovalResult::Allow),
        record("human", ApprovalPath::Human, ApprovalResult::Deny),
    ];
    for text in [
        workflows::public_history(&snapshot, DisplayProfile::default()),
        presentation::transcript_document(
            &snapshot,
            DisplayProfile::default(),
            &ImageLinks::default(),
            &AgentColors::default(),
        )
        .plain_text(150),
    ] {
        assert_eq!(
            text.matches("Allowed by Auto review (Jev)").count(),
            1,
            "{text}"
        );
        assert_eq!(text.matches("Denied by you").count(), 1, "{text}");
        assert_eq!(
            text.matches("1 completed, 1 denied · 1 auto-approved")
                .count(),
            1,
            "{text}"
        );
    }
}

#[test]
fn final_batch_summary_preserves_not_executed_and_unknown_outcomes() {
    let text = presentation::settled_batch_summary(
        &json!({"results":[
            {"call_id":"a","status":"completed"},
            {"call_id":"b","status":"denied"},
            {"call_id":"c","status":"not_executed"},
            {"call_id":"d","status":"unknown"},
            {"call_id":"e","status":"error"}
        ]})
        .to_string(),
        &ToolApprovals::default(),
        Uuid::from_u128(1),
    )
    .unwrap();
    assert_eq!(
        text,
        "1 completed, 1 denied, 1 failed, 1 not executed, 1 unknown"
    );
}

#[test]
fn noninteractive_permissions_still_reach_the_original_prompt_handler() {
    let console = Console::test_for_chat(Uuid::nil());
    for name in ["Bash", "ExitPlanMode"] {
        let mut tools = ToolPresentation::new(true);
        let mut request = permission("call");
        if let EventKind::PermissionRequested { request } = &mut request.kind {
            request.name = name.into();
        }
        assert!(
            !tools
                .present_event(&console, &request, &CallFacts::default())
                .unwrap(),
            "{name} must retain its original permission/workflow handler"
        );
    }
}

#[test]
fn live_permissions_restore_waiting_rows_after_their_events_are_evicted() {
    let mut snapshot = snapshot();
    snapshot.messages = serde_json::from_value(json!([
        {"role":"assistant","content":[{"type":"tool_use","id":"call","name":"Bash","input":{"command":"printf ok"}}]}
    ])).unwrap();
    snapshot.message_runs = [(0, Uuid::from_u128(1).into())].into();
    let EventKind::PermissionRequested { request } = permission("call").kind else {
        unreachable!()
    };
    snapshot.permissions.push(request);
    assert!(snapshot.events.is_empty());
    let console = Console::test_interactive_for_chat(Uuid::nil());
    workflows::write_public_history(&console, &snapshot).unwrap();
    let text = console.test_conversation_text();
    assert!(text.contains("Waiting for your approval…"), "{text}");
    assert!(
        text.contains("Auto review asked you: The scope needs your confirmation"),
        "{text}"
    );
    assert!(
        !text.contains("Auto review (Jev)") && !text.contains("Auto review (LLM)"),
        "{text}"
    );
}

#[test]
fn terminating_a_run_replaces_waiting_child_and_parent_without_a_tool_result() {
    for human in [false, true] {
        for ending in [
            EventKind::Interrupted,
            EventKind::Failed("lost execution".into()),
        ] {
            let console = Console::test_interactive_for_chat(Uuid::nil());
            let mut tools = ToolPresentation::new(false);
            present(
                &mut tools,
                &console,
                &started("batch", "ExecuteTools", json!({"calls":[{}]})),
            );
            let start = started("call", "mcp__fixture__write", json!({}));
            tools.learn_nested_call(&start, &nested("call", None));
            present(&mut tools, &console, &start);
            present(
                &mut tools,
                &console,
                &event(EventKind::ApprovalReviewStarted {
                    call_id: "call".into(),
                    reviewer: ApprovalReviewerKind::Llm,
                }),
            );
            if human {
                present(&mut tools, &console, &permission("call"));
            }
            let label = if matches!(ending, EventKind::Interrupted) {
                "Interrupted"
            } else {
                "Failed"
            };
            present(&mut tools, &console, &event(ending));
            let text = console.test_conversation_text();
            assert!(
                !text.contains("Waiting for your approval") && !text.contains("Auto review (LLM)…"),
                "{text}"
            );
            assert_eq!(text.matches(label).count(), 2, "{text}");
            if human {
                assert!(text.contains("The scope needs your confirmation"), "{text}");
            }
        }
    }
}

#[test]
fn detailed_failed_results_do_not_claim_completion_or_add_direct_labels() {
    for path in [ApprovalPath::AutoLlm, ApprovalPath::Direct] {
        let mut tools = ToolPresentation::new(true);
        tools.record(&started("call", "Bash", json!({"command":"false"})));
        tools.record(&event(EventKind::ApprovalRecorded {
            record: Box::new(record("call", path, ApprovalResult::Allow)),
        }));
        let text = tools.render_event(&completed("call", true)).unwrap();
        assert!(text.contains("Failed"), "{text}");
        assert!(!text.contains("Completed"), "{text}");
        assert_eq!(
            text.contains("Allowed by Auto review"),
            path == ApprovalPath::AutoLlm,
            "{text}"
        );
        if path == ApprovalPath::Direct {
            assert!(!text.contains('↳'), "{text}");
        }
    }
}

#[test]
fn technical_unavailability_and_admission_failure_do_not_blame_the_reviewer() {
    for (category, path, label) in [
        (
            DenialCategory::NoHuman,
            ApprovalPath::AutoLlm,
            "Approval unavailable:",
        ),
        (
            DenialCategory::FinalAdmission,
            ApprovalPath::Human,
            "Not executed:",
        ),
    ] {
        let mut record = record("call", path, ApprovalResult::Deny);
        record.denial_category = Some(category);
        let status = ApprovalStatus::Settled(Box::new(record));
        let text = presentation::joined(&status.decorate(result_runs("Failed", "⎿"), "⎿"));
        assert!(text.contains(label), "{text}");
        assert!(!text.contains("Denied by"), "{text}");
    }
}

#[test]
fn ordinary_output_with_an_approval_like_prefix_keeps_wrapping() {
    let console = Console::test_interactive_for_chat(Uuid::nil());
    let mut tools = ToolPresentation::new(false);
    present(
        &mut tools,
        &console,
        &started("call", "Bash", json!({"command":"printf log"})),
    );
    present(
        &mut tools,
        &console,
        &event(EventKind::ApprovalRecorded {
            record: Box::new(record("call", ApprovalPath::Direct, ApprovalResult::Allow)),
        }),
    );
    let result = event(EventKind::ToolCompleted {
        call_id: "call".into(),
        result: ToolResult {
            content: format!("Safety check {} END-OF-LOG", "x".repeat(145)),
            is_error: false,
            output_ref: None,
        },
        sources: Vec::new(),
    });
    present(&mut tools, &console, &result);
    let text = console.test_conversation_text();
    assert!(text.contains("END-OF-LOG"), "{text}");
}

#[test]
fn a_final_denial_marks_the_header_failed_without_waiting_for_tool_completed() {
    let console = Console::test_interactive_for_chat(Uuid::nil());
    let mut tools = ToolPresentation::new(false);
    present(
        &mut tools,
        &console,
        &started("call", "Bash", json!({"command":"printf ok"})),
    );
    let mut denial = record("call", ApprovalPath::Human, ApprovalResult::Deny);
    denial.denial_category = Some(DenialCategory::FinalAdmission);
    present(
        &mut tools,
        &console,
        &event(EventKind::ApprovalRecorded {
            record: Box::new(denial),
        }),
    );
    present(
        &mut tools,
        &console,
        &event(EventKind::Failed("final admission refused".into())),
    );
    let rows = console.test_conversation_rows();
    let header = rows.iter().find(|row| row.text().contains("Bash")).unwrap();
    assert!(
        header
            .spans
            .iter()
            .any(|span| span.text.contains('⏺') && span.role == Some(Role::Error)),
        "{header:?}"
    );
    assert!(
        !header
            .spans
            .iter()
            .any(|span| span.role == Some(Role::Success)),
        "{header:?}"
    );
}

#[test]
fn reviewing_and_allowed_calls_remain_queued_until_tool_started() {
    let console = Console::test_interactive_for_chat(Uuid::nil());
    let mut tools = ToolPresentation::new(false);
    present(
        &mut tools,
        &console,
        &event(EventKind::AssistantMessageCommitted {
            message_id: Uuid::new_v4(),
            tool_calls: vec![tickloop_harness::protocol::ProposedToolCall {
                call_id: "call".into(),
                name: "Bash".into(),
                input: json!({"command":"printf ok"}),
            }],
        }),
    );
    for kind in [
        EventKind::ApprovalReviewStarted {
            call_id: "call".into(),
            reviewer: ApprovalReviewerKind::Llm,
        },
        permission("call").kind,
        EventKind::ApprovalRecorded {
            record: Box::new(record("call", ApprovalPath::Human, ApprovalResult::Allow)),
        },
    ] {
        present(&mut tools, &console, &event(kind));
        let text = console.test_conversation_text();
        assert!(text.contains("Queued"), "{text}");
    }
    present(
        &mut tools,
        &console,
        &started("call", "Bash", json!({"command":"printf ok"})),
    );
    assert!(!console.test_conversation_text().contains("Queued"));
}
