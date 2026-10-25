use super::tests::{create, family, invoke, operation};
use super::*;
use std::{sync::Arc, time::Duration};
use tickloop_agent_test_support::{ScriptedModel, config, text};

async fn finish(core: &Agent, chat: ChatId) -> Receipt {
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
    receipt
}

fn task_count(request: &ModelRequest) -> usize {
    request
        .tools
        .iter()
        .filter(|tool| {
            matches!(
                tool.name.as_str(),
                "TaskCreate" | "TaskGet" | "TaskList" | "TaskUpdate"
            )
        })
        .count()
}

#[tokio::test]
async fn settings_are_default_off_and_apply_to_all_new_loops_without_model_calls() {
    let model = Arc::new(ScriptedModel::new(vec![text("done"); 4]));
    let core = Agent::new_embedded(config(), model.clone());
    let chat = core.create_chat(std::env::temp_dir()).await.unwrap();
    finish(&core, chat).await;
    assert_eq!(task_count(&model.requests.lock().unwrap()[0]), 0);
    core.set_task_tools_enabled(chat, true).await.unwrap();
    assert_eq!(model.requests.lock().unwrap().len(), 1);
    finish(&core, chat).await;
    finish(&core, chat).await;
    assert_eq!(task_count(&model.requests.lock().unwrap()[1]), 4);
    assert_eq!(task_count(&model.requests.lock().unwrap()[2]), 4);
    assert!(core.snapshot(chat).await.unwrap().task_boards.is_empty());
    core.set_task_tools_enabled(chat, false).await.unwrap();
    finish(&core, chat).await;
    assert_eq!(task_count(&model.requests.lock().unwrap()[3]), 0);
    core.shutdown().await;
}

#[tokio::test]
async fn externally_saved_setting_updates_loaded_members_and_survives_restart() {
    let directory = tempfile::tempdir().unwrap();
    let settings = tickloop_agent_settings::UserSettings::in_root(directory.path());
    let model = Arc::new(ScriptedModel::new(vec![text("done"); 2]));
    let core = Agent::new_with_user_settings_embedded(config(), model.clone(), settings.clone());
    let chat = core
        .create_chat(directory.path().to_path_buf())
        .await
        .unwrap();
    settings.save_task_tools_enabled(true).unwrap();
    assert!(!core.status(chat).await.unwrap().task_tools_enabled);
    finish(&core, chat).await;
    assert!(core.status(chat).await.unwrap().task_tools_enabled);
    assert_eq!(task_count(&model.requests.lock().unwrap()[0]), 4);
    settings.save_task_tools_enabled(false).unwrap();
    finish(&core, chat).await;
    assert_eq!(task_count(&model.requests.lock().unwrap()[1]), 0);
    core.set_task_tools_enabled(chat, true).await.unwrap();
    core.shutdown().await;
    let reopened = Agent::new_with_user_settings_embedded(
        config(),
        Arc::new(ScriptedModel::new(vec![])),
        settings,
    );
    let chat = reopened
        .create_chat(directory.path().to_path_buf())
        .await
        .unwrap();
    assert!(reopened.status(chat).await.unwrap().task_tools_enabled);
    reopened.shutdown().await;
}

