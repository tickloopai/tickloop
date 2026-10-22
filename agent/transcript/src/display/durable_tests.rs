use super::*;
use crate::{AbandonReason, InputState, ModelVisibleResult, ResumePoint, TaskBoardsEntry};
use serde_json::json;
use tickloop_agent_protocol::{Message, QuestionRequest, ToolResult};

fn entry(role: Role, content: Vec<Content>) -> ChainEntry {
    serde_json::from_value(json!({
        "uuid": Uuid::new_v4(), "chatId": Uuid::nil(), "timestamp": "2026-10-20T00:00:00Z",
        "version": "test", "message": Message { role, content }, "humanFamilyOrigin": null,
    }))
    .unwrap()
}

fn response(state: ResponseState) -> ResponseEntry {
    ResponseEntry {
        response_id: Uuid::new_v4(),
        input_id: Some(Uuid::new_v4()),
        attempt: 1,
        run_id: Uuid::new_v4(),
        loop_id: Uuid::new_v4(),
        display_parent: None,
        timestamp: "2026-10-20T00:00:00Z".into(),
        state,
        without_messages: false,
    }
}

fn call(response: &ResponseEntry, name: &str, input: serde_json::Value) -> Line {
    let mut entry = entry(
        Role::Assistant,
        vec![Content::ToolUse {
            id: "call".into(),
            name: name.into(),
            input,
        }],
    );
    entry.response_id = Some(response.response_id);
    entry.run_id = Some(response.run_id);
    Line::Assistant(entry)
}

fn wait(response: &ResponseEntry) -> WaitEntry {
    let id = Uuid::new_v4();
    WaitEntry {
        wait_id: id,
        chat_id: Uuid::nil(),
        loop_id: response.loop_id,
        run_id: response.run_id,
        response_id: response.response_id,
        kind: WaitKind::Question {
            request: QuestionRequest {
                id,
                chat_id: Uuid::nil(),
                loop_id: response.loop_id,
                run_id: response.run_id,
                call_id: "call".into(),
                questions: vec![Question {
                    question: "Which?".into(),
                    header: "Choice".into(),
                    options: vec![],
                    multi_select: false,
                }],
            },
            operation_id: Uuid::new_v4(),
        },
        resume: ResumePoint::InBatch {
            assistant_message: Uuid::new_v4(),
            call_id: "call".into(),
        },
        timestamp: response.timestamp.clone(),
        state: WaitState::Open,
    }
}

fn result(text: &str, is_error: bool) -> ModelVisibleResult {
    ModelVisibleResult {
        tool: ToolResult {
            content: text.into(),
            is_error,
            output_ref: None,
        },
        content: text.into(),
        ends_run: is_error,
    }
}

#[test]
fn question_wait_updates_the_same_answer_and_call_position_through_completion() {
    let mut response = response(ResponseState::Suspended);
    let mut wait = wait(&response);
    let mut projector = DisplayProjector::new();
    projector.apply(&call(&response, "AskUserQuestion", json!({})));
    projector.apply(&Line::Response(response.clone()));
    let changes = projector.apply(&Line::Wait(wait.clone()));
    let node = projector.node(response.response_id).unwrap();
    assert_eq!(changes, vec![DisplayChange::Upsert(node.clone())]);
    assert_eq!(
        node.status,
        DisplayStatus::Waiting {
            r#for: DisplayWait::Person {
                request: wait.wait_id
            }
        }
    );
    assert!(
        matches!(&node.blocks[..], [DisplayBlock::ToolCall { .. }, DisplayBlock::Question { request_id, reply: None, questions }] if *request_id == wait.wait_id && questions.len() == 1)
    );

    wait.state = WaitState::Resolved {
        result: result("Selected A", false),
        by_input: Some(Uuid::new_v4()),
    };
    projector.apply(&Line::Wait(wait.clone()));
    let node = projector.node(response.response_id).unwrap();
    assert_eq!(node.status, DisplayStatus::Streaming);
    assert!(
        matches!(&node.blocks[1], DisplayBlock::Question { reply: Some(reply), .. } if reply.text == "Selected A")
    );

    let mut returned = entry(
        Role::User,
        vec![Content::ToolResult {
            tool_use_id: "call".into(),
            content: "Selected A".into(),
            is_error: false,
        }],
    );
    returned.response_id = Some(response.response_id);
    projector.apply(&Line::User(returned));
    wait.state = WaitState::Appended;
    projector.apply(&Line::Wait(wait.clone()));
    response.state = ResponseState::Resumed;
    projector.apply(&Line::Response(response.clone()));
    assert_eq!(
        projector.node(response.response_id).unwrap().status,
        DisplayStatus::Streaming
    );
    response.state = ResponseState::Ended {
        ending: ResponseEnding::Completed,
    };
    projector.apply(&Line::Response(response.clone()));
    let nodes = projector.nodes().collect::<Vec<_>>();
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].id, response.response_id);
    assert_eq!(nodes[0].status, DisplayStatus::Complete);
    assert!(
        matches!(&nodes[0].blocks[1], DisplayBlock::Question { reply: Some(reply), .. } if reply.text == "Selected A")
    );
    assert_eq!(
        serde_json::from_str::<DisplayNode>(&serde_json::to_string(nodes[0]).unwrap()).unwrap(),
        *nodes[0]
    );
}

