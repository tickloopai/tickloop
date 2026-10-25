use super::{tests::*, *};

async fn complete(core: &Agent, receipt: &Receipt, task_id: Uuid) {
    invoke(core, receipt, "TaskUpdate", json!({"mutation_id":Uuid::new_v4(),"task_id":task_id,"expected_revision":1,"action":"claim"})).await.unwrap();
    invoke(core, receipt, "TaskUpdate", json!({"mutation_id":Uuid::new_v4(),"task_id":task_id,"expected_revision":2,"action":"complete","result":"Finished authorized work"})).await.unwrap();
}

#[tokio::test]
async fn completed_cleanup_is_shared_durable_and_does_not_replay_old_mutations() {
    let directory = tempfile::tempdir().unwrap();
    let store = directory.path().join("store");
    let (core, receipts) = family(Some(store.clone())).await;
    let root = &receipts[0];
    let input = json!({"mutation_id":Uuid::new_v4(),"title":"Finished","description":"Keep history, clear live tasks"});
    let created = invoke(&core, root, "TaskCreate", input.clone())
        .await
        .unwrap();
    let task_id = SharedTaskId::parse(created["task"]["id"].as_str().unwrap())
        .unwrap()
        .uuid();
    complete(&core, root, task_id).await;
    {
        let mut chats = core.inner.chats.lock().await;
        let board = chats
            .get_mut(&root.chat_id)
            .unwrap()
            .snapshot
            .task_boards
            .get_mut(&root.loop_id)
            .unwrap();
        board.mutations.values_mut().next().unwrap().notification = Some(TaskAssignment {
            target: receipts[1].chat_id,
            message: "Previously queued assignment".into(),
            delivered: false,
        });
    }
    let before = core.snapshot(root.chat_id).await.unwrap();
    let revision = before.task_boards[&root.loop_id].revision;
    let mut events = core.subscribe();
    core.clear_completed_tasks(receipts[1].chat_id, root.loop_id, revision)
        .await
        .unwrap();
    for receipt in &receipts {
        let board = core
            .snapshot(receipt.chat_id)
            .await
            .unwrap()
            .task_board
            .unwrap();
        assert!(board.tasks.is_empty());
        assert_eq!(board.task_number_offset, 1);
        assert_eq!(board.revision, revision + 1);
    }
    let event = events.try_recv().unwrap();
    assert!(
        matches!(event.kind, EventKind::TaskBoardChanged { revision: next, .. } if next == revision + 1)
    );
    let after = core.snapshot(root.chat_id).await.unwrap();
    assert_eq!(
        serde_json::to_value(&before.messages).unwrap(),
        serde_json::to_value(&after.messages).unwrap()
    );
    assert!(
        after.task_boards[&root.loop_id]
            .mutations
            .values()
            .all(|mutation| mutation.task_cleared && mutation.notification.is_none())
    );
    validate_restored_boards(&after).unwrap();
    let mut invalid = after.clone();
    invalid
        .task_boards
        .get_mut(&root.loop_id)
        .unwrap()
        .mutations
        .values_mut()
        .next()
        .unwrap()
        .task_cleared = false;
    assert!(validate_restored_boards(&invalid).is_err());
    let replay = invoke(&core, root, "TaskCreate", input.clone())
        .await
        .unwrap();
    assert_eq!(replay["status"], "already_applied");
    assert_eq!(replay["task_cleared"], true);
    assert!(replay["task"].is_null());
    assert_eq!(
        invoke(&core, root, "TaskList", json!({})).await.unwrap()["board"]["tasks"],
        json!([])
    );
    assert_eq!(
        invoke(&core, root, "TaskGet", json!({"task_id":task_id}))
            .await
            .unwrap_err(),
        tickloop_ids::not_found::<tickloop_harness_protocol::SharedTaskKind>(&task_id.to_string())
    );
    core.clear_completed_tasks(root.chat_id, root.loop_id, revision)
        .await
        .unwrap();
    create(&core, root, vec![]).await;
    let next = core.snapshot(root.chat_id).await.unwrap();
    assert_eq!(next.task_boards[&root.loop_id].task_number(0), Some(2));
    assert_eq!(next.task_boards[&root.loop_id].task_number(1), None);
    let mut exhausted = next.clone();
    exhausted
        .task_boards
        .get_mut(&root.loop_id)
        .unwrap()
        .task_number_offset = u64::MAX;
    assert!(validate_restored_boards(&exhausted).is_err());
    core.shutdown().await;

    let model = std::sync::Arc::new(tickloop_harness_test_support::ScriptedModel::new(vec![]));
    let restored = Agent::open_embedded(
        store,
        tickloop_harness_test_support::config(),
        model.clone(),
    )
    .unwrap();
    restored.resume_chat(receipts[1].chat_id).await.unwrap();
    let restored_root = restored.snapshot(root.chat_id).await.unwrap();
    validate_restored_boards(&restored_root).unwrap();
    assert_eq!(
        restored_root.task_boards[&root.loop_id].task_number_offset,
        1
    );
    assert_eq!(restored_root.task_boards[&root.loop_id].tasks.len(), 1);
    assert!(
        restored_root.task_boards[&root.loop_id]
            .mutations
            .values()
            .any(|mutation| mutation.task_cleared)
    );
    assert!(model.requests.lock().unwrap().is_empty());
    restored.shutdown().await;
}