#[tokio::test]
async fn disabling_keeps_the_claiming_run_finishable_but_blocks_new_task_work() {
    let (core, receipts) = family(None).await;
    let task = create(&core, &receipts[0], vec![]).await;
    invoke(&core, &receipts[1], "TaskUpdate", json!({"mutation_id":Uuid::new_v4(),"task_id":task.id,"expected_revision":1,"action":"claim"})).await.unwrap();
    core.set_task_tools_enabled(receipts[1].chat_id, false)
        .await
        .unwrap();
    for receipt in &receipts {
        assert!(
            !core
                .status(receipt.chat_id)
                .await
                .unwrap()
                .task_tools_enabled
        );
    }
    {
        let chats = core.inner.chats.lock().await;
        assert!(tool_access(&chats, &receipts[1]) == TaskToolAccess::Finishing);
        assert_eq!(definitions(tool_access(&chats, &receipts[1])).len(), 3);
        assert!(execution_binding(&chats, &receipts[1]).unwrap().is_some());
        assert!(tool_access(&chats, &receipts[0]) == TaskToolAccess::Disabled);
    }
    assert!(
        invoke(
            &core,
            &receipts[1],
            "TaskCreate",
            json!({"mutation_id":Uuid::new_v4(),"title":"No","description":"No"})
        )
        .await
        .is_err()
    );
    assert!(
        invoke(&core, &receipts[0], "TaskList", json!({}))
            .await
            .is_err()
    );
    invoke(&core, &receipts[1], "TaskList", json!({}))
        .await
        .unwrap();
    invoke(&core, &receipts[1], "TaskUpdate", json!({"mutation_id":Uuid::new_v4(),"task_id":task.id,"expected_revision":2,"action":"complete","result":"verified"})).await.unwrap();
    assert!(
        invoke(&core, &receipts[1], "TaskList", json!({}))
            .await
            .is_err()
    );
    assert_eq!(
        core.snapshot(receipts[0].chat_id)
            .await
            .unwrap()
            .task_board
            .unwrap()
            .tasks[0]
            .status,
        Status::Completed
    );
    core.shutdown().await;
}

#[tokio::test]
async fn disabled_history_does_not_block_normal_work_but_reenable_requires_reconciliation() {
    let (core, receipts) = family(None).await;
    let task = create(&core, &receipts[0], vec![]).await;
    invoke(&core, &receipts[1], "TaskUpdate", json!({"mutation_id":Uuid::new_v4(),"task_id":task.id,"expected_revision":1,"action":"claim"})).await.unwrap();
    core.settle_task_claims(&receipts[1]).await;
    core.set_task_tools_enabled(receipts[0].chat_id, false)
        .await
        .unwrap();
    let next = Receipt {
        run_id: Uuid::new_v4().into(),
        ..receipts[1].clone()
    };
    {
        let chats = core.inner.chats.lock().await;
        assert!(execution_binding(&chats, &next).unwrap().is_none());
    }
    core.set_task_tools_enabled(receipts[0].chat_id, true)
        .await
        .unwrap();
    {
        let chats = core.inner.chats.lock().await;
        assert!(
            execution_binding(&chats, &next)
                .unwrap_err()
                .contains("needs attention")
        );
    }
    let unknown = operation(&next, "Write", json!({}));
    let cancelled_runs;
    {
        let mut chats = core.inner.chats.lock().await;
        let child = chats.get_mut(&next.chat_id).unwrap();
        child.active_run = None;
        child.active = None;
        cancelled_runs = child
            .parked
            .values()
            .map(|run| run.cancel.clone())
            .collect::<Vec<_>>();
        child.parked.clear();
        for operation in &mut child.snapshot.operations {
            operation.state = tickloop_agent_protocol::OperationState::Completed;
        }
        let mut unknown = unknown.clone();
        unknown.state = tickloop_agent_protocol::OperationState::Unknown;
        assert!(unknown.task_binding.is_none());
        child.snapshot.operations.push(unknown);
    }
    let release = json!({"mutation_id":Uuid::new_v4(),"task_id":task.id,"expected_revision":3,"action":"release","result":"checked"});
    let error = invoke(&core, &receipts[0], "TaskUpdate", release.clone())
        .await
        .unwrap_err();
    assert!(error.contains("running or unknown"), "{error}");
    {
        let mut chats = core.inner.chats.lock().await;
        chats
            .get_mut(&next.chat_id)
            .unwrap()
            .snapshot
            .operations
            .iter_mut()
            .find(|op| op.id == unknown.id)
            .unwrap()
            .state = tickloop_agent_protocol::OperationState::Completed;
    }
    {
        let mut chats = core.inner.chats.lock().await;
        chats
            .get_mut(&next.chat_id)
            .unwrap()
            .snapshot
            .tool_call_recoveries
            .push(tickloop_agent_protocol::ToolCallRecovery {
                run_id: Some(next.run_id),
                call_id: "later-off-work".into(),
                was_admitted: Some(true),
            });
    }
    let error = invoke(&core, &receipts[0], "TaskUpdate", release.clone())
        .await
        .unwrap_err();
    assert!(error.contains("recovered operations"), "{error}");
    core.inner
        .chats
        .lock()
        .await
        .get_mut(&next.chat_id)
        .unwrap()
        .snapshot
        .tool_call_recoveries
        .clear();
    invoke(&core, &receipts[0], "TaskUpdate", release)
        .await
        .unwrap();
    for cancel in cancelled_runs {
        cancel.cancel();
    }
    core.shutdown().await;
}

