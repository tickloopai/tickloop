use super::*;
use std::{sync::Arc, time::Duration};
use tickloop_harness_protocol::SharedTaskId;
use tickloop_harness_protocol::{CommandState, OperationState};
use tickloop_harness_test_support::{
    call, config,
    routed::{RoutedModel, has_tool_result, state},
    single_question, text,
};

pub(super) async fn family(store: Option<PathBuf>) -> (Agent, Vec<Receipt>) {
    let model = Arc::new(RoutedModel::new(|request| {
        if state(request)["delegation"].is_null() {
            if !has_tool_result(request, "a") {
                return call(
                    "a",
                    "Subagent",
                    json!({"name":"a","task":"wait","run_in_background":true}),
                );
            }
            if !has_tool_result(request, "b") {
                return call(
                    "b",
                    "Subagent",
                    json!({"name":"b","task":"wait","run_in_background":true}),
                );
            }
        }
        if !has_tool_result(request, "question") {
            return call("question", "AskUserQuestion", single_question("Wait?"));
        }
        text("done")
    }));
    let core = match store {
        Some(path) => Agent::open_embedded(path, config(), model).unwrap(),
        None => Agent::new_embedded(config(), model),
    };
    let directory = std::env::temp_dir();
    let root = core.create_chat(directory).await.unwrap();
    core.set_task_tools_enabled(root, true).await.unwrap();
    core.set_execution_mode(root, tickloop_harness_protocol::ExecutionMode::FullAccess)
        .await
        .unwrap();
    let mut events = core.subscribe();
    core.submit(root, "coordinate".into()).await.unwrap();
    let mut receipts = tokio::time::timeout(Duration::from_secs(15), async {
        let mut receipts = Vec::new();
        while receipts.len() < 3 {
            let event = events.recv().await.unwrap();
            if matches!(event.kind, EventKind::QuestionAsked { .. }) {
                receipts.push(Receipt {
                    chat_id: event.chat_id,
                    loop_id: event.loop_id,
                    run_id: event.run_id,
                });
            }
        }
        receipts
    })
    .await
    .unwrap();
    // Exercise the atomic tool handler with the same receipts while model execution is parked.
    {
        let mut chats = core.inner.chats.lock().await;
        for receipt in &receipts {
            let chat = chats.get_mut(&receipt.chat_id).unwrap();
            chat.active_run = Some(receipt.run_id);
            chat.active = Some(CancellationToken::new());
        }
    }
    receipts.sort_by_key(|receipt| receipt.chat_id != root);
    (core, receipts)
}

pub(super) fn operation(receipt: &Receipt, name: &str, input: Value) -> OperationRecord {
    OperationRecord {
        parent_call_id: None,
        parent_call_index: None,
        id: Uuid::new_v4(),
        loop_id: receipt.loop_id,
        run_id: receipt.run_id,
        call_id: Uuid::new_v4().to_string(),
        name: name.into(),
        input,
        task_binding: None,
        file_change: None,
        state: OperationState::Admitted,
        result: None,
    }
}

pub(super) async fn invoke(
    core: &Agent,
    receipt: &Receipt,
    name: &str,
    input: Value,
) -> Result<Value, String> {
    let result = core
        .shared_task_tool(
            receipt,
            &operation(receipt, name, input),
            &CancellationToken::new(),
        )
        .await?;
    serde_json::from_str(&result.content).map_err(|error| error.to_string())
}

pub(super) async fn create(core: &Agent, receipt: &Receipt, dependencies: Vec<Uuid>) -> SharedTask {
    let result = invoke(core, receipt, "TaskCreate", json!({"mutation_id":Uuid::new_v4(),"title":"Inspect","description":"Inspect authorized work","dependencies":dependencies})).await.unwrap();
    serde_json::from_value(result["task"].clone()).unwrap()
}