#[test]
fn resolved_declined_and_abandoned_questions_are_not_waiting() {
    for state in [
        WaitState::Resolved {
            result: result("Declined", true),
            by_input: None,
        },
        WaitState::Appended,
        WaitState::Abandoned {
            reason: AbandonReason::Interrupted,
        },
        WaitState::Abandoned {
            reason: AbandonReason::HostLost,
        },
    ] {
        let response = response(ResponseState::Suspended);
        let mut wait = wait(&response);
        wait.state = state;
        let nodes = project(&[
            Line::Wait(wait.clone()),
            call(&response, "AskUserQuestion", json!({})),
            Line::Response(response),
        ]);
        assert_eq!(nodes[0].status, DisplayStatus::Streaming);
        if matches!(wait.state, WaitState::Resolved { .. }) {
            assert!(
                matches!(&nodes[0].blocks[1], DisplayBlock::Question { reply: Some(reply), .. } if reply.is_error)
            );
        }
    }
}

#[test]
fn process_cleanup_and_handover_override_stale_open_waits() {
    for (ending, expected) in [
        (
            ResponseEnding::Interrupted {
                by: InterruptedBy::Host,
            },
            DisplayStatus::Interrupted {
                by: InterruptedBy::Host,
            },
        ),
        (
            ResponseEnding::Interrupted {
                by: InterruptedBy::User,
            },
            DisplayStatus::Interrupted {
                by: InterruptedBy::User,
            },
        ),
        (
            ResponseEnding::Interrupted {
                by: InterruptedBy::Handover,
            },
            DisplayStatus::Streaming,
        ),
        (
            ResponseEnding::Failed {
                error: "failed".into(),
            },
            DisplayStatus::Failed {
                error: "failed".into(),
            },
        ),
    ] {
        let response = response(ResponseState::Ended { ending });
        let wait = wait(&response);
        let nodes = project(&[Line::Wait(wait), Line::Response(response)]);
        assert_eq!(nodes[0].status, expected);
    }
}

#[test]
fn foreground_and_children_waits_show_the_member_and_resume() {
    let child = Uuid::new_v4();
    for kind in [
        WaitKind::Subagent {
            child_chat: child,
            child_loop: Uuid::new_v4(),
            child_run: Uuid::new_v4(),
            operation_id: Uuid::new_v4(),
        },
        WaitKind::Children {
            children: vec![child, Uuid::new_v4()],
        },
    ] {
        let mut response = response(ResponseState::Suspended);
        let mut wait = wait(&response);
        wait.kind = kind;
        let mut projector = DisplayProjector::new();
        projector.apply(&Line::Wait(wait.clone()));
        projector.apply(&Line::Response(response.clone()));
        assert_eq!(
            projector.node(response.response_id).unwrap().status,
            DisplayStatus::Waiting {
                r#for: DisplayWait::Member { chat: child }
            }
        );
        wait.state = WaitState::Appended;
        projector.apply(&Line::Wait(wait));
        response.state = ResponseState::Resumed;
        projector.apply(&Line::Response(response.clone()));
        assert_eq!(
            projector.node(response.response_id).unwrap().status,
            DisplayStatus::Streaming
        );
    }
}