#[tokio::test]
async fn disabling_during_assignment_approval_prevents_notification_dispatch() {
    let (core, receipts) = family(None).await;
    let task = create(&core, &receipts[0], vec![]).await;
    let root = receipts[0].clone();
    core.set_permission_rules(
        root.chat_id,
        vec![tickloop_agent_protocol::PermissionRule {
            id: Uuid::new_v4(),
            decision: tickloop_agent_protocol::Decision::Ask,
            tool: "SendMessage".into(),
            specifier: None,
            input: None,
            loop_id: None,
            persistent: false,
        }],
    )
    .await
    .unwrap();
    let original_run_cancel = core.inner.chats.lock().await[&root.chat_id].parked[&root.run_id]
        .cancel
        .clone();
    let assignment = Uuid::new_v4();
    let mut events = core.subscribe();
    let assigning = core.clone();
    let sender = root.clone();
    let pending = tokio::spawn(async move {
        invoke(&assigning, &sender, "TaskUpdate", json!({"mutation_id":assignment,"task_id":task.id,"expected_revision":1,"action":"assign","owner":"a"})).await
    });
    let request = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = events.recv().await.unwrap();
            if let EventKind::PermissionRequested { request } = event.kind {
                break request;
            }
        }
    })
    .await
    .unwrap();
    core.set_task_tools_enabled(root.chat_id, false)
        .await
        .unwrap();
    core.answer_permission(root.chat_id, request.id, true)
        .await
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), pending)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        result["notification"]
            .as_str()
            .unwrap()
            .contains("disabled"),
        "{result}"
    );
    let chats = core.inner.chats.lock().await;
    let board = &chats[&root.chat_id].snapshot.task_boards[&root.loop_id];
    assert!(
        !board.mutations[&assignment]
            .notification
            .as_ref()
            .unwrap()
            .delivered
    );
    assert!(!chats.values().any(|chat| {
        chat.snapshot
            .agent_message_origins
            .values()
            .any(|source| source["message_id"] == assignment.to_string())
            || chat.snapshot.pending_inputs.iter().any(|input| {
                input
                    .agent_message
                    .as_ref()
                    .is_some_and(|source| source.message_id == assignment)
            })
    }));
    drop(chats);
    original_run_cancel.cancel();
    core.shutdown().await;
}

#[tokio::test]
async fn a_new_member_reuses_the_existing_solo_board_and_task() {
    use tickloop_agent_test_support::{
        call,
        routed::{RoutedModel, has_tool_result, state, tool_result},
    };
    let model = Arc::new(RoutedModel::new(|request| {
        if !state(request)["delegation"].is_null() {
            return text("child done");
        }
        if !has_tool_result(request, "create") {
            return call(
                "create",
                "TaskCreate",
                json!({"mutation_id":Uuid::new_v4(),"title":"Inspect","description":"Inspect authorized work"}),
            );
        }
        if !has_tool_result(request, "child") {
            return call(
                "child",
                "Subagent",
                json!({"name":"worker","task":"Inspect authorized work"}),
            );
        }
        text("root done")
    }));
    let core = Agent::new_embedded(config(), model);
    let chat = core.create_chat(std::env::temp_dir()).await.unwrap();
    core.set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    core.set_task_tools_enabled(chat, true).await.unwrap();
    let receipt = finish(&core, chat).await;
    let root = core.snapshot(chat).await.unwrap();
    let created = tool_result(&root.messages, "create").unwrap();
    assert_eq!(root.task_boards.len(), 1);
    let board = &root.task_boards[&receipt.loop_id];
    assert!(board.is_team);
    assert_eq!(
        created["task"]["id"],
        tickloop_ids::IdContext::new().model_text(board.tasks[0].id)
    );
    let child = core.subagents(chat).await.unwrap()[0].child_chat_id;
    assert_eq!(
        core.snapshot(child)
            .await
            .unwrap()
            .task_board
            .unwrap()
            .tasks[0]
            .id,
        board.tasks[0].id
    );
    core.shutdown().await;
}