#[tokio::test]
async fn competing_claims_have_one_winner_and_all_views_share_the_board() {
    let (core, receipts) = family(None).await;
    let task = create(&core, &receipts[0], vec![]).await;
    let input =
        |id| json!({"mutation_id":id,"task_id":task.id,"expected_revision":1,"action":"claim"});
    let (a, b) = tokio::join!(
        invoke(&core, &receipts[1], "TaskUpdate", input(Uuid::new_v4())),
        invoke(&core, &receipts[2], "TaskUpdate", input(Uuid::new_v4()))
    );
    assert_ne!(a.is_ok(), b.is_ok());
    let conflict: Value = serde_json::from_str(a.as_ref().err().or(b.as_ref().err()).unwrap())
        .expect("the losing claimant must receive the current task");
    assert_eq!(conflict["code"], "task_revision_conflict");
    assert_eq!(conflict["applied"], false);
    assert_eq!(conflict["current_task"]["status"], "in_progress");
    assert_eq!(conflict["current_task"]["revision"], 2);
    assert!(
        conflict["recovery"]
            .as_str()
            .unwrap()
            .contains("Do not merely replace expected_revision")
    );
    let board = core
        .snapshot(receipts[0].chat_id)
        .await
        .unwrap()
        .task_board
        .unwrap();
    for receipt in &receipts {
        let snapshot = core.snapshot(receipt.chat_id).await.unwrap();
        assert_eq!(snapshot.task_board, Some(board.clone()));
        assert_eq!(
            core.status(receipt.chat_id).await.unwrap().task_board,
            snapshot.task_board
        );
        let listed = invoke(&core, receipt, "TaskList", json!({})).await.unwrap();
        assert_eq!(
            listed["board"],
            tickloop_harness_protocol::model_ids::model_json(
                &IdContext::new(),
                serde_json::to_value(&board).unwrap()
            )
        );
    }
    assert_eq!(board.tasks[0].status, Status::InProgress);
    core.shutdown().await;
}

#[tokio::test]
async fn stale_root_release_reports_completed_task_without_mutating_it() {
    let (core, receipts) = family(None).await;
    let root = &receipts[0];
    let child = &receipts[1];
    let task = create(&core, root, vec![]).await;
    invoke(&core, root, "TaskUpdate", json!({"mutation_id":Uuid::new_v4(),"task_id":task.id,"expected_revision":1,"action":"assign","owner":child.chat_id.uuid().to_string()})).await.unwrap();
    invoke(&core, child, "TaskUpdate", json!({"mutation_id":Uuid::new_v4(),"task_id":task.id,"expected_revision":2,"action":"claim"})).await.unwrap();
    let observed = invoke(&core, root, "TaskList", json!({})).await.unwrap();
    assert_eq!(observed["board"]["tasks"][0]["revision"], 3);
    assert_eq!(observed["board"]["tasks"][0]["status"], "in_progress");
    let premature_release = invoke(&core, root, "TaskUpdate", json!({"mutation_id":Uuid::new_v4(),"task_id":task.id,"expected_revision":3,"action":"release","result":"An earlier hand-back said the work was done"})).await.unwrap_err();
    assert!(premature_release.contains("release requires the claiming Run"));
    let completed = invoke(&core, child, "TaskUpdate", json!({"mutation_id":Uuid::new_v4(),"task_id":task.id,"expected_revision":3,"action":"complete","result":"Verified by the current claiming Run"})).await.unwrap();
    let before = invoke(&core, root, "TaskList", json!({})).await.unwrap();
    let stale_release = json!({"mutation_id":Uuid::new_v4(),"task_id":task.id,"expected_revision":3,"action":"release","result":"An earlier hand-back said the work was done"});
    let conflict = invoke(&core, root, "TaskUpdate", stale_release)
        .await
        .unwrap_err();
    let after = invoke(&core, root, "TaskList", json!({})).await.unwrap();
    let terminal_release = invoke(&core, root, "TaskUpdate", json!({"mutation_id":Uuid::new_v4(),"task_id":task.id,"expected_revision":4,"action":"release","result":"An earlier hand-back said the work was done"})).await.unwrap_err();
    assert!(terminal_release.contains("terminal tasks cannot be changed"));
    core.shutdown().await;
    assert_eq!(before, after);
    let conflict: Value = serde_json::from_str(&conflict)
        .expect("a revision conflict must include the current task and recovery guidance");
    assert_eq!(conflict["code"], "task_revision_conflict");
    assert_eq!(conflict["applied"], false);
    assert_eq!(conflict["expected_revision"], 3);
    assert_eq!(conflict["current_task"], completed["task"]);
    assert_eq!(conflict["current_task"]["status"], "completed");
    assert_eq!(conflict["current_task"]["revision"], 4);
    assert_eq!(
        conflict["current_task"]["claim"]["run_id"],
        IdContext::new().model_text(child.run_id)
    );
    assert_eq!(
        conflict["current_task"]["report"]["run_id"],
        IdContext::new().model_text(child.run_id)
    );
    assert!(
        conflict["recovery"]
            .as_str()
            .unwrap()
            .contains("Do not retry")
    );
}