#[test]
fn subagent_cards_link_foreground_and_background_members_when_registration_arrives() {
    for background in [false, true] {
        let response = response(ResponseState::Suspended);
        let child = Uuid::new_v4();
        let registration = Line::Subagents(crate::SubagentsEntry {
            chat_id: Uuid::nil(),
            subagents: vec![SubagentRegistration {
                child_chat_id: child,
                handle: "research".into(),
                description: "Research".into(),
                created_at: 0,
                color: Some(AgentColor::Blue),
            }],
        });
        let mut lines = vec![call(
            &response,
            "Subagent",
            json!({"name": "research", "run_in_background": background}),
        )];
        if background {
            let mut returned = entry(
                Role::User,
                vec![Content::ToolResult {
                    tool_use_id: "call".into(),
                    content: json!({"child_chat_id": child}).to_string().into(),
                    is_error: false,
                }],
            );
            returned.response_id = Some(response.response_id);
            lines.push(Line::User(returned));
        } else {
            let mut wait = wait(&response);
            wait.kind = WaitKind::Subagent {
                child_chat: child,
                child_loop: Uuid::new_v4(),
                child_run: Uuid::new_v4(),
                operation_id: Uuid::new_v4(),
            };
            lines.push(Line::Wait(wait));
        }
        let mut projector = DisplayProjector::new();
        for line in &lines {
            projector.apply(line);
        }
        let changes = projector.apply(&registration);
        assert_eq!(changes.len(), 1);
        assert!(
            matches!(&projector.node(response.response_id).unwrap().blocks[0], DisplayBlock::ToolCall { member: Some(member), .. }
            if member.chat == child && member.handle == "research" && member.color == Some(AgentColor::Blue))
        );
        lines.push(registration);
        assert_eq!(
            project(&lines),
            projector.nodes().cloned().collect::<Vec<_>>()
        );
    }
}

fn input(response: &ResponseEntry, op: InputOp) -> Line {
    Line::Input(InputEntry {
        input_id: response.input_id.unwrap(),
        op,
        digest: String::new(),
        display_parent: None,
        loop_id: response.loop_id,
        run_id: response.run_id,
        timestamp: response.timestamp.clone(),
        state: InputState::Started,
        queued: None,
    })
}

#[test]
fn all_prompt_origins_survive_metadata_order_and_retained_replay() {
    let sender = Uuid::new_v4();
    for (op, source, text) in [
        (InputOp::Continue, None, "user"),
        (InputOp::Trigger, None, "scheduled prompt"),
        (InputOp::Goal, None, "goal"),
        (
            InputOp::Feedback,
            None,
            "@tickloop feedback request on artifact doc. Delivery one. Input JSON:\n{}",
        ),
        (InputOp::AgentMessage, Some(("send", false)), "message"),
        (
            InputOp::AgentMessage,
            Some(("subagent-completion", true)),
            "notification",
        ),
    ] {
        let mut response = response(ResponseState::Started);
        let mut prompt = entry(Role::User, vec![Content::Text { text: text.into() }]);
        prompt.run_id = Some(response.run_id);
        if matches!(op, InputOp::Continue | InputOp::Goal) {
            prompt.execution_mode = Some(json!("default"));
        }
        if let Some((call_id, is_result)) = source {
            prompt.agent_message_origin = Some(json!({
                "message_id": prompt.uuid, "call_id": call_id, "created_at": 0,
                "sender_chat_id": sender, "sender_loop_id": sender, "sender_run_id": sender,
                "sender_relationship": "child", "sender_label": "Research", "sender_handle": "research",
                "relay": null, "cause": {"chain_id": sender, "depth": 1, "reply_to": null}, "is_result": is_result,
            }));
        }
        let expected = match op {
            InputOp::Trigger => PromptOrigin::Trigger {
                r#loop: response.loop_id,
            },
            InputOp::Goal => PromptOrigin::Goal {
                r#loop: response.loop_id,
            },
            InputOp::Feedback => PromptOrigin::Feedback {
                artifact: "doc".into(),
            },
            InputOp::AgentMessage if source.unwrap().1 => {
                PromptOrigin::SubagentNotification { child: sender }
            }
            InputOp::AgentMessage => PromptOrigin::AgentMessage { from: sender },
            _ => PromptOrigin::User,
        };
        let prompt_id = prompt.uuid;
        if op == InputOp::Continue {
            response.display_parent = Some(prompt_id);
        }
        let before = project(&[
            input(&response, op),
            Line::User(prompt.clone()),
            Line::Response(response.clone()),
        ]);
        let after = project(&[
            Line::User(prompt.clone()),
            Line::Response(response.clone()),
            input(&response, op),
        ]);
        assert_eq!(before, after, "{op:?}");
        assert_eq!(
            after[0].kind,
            DisplayKind::Prompt {
                input_id: None,
                origin: expected
            }
        );
        assert_eq!(after[1].parent, Some(prompt_id));
        let snapshot = Line::RetainedSnapshot(crate::RetainedSnapshotEntry {
            chat_id: Uuid::nil(),
            lines: vec![Line::Response(response.clone()), input(&response, op)],
        });
        assert_eq!(project(&[snapshot, Line::User(prompt)]), before);
    }
}