#[tokio::test]
async fn explicit_setting_reads_refresh_only_the_target_configuration_without_running_work() {
    let target_root = tempfile::tempdir().unwrap();
    let other_root = tempfile::tempdir().unwrap();
    let workdir = tempfile::tempdir().unwrap();
    let target_settings = tickloop_agent_settings::UserSettings::in_root(target_root.path());
    let other_settings = tickloop_agent_settings::UserSettings::in_root(other_root.path());
    let model = Arc::new(ScriptedModel::new(vec![]));
    let target =
        Agent::new_with_user_settings_embedded(config(), model.clone(), target_settings.clone());
    let other =
        Agent::new_with_user_settings_embedded(config(), model.clone(), other_settings.clone());
    let target_chat = target.create_chat(workdir.path().into()).await.unwrap();
    let other_chat = other.create_chat(workdir.path().into()).await.unwrap();
    target_settings.save_task_tools_enabled(true).unwrap();
    other_settings.save_task_tools_enabled(false).unwrap();
    assert!(!target.status(target_chat).await.unwrap().task_tools_enabled);
    assert!(target.task_tools_setting(target_chat).await.unwrap());
    assert!(!other.task_tools_setting(other_chat).await.unwrap());
    assert!(target.status(target_chat).await.unwrap().task_tools_enabled);
    target_settings.save_task_tools_enabled(false).unwrap();
    other_settings.save_task_tools_enabled(true).unwrap();
    assert!(!target.task_tools_setting(target_chat).await.unwrap());
    assert!(other.task_tools_setting(other_chat).await.unwrap());
    assert!(matches!(
        target.task_tools_setting(other_chat).await,
        Err(AgentError::NotFound)
    ));
    let snapshot = target.snapshot(target_chat).await.unwrap();
    assert_eq!(snapshot.chat_loop.run_id, None);
    assert!(snapshot.messages.is_empty());
    assert!(snapshot.task_boards.is_empty());
    assert!(model.requests.lock().unwrap().is_empty());
    target.shutdown().await;
    other.shutdown().await;
}

#[tokio::test]
async fn ordinary_binding_checks_skip_blocked_settings_reads_until_tracking_can_change_the_binding()
{
    let directory = tempfile::tempdir().unwrap();
    let (core, receipts) = family(Some(directory.path().join("store"))).await;
    let task = create(&core, &receipts[0], vec![]).await;
    let settings_write = core.inner.settings_writes.lock().await;
    tokio::time::timeout(
        Duration::from_millis(100),
        core.refresh_task_binding_setting(&receipts[0]),
    )
    .await
    .unwrap()
    .unwrap();
    {
        let mut chats = core.inner.chats.lock().await;
        let task = chats
            .get_mut(&receipts[0].chat_id)
            .unwrap()
            .snapshot
            .task_boards
            .get_mut(&receipts[0].loop_id)
            .unwrap()
            .tasks
            .iter_mut()
            .find(|entry| entry.id == task.id)
            .unwrap();
        task.status = Status::InProgress;
        task.owner = Some(TaskOwner {
            chat_id: receipts[1].chat_id,
            name: "a".into(),
        });
        task.claim = Some(TaskClaim {
            run_id: receipts[1].run_id,
            generation: 0,
        });
    }
    tokio::time::timeout(
        Duration::from_millis(100),
        core.refresh_task_binding_setting(&receipts[0]),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        tokio::time::timeout(
            Duration::from_millis(20),
            core.refresh_task_binding_setting(&receipts[1])
        )
        .await
        .is_err()
    );
    for status in [Status::NeedsAttention, Status::Completed] {
        core.inner
            .chats
            .lock()
            .await
            .get_mut(&receipts[0].chat_id)
            .unwrap()
            .snapshot
            .task_boards
            .get_mut(&receipts[0].loop_id)
            .unwrap()
            .tasks[0]
            .status = status;
        let refreshed = tokio::time::timeout(
            Duration::from_millis(20),
            core.refresh_task_binding_setting(&receipts[1]),
        )
        .await;
        assert_eq!(refreshed.is_ok(), status == Status::Completed);
    }
    core.inner
        .chats
        .lock()
        .await
        .get_mut(&receipts[0].chat_id)
        .unwrap()
        .task_checkpoint_pending = true;
    assert!(
        tokio::time::timeout(
            Duration::from_millis(20),
            core.refresh_task_binding_setting(&receipts[0])
        )
        .await
        .is_err()
    );
    core.inner
        .chats
        .lock()
        .await
        .get_mut(&receipts[0].chat_id)
        .unwrap()
        .task_checkpoint_pending = false;
    drop(settings_write);
    core.shutdown().await;
}