#[tokio::test]
async fn completed_cleanup_rechecks_settings_scope_revision_and_every_task_status() {
    let (core, receipts) = family(None).await;
    let root = &receipts[0];
    let first = create(&core, root, vec![]).await;
    complete(&core, root, first.id.into()).await;
    let revision = core.snapshot(root.chat_id).await.unwrap().task_boards[&root.loop_id].revision;
    let second = create(&core, root, vec![]).await;
    core.clear_completed_tasks(root.chat_id, root.loop_id, revision)
        .await
        .unwrap();
    assert_eq!(
        core.snapshot(root.chat_id).await.unwrap().task_boards[&root.loop_id]
            .tasks
            .len(),
        2
    );
    assert!(
        core.clear_completed_tasks(receipts[1].chat_id, Uuid::new_v4().into(), revision)
            .await
            .is_err()
    );
    let foreign = core.create_chat(std::env::temp_dir()).await.unwrap();
    assert!(
        core.clear_completed_tasks(foreign, root.loop_id, revision)
            .await
            .is_err()
    );
    for status in [
        Status::Pending,
        Status::InProgress,
        Status::Cancelled,
        Status::NeedsAttention,
    ] {
        let revision = {
            let mut chats = core.inner.chats.lock().await;
            let board = chats
                .get_mut(&root.chat_id)
                .unwrap()
                .snapshot
                .task_boards
                .get_mut(&root.loop_id)
                .unwrap();
            board
                .tasks
                .iter_mut()
                .find(|task| task.id == second.id)
                .unwrap()
                .status = status;
            board.revision
        };
        core.clear_completed_tasks(root.chat_id, root.loop_id, revision)
            .await
            .unwrap();
        assert_eq!(
            core.snapshot(root.chat_id).await.unwrap().task_boards[&root.loop_id]
                .tasks
                .len(),
            2
        );
    }
    let revision = {
        let mut chats = core.inner.chats.lock().await;
        let board = chats
            .get_mut(&root.chat_id)
            .unwrap()
            .snapshot
            .task_boards
            .get_mut(&root.loop_id)
            .unwrap();
        board
            .tasks
            .iter_mut()
            .find(|task| task.id == second.id)
            .unwrap()
            .status = Status::Completed;
        board.revision
    };
    core.set_task_tools_enabled(root.chat_id, false)
        .await
        .unwrap();
    core.clear_completed_tasks(root.chat_id, root.loop_id, revision)
        .await
        .unwrap();
    assert_eq!(
        core.snapshot(root.chat_id).await.unwrap().task_boards[&root.loop_id]
            .tasks
            .len(),
        2
    );
    core.set_task_tools_enabled(root.chat_id, true)
        .await
        .unwrap();
    core.clear_completed_tasks(root.chat_id, root.loop_id, revision)
        .await
        .unwrap();
    assert!(
        core.snapshot(root.chat_id).await.unwrap().task_boards[&root.loop_id]
            .tasks
            .is_empty()
    );
    core.shutdown().await;
}

#[tokio::test]
async fn failed_cleanup_checkpoint_returns_error_and_preserves_the_persistence_barrier() {
    let directory = tempfile::tempdir().unwrap();
    let store = directory.path().join("store");
    let (core, receipts) = family(Some(store.clone())).await;
    let root = &receipts[0];
    let task = create(&core, root, vec![]).await;
    complete(&core, root, task.id.into()).await;
    let revision = core.snapshot(root.chat_id).await.unwrap().task_boards[&root.loop_id].revision;
    std::fs::remove_file(tickloop_harness_test_support::transcript_file(
        &store,
        root.chat_id.into(),
    ))
    .unwrap();
    let error = core
        .clear_completed_tasks(root.chat_id, root.loop_id, revision)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("outcome unknown"));
    assert!(core.inner.chats.lock().await[&root.chat_id].task_checkpoint_pending);
    core.shutdown().await;
}