#[tokio::test]
async fn dependencies_versions_and_one_active_task_are_enforced() {
    let (core, receipts) = family(None).await;
    let first = create(&core, &receipts[0], vec![]).await;
    let blocked = create(&core, &receipts[0], vec![first.id.into()]).await;
    let second = create(&core, &receipts[0], vec![]).await;
    let claim = |task| json!({"mutation_id":Uuid::new_v4(),"task_id":task,"expected_revision":1,"action":"claim"});
    assert!(
        invoke(&core, &receipts[1], "TaskUpdate", claim(blocked.id))
            .await
            .unwrap_err()
            .contains("dependencies")
    );
    invoke(&core, &receipts[1], "TaskUpdate", claim(first.id))
        .await
        .unwrap();
    assert!(
        invoke(&core, &receipts[1], "TaskUpdate", claim(second.id))
            .await
            .unwrap_err()
            .contains("existing active")
    );
    assert!(invoke(&core,&receipts[0],"TaskUpdate",json!({"mutation_id":Uuid::new_v4(),"task_id":first.id,"expected_revision":2,"action":"assign","owner":"b"})).await.is_err());
    invoke(&core,&receipts[1],"TaskUpdate",json!({"mutation_id":Uuid::new_v4(),"task_id":first.id,"expected_revision":2,"action":"complete","result":"Verified"})).await.unwrap();
    invoke(&core, &receipts[2], "TaskUpdate", claim(blocked.id))
        .await
        .unwrap();
    core.shutdown().await;
}

#[tokio::test]
async fn background_or_unknown_operations_prevent_release() {
    let (core, receipts) = family(None).await;
    let task = create(&core, &receipts[0], vec![]).await;
    invoke(&core,&receipts[1],"TaskUpdate",json!({"mutation_id":Uuid::new_v4(),"task_id":task.id,"expected_revision":1,"action":"claim"})).await.unwrap();
    let mut op = operation(
        &receipts[1],
        "Bash",
        json!({"command":"sleep 30","run_in_background":true}),
    );
    {
        let mut chats = core.inner.chats.lock().await;
        op.task_binding = execution_binding(&chats, &receipts[1]).unwrap();
        op.state = OperationState::Completed;
        let chat = chats.get_mut(&receipts[1].chat_id).unwrap();
        chat.snapshot
            .commands
            .push(tickloop_harness_protocol::CommandSnapshot {
                operation_id: op.id,
                loop_id: receipts[1].loop_id.into(),
                command: "sleep 30".into(),
                description: None,
                cwd: std::env::temp_dir(),
                pid: None,
                state: CommandState::Background,
                was_background: true,
                backgrounded_on_timeout: false,
                exit_code: None,
                signal: None,
                reason: None,
                output_ref: PathBuf::new(),
                output_bytes: 0,
                updated_at_ms: None,
            });
        chat.snapshot.operations.push(op);
    }
    let release = json!({"mutation_id":Uuid::new_v4(),"task_id":task.id,"expected_revision":2,"action":"release","result":"safe"});
    assert!(
        invoke(&core, &receipts[1], "TaskUpdate", release.clone())
            .await
            .unwrap_err()
            .contains("running or unknown")
    );
    {
        let mut chats = core.inner.chats.lock().await;
        let chat = chats.get_mut(&receipts[1].chat_id).unwrap();
        chat.snapshot.commands.last_mut().unwrap().state = CommandState::Completed;
        chat.snapshot.operations.last_mut().unwrap().state = OperationState::Unknown;
    }
    assert!(
        invoke(&core, &receipts[1], "TaskUpdate", release)
            .await
            .is_err()
    );
    core.shutdown().await;
}