#[tokio::test]
async fn host_admission_rechecks_task_binding_after_waiting_for_the_commit_lock() {
    use std::future::Future;
    let (core, receipts) = family(None).await;
    let receipt = &receipts[0];
    let task = create(&core, receipt, vec![]).await;
    invoke(&core, receipt, "TaskUpdate", json!({"mutation_id":Uuid::new_v4(),"task_id":task.id,"expected_revision":1,"action":"claim"})).await.unwrap();
    core.settle_task_claims(receipt).await;
    core.set_task_tools_enabled(receipt.chat_id, false)
        .await
        .unwrap();
    let mut operation = operation(receipt, "MutateHost", json!({}));
    operation.state = OperationState::Proposed;
    let cancel = CancellationToken::new();
    let mut chats = core.inner.chats.lock().await;
    let expected = binding_for_tool(&chats, receipt, &operation.name).unwrap();
    assert!(expected.is_none());
    let chat = chats.get_mut(&receipt.chat_id).unwrap();
    let revision = chat.snapshot.policy.revision;
    chat.snapshot.operations.push(operation.clone());
    let session_operation = SessionPermissionOperation::from_normalized(
        operation.name.clone(),
        operation.input.clone(),
        session_permissions::capture_workspace_scope(&configured_workspace_for_chat(chat)).unwrap(),
    )
    .unwrap();
    let evaluation = core.inner.session_permissions.lock().await.evaluate(
        PermissionOwner::new(receipt.chat_id.into(), receipt.loop_id.into()).unwrap(),
        &session_operation,
    );
    let admission = core.admit_host_operation(
        receipt,
        &operation,
        &operation.input,
        HostAdmission {
            tool_name: &operation.name,
            expected_task_binding: Some(expected),
            session_evaluation: &evaluation,
            policy_revision: revision,
            ancestor_revisions: &[],
            host_permission: Some(crate::host_tools::HostToolPermission::ReadOnly),
            local_read: None,
            approval_path: ApprovalPath::Direct,
            direct_basis: denials::DirectApprovalBasis::ReadOnly,
            approval_source: None,
            assessment: None,
        },
        &cancel,
    );
    tokio::pin!(admission);
    let mut context = std::task::Context::from_waker(futures_util::task::noop_waker_ref());
    assert!(admission.as_mut().poll(&mut context).is_pending());
    // Model the setting commit while final admission waits for the same Chat lock.
    core.inner
        .task_tools_enabled
        .store(true, std::sync::atomic::Ordering::SeqCst);
    for chat in chats.values_mut() {
        chat.snapshot.task_tools_enabled = true;
    }
    drop(chats);
    let error = admission.await.unwrap_err();
    assert!(error.contains("needs attention"), "{error}");
    let snapshot = core.snapshot(receipt.chat_id).await.unwrap();
    assert_eq!(
        snapshot
            .operations
            .iter()
            .find(|entry| entry.id == operation.id)
            .unwrap()
            .state,
        OperationState::Proposed
    );
    assert!(!snapshot.events.iter().any(|event| matches!(&event.kind, EventKind::ToolStarted { call_id, .. } if call_id == &operation.call_id)));
    core.shutdown().await;
}