#[test]
fn loop_projection_uses_the_latest_record() {
    let id = Uuid::new_v4();
    let trigger = Trigger::for_loop_goal("watched");
    let mut record: LoopRecord = serde_json::from_value(json!({
        "loopId": id, "goal": "watched", "state": "WaitingTrigger", "trigger": trigger,
        "stateSince": 2000, "runStartedAt": null,
    }))
    .unwrap();
    let line = |record: &LoopRecord| {
        let mut entry = entry(Role::System, vec![]);
        entry.loop_record = Some(record.clone());
        Line::System(entry)
    };
    let first = line(&record);
    record.goal = "updated".into();
    let latest = line(&record);
    let snapshot = Line::RetainedSnapshot(crate::RetainedSnapshotEntry {
        chat_id: Uuid::nil(),
        lines: vec![first],
    });
    let projected = loops(&[snapshot, latest]);
    assert_eq!(projected.len(), 1);
    assert_eq!(projected[0].goal, "updated");
    assert_eq!(projected[0].trigger.as_ref().unwrap().prompt, "watched");
    assert_eq!(projected[0].state_since, Some(2000));
}

#[test]
fn task_projection_obeys_host_switch_latest_boards_and_clear() {
    let chat = Uuid::new_v4();
    let goal = Uuid::new_v4();
    let mut board = TaskBoard::new(chat, goal);
    board.task_number_offset = 10;
    board.tasks = serde_json::from_value(json!([{
        "id": Uuid::new_v4(), "title": "Research", "description": "Check", "dependencies": [],
        "status": "needs_attention", "owner": {"chat_id": chat, "name": "main"}, "claim": null,
        "revision": 2, "result": "host lost",
    }]))
    .unwrap();
    let line = |board: TaskBoard| {
        Line::TaskBoards(TaskBoardsEntry {
            chat_id: chat,
            boards: [(goal, board)].into(),
        })
    };
    let first = line(board.clone());
    board.revision = 2;
    board.tasks[0].status = tickloop_agent_protocol::SharedTaskStatus::Completed;
    let lines = [
        first,
        Line::RetainedSnapshot(crate::RetainedSnapshotEntry {
            chat_id: chat,
            lines: vec![line(board.clone())],
        }),
    ];
    assert!(task_boards(&lines, false).is_empty());
    let shown = task_boards(&lines, true);
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0].revision, 2);
    assert_eq!(shown[0].task_number_offset, 10);
    assert_eq!(shown[0].tasks, board.tasks);
    assert!(!serde_json::to_string(&shown).unwrap().contains("mutations"));
    board.tasks.clear();
    assert!(task_boards(&[lines[0].clone(), line(board)], true).is_empty());
    assert!(
        task_boards(
            &[
                lines[0].clone(),
                Line::TaskBoards(TaskBoardsEntry {
                    chat_id: chat,
                    boards: Default::default()
                })
            ],
            true
        )
        .is_empty()
    );
}

#[test]
fn human_prompt_parents_and_followups_keep_the_recorded_lifecycle() {
    let mut response = response(ResponseState::Started);
    let mut first = entry(
        Role::User,
        vec![Content::Text {
            text: "first".into(),
        }],
    );
    first.execution_mode = Some(json!("default"));
    first.run_id = Some(response.run_id);
    let first_id = first.uuid;
    let mut second = entry(
        Role::User,
        vec![Content::Text {
            text: "second".into(),
        }],
    );
    second.parent_uuid = Some(first_id);
    second.execution_mode = Some(json!("default"));
    response.display_parent = Some(second.uuid);
    let nodes = project(&[
        Line::User(first),
        Line::User(second),
        Line::Response(response.clone()),
    ]);
    assert_eq!(nodes[2].parent, response.display_parent);

    let mut follow_up = entry(
        Role::User,
        vec![Content::Text {
            text: "follow up".into(),
        }],
    );
    follow_up.execution_mode = Some(json!("default"));
    follow_up.run_id = Some(response.run_id);
    follow_up.response_id = Some(response.response_id);
    let follow_up_id = follow_up.uuid;
    let nodes = project(&[
        Line::Response(response.clone()),
        Line::User(follow_up),
        input(&response, InputOp::Continue),
    ]);
    assert_eq!(nodes[0].blocks.len(), 1);
    assert!(
        matches!(&nodes[0].blocks[0], DisplayBlock::FollowUp { prompt_id, .. } if *prompt_id == follow_up_id)
    );
}