#[tokio::test]
async fn mutation_receipts_survive_restart_without_resuming_claims() {
    let directory = tempfile::tempdir().unwrap();
    let store = directory.path().join("store");
    let (core, receipts) = family(Some(store.clone())).await;
    let mutation = Uuid::new_v4();
    let input =
        json!({"mutation_id":mutation,"title":"Durable","description":"Retain exactly once"});
    let first = invoke(&core, &receipts[0], "TaskCreate", input.clone())
        .await
        .unwrap();
    let second = invoke(&core, &receipts[0], "TaskCreate", input.clone())
        .await
        .unwrap();
    assert_eq!(first["task"]["id"], second["task"]["id"]);
    invoke(&core,&receipts[1],"TaskUpdate",json!({"mutation_id":Uuid::new_v4(),"task_id":first["task"]["id"],"expected_revision":1,"action":"claim"})).await.unwrap();
    core.shutdown().await;
    let model = Arc::new(tickloop_harness_test_support::ScriptedModel::new(vec![]));
    let restored = Agent::open_embedded(store, config(), model.clone()).unwrap();
    restored.resume_chat(receipts[1].chat_id).await.unwrap();
    let board = restored
        .snapshot(receipts[1].chat_id)
        .await
        .unwrap()
        .task_board
        .unwrap();
    assert_eq!(board.tasks.len(), 1);
    assert_eq!(board.tasks[0].status, Status::NeedsAttention);
    assert!(model.requests.lock().unwrap().is_empty());
    let root = restored.snapshot(receipts[0].chat_id).await.unwrap();
    assert!(
        root.task_boards[&receipts[0].loop_id]
            .mutations
            .contains_key(&mutation)
    );
    let branch = restored
        .branch_chat(tickloop_harness_protocol::BranchRequest {
            request_id: Uuid::new_v4(),
            source_chat_id: receipts[0].chat_id,
            name: Some("Independent".into()),
        })
        .await
        .unwrap();
    assert!(branch.snapshot.task_boards.is_empty());
    assert!(branch.snapshot.task_board.is_none());
    restored.shutdown().await;
}

#[tokio::test]
async fn a_failed_checkpoint_retains_the_claim_and_blocks_competing_execution() {
    let directory = tempfile::tempdir().unwrap();
    let store = directory.path().join("store");
    let (core, receipts) = family(Some(store.clone())).await;
    let task = create(&core, &receipts[0], vec![]).await;
    let path = tickloop_harness_test_support::transcript_file(&store, receipts[0].chat_id.into());
    std::fs::remove_file(path).unwrap();
    let claim =
        |id| json!({"mutation_id":id,"task_id":task.id,"expected_revision":1,"action":"claim"});
    let error = invoke(&core, &receipts[1], "TaskUpdate", claim(Uuid::new_v4()))
        .await
        .unwrap_err();
    assert!(error.contains("outcome unknown"), "{error}");
    assert!(
        invoke(&core, &receipts[2], "TaskUpdate", claim(Uuid::new_v4()))
            .await
            .is_err()
    );
    let chats = core.inner.chats.lock().await;
    assert!(
        execution_binding(&chats, &receipts[1])
            .unwrap_err()
            .contains("persistence is unresolved")
    );
    let board = &chats[&receipts[0].chat_id].snapshot.task_boards[&receipts[0].loop_id];
    assert_eq!(
        board.tasks[0].owner.as_ref().unwrap().chat_id,
        receipts[1].chat_id
    );
    drop(chats);
    core.shutdown().await;
}

#[tokio::test]
async fn unresolved_owner_cannot_start_unbound_work_or_be_reassigned_while_active() {
    let (core, receipts) = family(None).await;
    let task = create(&core, &receipts[0], vec![]).await;
    invoke(&core,&receipts[1],"TaskUpdate",json!({"mutation_id":Uuid::new_v4(),"task_id":task.id,"expected_revision":1,"action":"claim"})).await.unwrap();
    core.settle_task_claims(&receipts[1]).await;
    let next = Receipt {
        run_id: Uuid::new_v4().into(),
        ..receipts[1].clone()
    };
    {
        let mut chats = core.inner.chats.lock().await;
        chats.get_mut(&next.chat_id).unwrap().active_run = Some(next.run_id);
        assert!(
            execution_binding(&chats, &next)
                .unwrap_err()
                .contains("needs attention")
        );
    }
    assert!(invoke(&core,&receipts[0],"TaskUpdate",json!({"mutation_id":Uuid::new_v4(),"task_id":task.id,"expected_revision":3,"action":"release","result":"reconciled"})).await.unwrap_err().contains("prior task owner's current execution"));
    core.shutdown().await;
}

#[tokio::test]
async fn a_delegates_unrelated_later_run_does_not_own_the_old_task() {
    let (core, receipts) = family(None).await;
    let task = create(&core, &receipts[0], vec![]).await;
    invoke(&core,&receipts[0],"TaskUpdate",json!({"mutation_id":Uuid::new_v4(),"task_id":task.id,"expected_revision":1,"action":"claim"})).await.unwrap();
    core.bind_task_delegate(&receipts[0], &receipts[1])
        .await
        .unwrap();
    let next = Receipt {
        run_id: Uuid::new_v4().into(),
        ..receipts[1].clone()
    };
    let fixture_cancellations = {
        let mut chats = core.inner.chats.lock().await;
        let child = chats.get_mut(&next.chat_id).unwrap();
        let cancellations = child
            .parked
            .values()
            .map(|run| run.cancel.clone())
            .collect::<Vec<_>>();
        child.parked.clear();
        child.family_finishing.clear();
        child.snapshot.pending_inputs.clear();
        child.snapshot.questions.clear();
        for operation in &mut child.snapshot.operations {
            operation.state = OperationState::Completed;
        }
        child.active_run = Some(next.run_id);
        child
            .snapshot
            .operations
            .push(operation(&next, "Bash", json!({"command":"other work"})));
        cancellations
    };
    core.bind_task_delegate(&next, &receipts[2]).await.unwrap();
    let root = core.snapshot(receipts[0].chat_id).await.unwrap();
    assert_eq!(
        root.task_boards[&receipts[0].loop_id].tasks[0]
            .delegated_runs
            .len(),
        1
    );
    invoke(&core,&receipts[0],"TaskUpdate",json!({"mutation_id":Uuid::new_v4(),"task_id":task.id,"expected_revision":2,"action":"complete","result":"Old delegated work completed"})).await.unwrap();
    for cancel in fixture_cancellations {
        cancel.cancel();
    }
    tokio::time::timeout(Duration::from_secs(5), core.shutdown())
        .await
        .unwrap();
}

#[tokio::test]
async fn task_command_wait_does_not_adopt_other_runs_or_unknown_results_and_honors_cancel() {
    for case in [
        "other-run",
        "unknown",
        "cancelled",
        "parked-running",
        "parked-notified",
    ] {
        let (core, receipts) = family(None).await;
        let receipt = &receipts[0];
        let task = create(&core, receipt, vec![]).await;
        invoke(&core, receipt, "TaskUpdate", json!({"mutation_id":Uuid::new_v4(),"task_id":task.id,"expected_revision":1,"action":"claim"})).await.unwrap();
        let mut op = operation(
            receipt,
            "Bash",
            json!({"command":"wait","run_in_background":true}),
        );
        if case == "other-run" {
            op.run_id = Uuid::new_v4().into();
        }
        let state = if case == "unknown" {
            CommandState::Unknown
        } else if case == "parked-notified" {
            CommandState::Completed
        } else {
            CommandState::Background
        };
        let parked_cancellations = {
            let mut chats = core.inner.chats.lock().await;
            let chat = chats.get_mut(&receipt.chat_id).unwrap();
            let cancellations = if case.starts_with("parked-") {
                for parked in chat.parked.values_mut() {
                    parked.receipt.run_id = Uuid::new_v4().into();
                    parked.receipt.loop_id = Uuid::new_v4().into();
                }
                Vec::new()
            } else {
                let cancellations = chat
                    .parked
                    .values()
                    .map(|parked| parked.cancel.clone())
                    .collect::<Vec<_>>();
                chat.parked.clear();
                cancellations
            };
            if case == "parked-notified" {
                chat.bookkeeping
                    .deferred_notifications
                    .push(tick_protocol::InstanceRef::shell(op.id));
                chat.deferred_messages.push(Message {
                    role: Role::User,
                    content: vec![Content::Text {
                        text: "Blocked completion notification".into(),
                    }],
                });
            }
            chat.snapshot
                .commands
                .push(tickloop_harness_protocol::CommandSnapshot {
                    operation_id: op.id,
                    loop_id: receipt.loop_id.into(),
                    command: "wait".into(),
                    description: None,
                    cwd: std::env::temp_dir(),
                    pid: None,
                    state,
                    was_background: true,
                    backgrounded_on_timeout: false,
                    exit_code: None,
                    signal: None,
                    reason: None,
                    output_ref: PathBuf::new(),
                    output_bytes: 0,
                    updated_at_ms: None,
                });
            chat.snapshot.operations.push(op);
            cancellations
        };
        let cancel = CancellationToken::new();
        if case == "cancelled" {
            cancel.cancel();
        }
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            core.wait_for_task_commands(receipt, &cancel),
        )
        .await
        .unwrap();
        if case == "cancelled" {
            assert!(result.unwrap_err().contains("interrupted"));
        } else {
            assert!(!result.unwrap());
        }
        if case == "parked-notified" {
            let chats = core.inner.chats.lock().await;
            let chat = &chats[&receipt.chat_id];
            assert_eq!(chat.bookkeeping.deferred_notifications.len(), 1);
            assert_eq!(chat.deferred_messages.len(), 1);
        }
        core.settle_task_claims(receipt).await;
        let snapshot = core.snapshot(receipt.chat_id).await.unwrap();
        for cancel in parked_cancellations {
            cancel.cancel();
        }
        core.shutdown().await;
        assert_eq!(
            snapshot.task_boards[&receipt.loop_id].tasks[0].status,
            Status::NeedsAttention
        );
    }
}

#[tokio::test]
async fn task_command_wait_consumes_an_already_queued_completion_once() {
    let (core, receipts) = family(None).await;
    let receipt = &receipts[0];
    let task = create(&core, receipt, vec![]).await;
    invoke(&core, receipt, "TaskUpdate", json!({"mutation_id":Uuid::new_v4(),"task_id":task.id,"expected_revision":1,"action":"claim"})).await.unwrap();
    let mut op = operation(
        receipt,
        "Bash",
        json!({"command":"done","run_in_background":true}),
    );
    op.state = OperationState::Completed;
    let parked_cancellations = {
        let mut chats = core.inner.chats.lock().await;
        let chat = chats.get_mut(&receipt.chat_id).unwrap();
        let cancellations = chat
            .parked
            .values()
            .map(|parked| parked.cancel.clone())
            .collect::<Vec<_>>();
        chat.parked.clear();
        chat.snapshot
            .commands
            .push(tickloop_harness_protocol::CommandSnapshot {
                operation_id: op.id,
                loop_id: receipt.loop_id.into(),
                command: "done".into(),
                description: None,
                cwd: std::env::temp_dir(),
                pid: None,
                state: CommandState::Completed,
                was_background: true,
                backgrounded_on_timeout: false,
                exit_code: Some(0),
                signal: None,
                reason: None,
                output_ref: PathBuf::new(),
                output_bytes: 0,
                updated_at_ms: None,
            });
        chat.bookkeeping
            .deferred_notifications
            .push(tick_protocol::InstanceRef::shell(op.id));
        chat.deferred_messages.push(Message {
            role: Role::User,
            content: vec![Content::Text {
                text: "Owned command completed".into(),
            }],
        });
        chat.snapshot.operations.push(op);
        cancellations
    };
    let cancel = CancellationToken::new();
    assert!(core.wait_for_task_commands(receipt, &cancel).await.unwrap());
    assert!(!core.wait_for_task_commands(receipt, &cancel).await.unwrap());
    let snapshot = core.snapshot(receipt.chat_id).await.unwrap();
    for cancel in parked_cancellations {
        cancel.cancel();
    }
    core.shutdown().await;
    assert_eq!(snapshot.messages.iter().flat_map(|message| &message.content).filter(|content| matches!(content, Content::Text { text } if text == "Owned command completed")).count(), 1);
    assert_eq!(
        snapshot.task_boards[&receipt.loop_id].tasks[0].status,
        Status::InProgress
    );
    assert_eq!(snapshot.task_boards[&receipt.loop_id].tasks[0].revision, 2);
}

/// TaskGet and TaskUpdate take a task and its dependencies as TaskList wrote them or as UUIDs
/// from an older session; an id that names nothing is echoed with where ids come from.
#[test]
fn task_references_resolve_in_either_form() {
    let ids = IdContext::new();
    let (task, dependency) = (SharedTaskId::new(), SharedTaskId::new());
    for (task_text, dependency_text) in [
        (ids.model_text(task), dependency.uuid().to_string()),
        (task.uuid().to_string(), ids.model_text(dependency)),
    ] {
        let resolved = resolve_task_references(
            &ids,
            &json!({"task_id":task_text,"dependencies":[dependency_text],"title":"t"}),
        )
        .unwrap();
        assert_eq!(
            resolved,
            json!({"task_id":task.uuid().to_string(),"dependencies":[dependency.uuid().to_string()],"title":"t"})
        );
    }
    assert_eq!(
        resolve_task_references(&ids, &json!({"task_id":"3"})),
        Err(tickloop_ids::not_found::<SharedTaskKind>("3"))
    );
}

/// The flat TaskUpdate schema offers every action's fields, and models fill the ones the action
/// does not take, empty or with placeholder content; those are dropped before the call is
/// prepared, so they neither fail it nor make a retry differ.
#[tokio::test]
async fn task_update_drops_fields_of_other_actions() {
    use tickloop_harness_test_support::routed::tool_result;
    let completion = Uuid::new_v4();
    let model = Arc::new(RoutedModel::new(move |request| {
        let Some(created) = tool_result(&request.messages, "create") else {
            return call(
                "create",
                "TaskCreate",
                json!({"mutation_id":Uuid::new_v4(),"title":"Inspect","description":"Inspect authorized work"}),
            );
        };
        let task_id = created["task"]["id"].clone();
        let update = |fields: Value| {
            let mut input = json!({"task_id":task_id,"expected_revision":2});
            input
                .as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            input
        };
        for (id, input) in [
            (
                "claim",
                json!({"mutation_id":Uuid::new_v4(),"task_id":task_id,"expected_revision":1,"action":"claim","owner":"main","result":"started","title":"Inspect","description":"Inspect authorized work","dependencies":[]}),
            ),
            (
                "complete",
                update(
                    json!({"mutation_id":completion,"action":"complete","result":"Verified","dependencies":[task_id],"title":"Inspect","owner":null}),
                ),
            ),
            (
                "retry",
                update(json!({"mutation_id":completion,"action":"complete","result":"Verified"})),
            ),
        ] {
            if !has_tool_result(request, id) {
                return call(id, "TaskUpdate", input);
            }
        }
        text("done")
    }));
    let core = Agent::new_embedded(config(), model);
    let chat = core.create_chat(std::env::temp_dir()).await.unwrap();
    core.set_execution_mode(chat, tickloop_harness_protocol::ExecutionMode::FullAccess)
        .await
        .unwrap();
    core.set_task_tools_enabled(chat, true).await.unwrap();
    let mut events = core.subscribe();
    let receipt = core.submit(chat, "authorized work".into()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.run_id == receipt.run_id {
                match event.kind {
                    EventKind::Completed { .. } => break,
                    EventKind::Failed(error) => panic!("{error}"),
                    _ => {}
                }
            }
        }
    })
    .await
    .unwrap();
    let snapshot = core.snapshot(chat).await.unwrap();
    core.shutdown().await;
    let result = |id: &str| {
        snapshot
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .find_map(|content| match content {
                Content::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } if tool_use_id == id => Some((content.text(), *is_error)),
                _ => None,
            })
            .unwrap()
    };
    let (claimed, is_error) = result("claim");
    assert!(!is_error, "{claimed}");
    let (completed, is_error) = result("complete");
    assert!(!is_error, "{completed}");
    assert_eq!(
        serde_json::from_str::<Value>(&completed).unwrap()["status"],
        "applied"
    );
    let (retried, is_error) = result("retry");
    assert!(!is_error, "{retried}");
    assert_eq!(
        serde_json::from_str::<Value>(&retried).unwrap()["status"],
        "already_applied"
    );
    let board = &snapshot.task_boards[&receipt.loop_id];
    assert_eq!(board.tasks[0].status, Status::Completed);
    assert_eq!(board.tasks[0].result.as_deref(), Some("Verified"));
    assert_eq!(
        board.mutations[&completion].input,
        format!(
            "TaskUpdate:{}",
            json!({"task_id":tool_result(&snapshot.messages, "create").unwrap()["task"]["id"],"expected_revision":2,"mutation_id":completion,"action":"complete","result":"Verified"})
        )
    );
}

/// Fields of other actions are dropped whatever they hold; an action's own fields, keys outside
/// the schema and input without a known action reach the strict parse as written.
#[test]
fn prepared_update_input_keeps_what_the_strict_parse_judges() {
    for unchanged in [
        json!({"action":"complete","result":""}),
        json!({"action":"edit","title":"t","description":"d","dependencies":[]}),
        json!({"action":"assign","owner":""}),
        json!({"action":"claim","foo":""}),
        json!({"action":"merge","dependencies":["x"]}),
        json!({"result":"r","dependencies":[]}),
        json!({"action":7,"dependencies":[]}),
    ] {
        assert_eq!(prepared_update_input(unchanged.clone()), unchanged);
    }
    assert_eq!(
        prepared_update_input(
            json!({"action":"edit","title":"t","description":"d","dependencies":[],"owner":null,"result":"r"})
        ),
        json!({"action":"edit","title":"t","description":"d","dependencies":[]})
    );
    assert_eq!(
        prepared_update_input(
            json!({"action":"assign","owner":"a","dependencies":["x"],"title":"t","result":""})
        ),
        json!({"action":"assign","owner":"a"})
    );
    assert_eq!(
        prepared_update_input(
            json!({"action":"claim","owner":"main","result":"started","title":"t","description":"d","dependencies":[]})
        ),
        json!({"action":"claim"})
    );
}

/// An edit changes only the fields it names; leaving dependencies out keeps them, and an edit
/// naming none is refused without changing the task.
#[tokio::test]
async fn task_edit_changes_only_the_fields_it_names() {
    let (core, receipts) = family(None).await;
    let first = create(&core, &receipts[0], vec![]).await;
    let task = create(&core, &receipts[0], vec![first.id.into()]).await;
    let edit = |revision: u64, fields: Value| {
        let mut input = json!({"mutation_id":Uuid::new_v4(),"task_id":task.id,"expected_revision":revision,"action":"edit"});
        input
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        input
    };
    let edited =
        |result: Value| serde_json::from_value::<SharedTask>(result["task"].clone()).unwrap();

    let renamed = edited(
        invoke(
            &core,
            &receipts[0],
            "TaskUpdate",
            edit(1, json!({"title":"Renamed"})),
        )
        .await
        .unwrap(),
    );
    assert_eq!(renamed.title, "Renamed");
    assert_eq!(renamed.description, task.description);
    assert_eq!(renamed.dependencies, task.dependencies);
    assert!(!renamed.dependencies.is_empty());

    let described = edited(
        invoke(
            &core,
            &receipts[0],
            "TaskUpdate",
            edit(2, json!({"description":"New scope"})),
        )
        .await
        .unwrap(),
    );
    assert_eq!(described.title, "Renamed");
    assert_eq!(described.description, "New scope");
    assert_eq!(described.dependencies, task.dependencies);

    let refused = invoke(&core, &receipts[0], "TaskUpdate", edit(3, json!({})))
        .await
        .unwrap_err();
    assert!(
        refused.contains("edit requires at least one of title, description or dependencies"),
        "{refused}"
    );

    let cleared = edited(
        invoke(
            &core,
            &receipts[0],
            "TaskUpdate",
            edit(3, json!({"dependencies":[]})),
        )
        .await
        .unwrap(),
    );
    assert_eq!(cleared.revision, 4);
    assert_eq!(cleared.title, "Renamed");
    assert!(cleared.dependencies.is_empty());
    core.shutdown().await;
}

/// The TaskUpdate description offers edit as the partial update the parse accepts, so a model
/// asked to change only a title calls it.
#[test]
fn task_update_description_offers_a_partial_edit() {
    let update = definitions(TaskToolAccess::Enabled)
        .into_iter()
        .find(|definition| definition.name == "TaskUpdate")
        .unwrap();
    assert!(
        update.description.contains(
            "edit takes any of title, description and dependencies (the full list, [] for none); omitted fields keep their current value; give at least one."
        ),
        "{}",
        update.description
    );
    assert!(!update.description.contains("edit requires"));
    assert_eq!(
        update.input_schema["required"],
        json!(["mutation_id", "task_id", "expected_revision", "action"])
    );
}
