use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use tickloop_harness_runtime::codemode::{Options, StoreWrites, run};

#[tokio::test]
async fn store_and_load_keep_json_values_and_report_successful_writes_only() {
    let stored = BTreeMap::from([
        ("cursor".to_owned(), json!({"page": 1})),
        ("stale".to_owned(), json!("old")),
    ]);
    let result = run(
        r#"
        const cursor = load("cursor");
        text(JSON.stringify([cursor, load("missing"), load("stale")]));
        store("cursor", {page: cursor.page + 1});
        store("empty", null);
        store("stale", undefined);
        text(String(load("stale")));
        cursor.page = 99;
        text(JSON.stringify(load("cursor")));
        "#,
        Options {
            store: stored.clone(),
            ..Default::default()
        },
        vec![],
        |_, _, _| async { unreachable!() },
    )
    .await;
    assert_eq!(result.error, None);
    assert_eq!(
        result.output,
        "[{\"page\":1},null,\"old\"]\nundefined\n{\"page\":2}"
    );
    assert_eq!(
        result.store_writes,
        StoreWrites {
            set: BTreeMap::from([
                ("cursor".to_owned(), json!({"page": 2})),
                ("empty".to_owned(), json!(null)),
            ]),
            delete: BTreeSet::from(["stale".to_owned()]),
        }
    );
    let failed = run(
        "store('cursor', 5); throw new Error('boom');",
        Options {
            store: stored,
            ..Default::default()
        },
        vec![],
        |_, _, _| async { unreachable!() },
    )
    .await;
    assert!(failed.error.is_some());
    assert_eq!(failed.store_writes, StoreWrites::default());
    let exited = run(
        "store('cursor', 7); exit();",
        Options::default(),
        vec![],
        |_, _, _| async { unreachable!() },
    )
    .await;
    assert_eq!(exited.error, None);
    assert_eq!(exited.store_writes.set["cursor"], json!(7));
}

#[tokio::test]
async fn store_limits_count_key_and_json_characters() {
    let result = run(
        r#"
        const errors = [];
        for (const attempt of [
            () => store("big", "x".repeat(262144)),
            () => store(1, "number key"),
            () => store("function", () => 1),
            () => load(2),
        ]) {
            try { attempt(); errors.push("stored"); } catch (error) { errors.push(error.name); }
        }
        for (let index = 0; index < 4; index++) store("fill" + index, "x".repeat(262100));
        try { store("over", "x".repeat(100)); errors.push("stored"); } catch (error) { errors.push(error.name); }
        store("fill0", undefined);
        store("over", "x".repeat(100));
        text(errors.join(","));
        "#,
        Options {
            store: BTreeMap::from([("seed".to_owned(), json!("x".repeat(100)))]),
            ..Default::default()
        },
        vec![],
        |_, _, _| async { unreachable!() },
    )
    .await;
    assert_eq!(result.error, None);
    assert_eq!(
        result.output,
        "RangeError,TypeError,TypeError,TypeError,RangeError"
    );
    assert!(!result.store_writes.set.contains_key("fill0"));
    assert!(
        result.store_writes.delete.is_empty(),
        "keys created and deleted in one script leave no write"
    );
    assert!(result.store_writes.set.contains_key("over"));
    assert!(!result.store_writes.set.contains_key("big"));
}

#[tokio::test]
async fn store_rejects_unpaired_surrogates_the_host_cannot_keep() {
    let result = run(
        r#"
        try { store("lone", "\ud800"); } catch (error) { text(error.name); }
        store("escaped", "\\ud800 and 😀");
        "#,
        Options::default(),
        vec![],
        |_, _, _| async { unreachable!() },
    )
    .await;
    assert_eq!(result.error, None);
    assert_eq!(result.output, "TypeError");
    assert_eq!(
        result.store_writes.set,
        BTreeMap::from([("escaped".to_owned(), json!("\\ud800 and 😀"))])
    );
}

#[tokio::test]
async fn tampered_builtins_cannot_bypass_store_limits() {
    let seed = BTreeMap::from([("seed".to_owned(), json!("kept"))]);
    for source in [
        r#"
        Map.prototype.has = () => true;
        Map.prototype.get = () => "x".repeat(1048576);
        for (let index = 0; index < 5; index++) store("fill" + index, "x".repeat(262100));
        "#,
        r#"
        Map.prototype.has = () => true;
        Map.prototype.get = () => "";
        for (let index = 0; index < 5000; index++) store("k" + index, index);
        "#,
    ] {
        let result = run(
            source,
            Options {
                store: seed.clone(),
                ..Default::default()
            },
            vec![],
            |_, _, _| async { unreachable!() },
        )
        .await;
        assert!(result.error.is_some(), "{source}");
        assert_eq!(result.store_writes, StoreWrites::default(), "{source}");
    }
    let result = run(
        r#"
        Array.prototype.toJSON = function () { return [["huge", JSON.stringify("x".repeat(300000))]]; };
        Object.defineProperty(Array.prototype, Symbol.iterator, {value: function* () { yield "injected"; }});
        store("small", 1);
        "#,
        Options {
            store: seed,
            ..Default::default()
        },
        vec![],
        |_, _, _| async { unreachable!() },
    )
    .await;
    assert_eq!(result.error, None);
    assert_eq!(
        result.store_writes,
        StoreWrites {
            set: BTreeMap::from([("small".to_owned(), json!(1))]),
            delete: BTreeSet::new(),
        }
    );
}

mod agent {
    use serde_json::json;
    use std::sync::Arc;
    use tickloop_harness_protocol::{
        BranchRequest, ChatId, ChatSnapshot, Content, EventKind, ExecutionMode, RewindMode,
    };
    use tickloop_harness_runtime::{
        Agent, HarnessOptions, HarnessSeams, environment::FolderEnvironment,
    };
    use tickloop_harness_test_support::{
        ScriptedModel, call, config, harness::ChosenModels, next_event, text,
        transcripts::InMemoryTranscriptStore,
    };

    const PROBE: &str = "text(String(load('cursor')) + '/' + String(load('tmp')));";

    fn agent(store: &Arc<InMemoryTranscriptStore>, model: &Arc<ScriptedModel>) -> Agent {
        Agent::with_seams(
            HarnessSeams {
                search: None,
                transcripts: Some(store.clone()),
                environment: Some(Arc::new(FolderEnvironment::bare())),
                models: ChosenModels::new(model.clone()),
            },
            HarnessOptions::local(config()),
        )
        .unwrap()
    }

    fn script(model: &ScriptedModel, call_id: &str, code: &str) {
        model.enqueue(call(call_id, "Codemode", json!({ "code": code })));
    }

    async fn turn(agent: &Agent, chat: ChatId, prompt: &str) {
        let mut events = agent.subscribe();
        agent.submit(chat, prompt.into()).await.unwrap();
        loop {
            let event = next_event(&mut events).await;
            if event.chat_id != chat {
                continue;
            }
            match event.kind {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
    }

    fn script_result(snapshot: &ChatSnapshot, call_id: &str) -> String {
        snapshot
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .find_map(|content| match content {
                Content::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } if tool_use_id == call_id => Some(format!("{content:?}")),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no result for {call_id}"))
    }

    #[tokio::test]
    async fn stored_values_follow_successful_scripts_reloads_and_rewinds() {
        let transcripts = Arc::new(InMemoryTranscriptStore::new());
        let model = Arc::new(ScriptedModel::new(vec![]));
        let first = agent(&transcripts, &model);
        let directory = tempfile::tempdir().unwrap();
        let chat = first.create_chat(directory.path().into()).await.unwrap();
        first
            .set_execution_mode(chat, ExecutionMode::FullAccess)
            .await
            .unwrap();
        script(&model, "save", "store('cursor', 1); store('tmp', 'x');");
        model.enqueue(text("saved"));
        turn(&first, chat, "Save the cursor").await;
        script(
            &model,
            "failing",
            "store('cursor', 2); throw new Error('fail');",
        );
        script(&model, "after-failure", PROBE);
        script(
            &model,
            "advance",
            "store('cursor', 3); store('tmp', undefined);",
        );
        model.enqueue(text("advanced"));
        turn(&first, chat, "Advance the cursor").await;
        let snapshot = first.snapshot(chat).await.unwrap();
        assert!(script_result(&snapshot, "after-failure").contains("1/x"));
        first.sync().await.unwrap();
        first.shutdown().await;

        let second = agent(&transcripts, &model);
        second.resume_chat(chat).await.unwrap();
        script(&model, "reloaded", PROBE);
        model.enqueue(text("checked"));
        turn(&second, chat, "Check after reload").await;
        let snapshot = second.snapshot(chat).await.unwrap();
        assert!(script_result(&snapshot, "reloaded").contains("3/undefined"));
        let advance_prompt = snapshot.rewind_points()[1].message_id;
        second
            .rewind(chat, advance_prompt, RewindMode::Conversation)
            .await
            .unwrap();
        script(&model, "rewound", PROBE);
        model.enqueue(text("rewound"));
        turn(&second, chat, "Check after rewind").await;
        let snapshot = second.snapshot(chat).await.unwrap();
        assert!(script_result(&snapshot, "rewound").contains("1/x"));
        second.sync().await.unwrap();
        second.shutdown().await;

        let third = agent(&transcripts, &model);
        third.resume_chat(chat).await.unwrap();
        script(&model, "rewound-reloaded", PROBE);
        model.enqueue(text("checked again"));
        turn(&third, chat, "Check the rewound branch after reload").await;
        let snapshot = third.snapshot(chat).await.unwrap();
        assert!(script_result(&snapshot, "rewound-reloaded").contains("1/x"));
        third.shutdown().await;
    }

    #[tokio::test]
    async fn a_branch_starts_from_the_source_values_and_then_keeps_its_own() {
        let transcripts = Arc::new(InMemoryTranscriptStore::new());
        let model = Arc::new(ScriptedModel::new(vec![]));
        let first = agent(&transcripts, &model);
        let directory = tempfile::tempdir().unwrap();
        let source = first.create_chat(directory.path().into()).await.unwrap();
        first
            .set_execution_mode(source, ExecutionMode::FullAccess)
            .await
            .unwrap();
        script(&model, "source-save", "store('cursor', 'source');");
        model.enqueue(text("saved"));
        turn(&first, source, "Save in the source").await;
        let branch = first
            .branch_chat(BranchRequest {
                request_id: uuid::Uuid::new_v4(),
                source_chat_id: source,
                name: Some("Cursor branch".into()),
            })
            .await
            .unwrap()
            .child_chat_id;
        first
            .set_execution_mode(branch, ExecutionMode::FullAccess)
            .await
            .unwrap();
        script(
            &model,
            "branch-probe",
            "text(String(load('cursor'))); store('cursor', 'branch');",
        );
        model.enqueue(text("branched"));
        turn(&first, branch, "Continue in the branch").await;
        script(&model, "source-probe", "text(String(load('cursor')));");
        model.enqueue(text("source checked"));
        turn(&first, source, "Check the source").await;
        assert!(
            script_result(&first.snapshot(branch).await.unwrap(), "branch-probe")
                .contains("source")
        );
        assert!(
            script_result(&first.snapshot(source).await.unwrap(), "source-probe")
                .contains("source")
        );
        first.sync().await.unwrap();
        first.shutdown().await;

        let second = agent(&transcripts, &model);
        for (chat, expected) in [(branch, "branch"), (source, "source")] {
            second.resume_chat(chat).await.unwrap();
            second
                .set_execution_mode(chat, ExecutionMode::FullAccess)
                .await
                .unwrap();
            let call_id = format!("reloaded-{expected}");
            script(&model, &call_id, "text('value=' + String(load('cursor')));");
            model.enqueue(text("checked"));
            turn(&second, chat, "Check after reload").await;
            assert!(
                script_result(&second.snapshot(chat).await.unwrap(), &call_id)
                    .contains(&format!("value={expected}"))
            );
        }
        let copy = second.resume_as_new_chat(branch).await.unwrap().id;
        second.sync().await.unwrap();
        second.shutdown().await;
        let third = agent(&transcripts, &model);
        third.resume_chat(copy).await.unwrap();
        script(
            &model,
            "copy-probe",
            "text('value=' + String(load('cursor')));",
        );
        model.enqueue(text("checked"));
        turn(&third, copy, "Check the copy").await;
        assert!(
            script_result(&third.snapshot(copy).await.unwrap(), "copy-probe")
                .contains("value=branch")
        );
        third.shutdown().await;
    }

    #[tokio::test]
    async fn stored_values_survive_compaction_and_a_reload_after_it() {
        let transcripts = Arc::new(InMemoryTranscriptStore::new());
        let model = Arc::new(ScriptedModel::new(vec![]));
        let first = agent(&transcripts, &model);
        let directory = tempfile::tempdir().unwrap();
        let chat = first.create_chat(directory.path().into()).await.unwrap();
        script(
            &model,
            "save",
            "store('cursor', 1); store('tmp', 'x'); store('gone', true);",
        );
        model.enqueue(text("saved"));
        turn(&first, chat, "Save the cursor").await;
        script(&model, "delete", "store('gone', undefined);");
        model.enqueue(text("deleted"));
        turn(&first, chat, "Delete a value").await;
        model.enqueue(text("Earlier scripts stored a cursor."));
        first.compact(chat).await.unwrap();
        script(
            &model,
            "after-compaction",
            "text(String(load('cursor')) + '/' + String(load('tmp')) + '/' + String(load('gone')));",
        );
        model.enqueue(text("checked"));
        turn(&first, chat, "Check after compaction").await;
        let snapshot = first.snapshot(chat).await.unwrap();
        assert!(
            script_result(&snapshot, "after-compaction").contains("1/x/undefined"),
            "{}",
            script_result(&snapshot, "after-compaction")
        );
        first.sync().await.unwrap();
        first.shutdown().await;

        let second = agent(&transcripts, &model);
        second.resume_chat(chat).await.unwrap();
        script(
            &model,
            "reloaded",
            "text(String(load('cursor')) + '/' + String(load('tmp')) + '/' + String(load('gone')));",
        );
        model.enqueue(text("checked again"));
        turn(&second, chat, "Check after reload").await;
        let snapshot = second.snapshot(chat).await.unwrap();
        assert!(script_result(&snapshot, "reloaded").contains("1/x/undefined"));
        second.shutdown().await;
    }

    #[tokio::test]
    async fn a_script_cut_off_by_a_process_exit_loads_as_interrupted_with_each_call_outcome() {
        use std::time::Duration;
        use tickloop_harness_protocol::{Decision, PermissionRule};
        use tickloop_harness_runtime::transcript::StorePosition;
        let (server, provider) = crate::codemode::slow_mcp_provider().await;
        server.set_delay(Duration::from_secs(30));
        let transcripts = Arc::new(InMemoryTranscriptStore::new());
        let model = Arc::new(ScriptedModel::new(vec![]));
        let first = agent(&transcripts, &model);
        first.install_host_tools(provider).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let chat = first.create_chat(directory.path().into()).await.unwrap();
        first
            .set_execution_mode(chat, ExecutionMode::FullAccess)
            .await
            .unwrap();
        first
            .set_permission_rules(
                chat,
                vec![PermissionRule {
                    id: tickloop_ids::generate(),
                    decision: Decision::Ask,
                    tool: "Glob".into(),
                    specifier: None,
                    input: None,
                    loop_id: None,
                    persistent: false,
                }],
            )
            .await
            .unwrap();
        script(
            &model,
            "script",
            "await tools.Write({file_path:'done.txt',content:'x'}); tools.mcp__slow__write({}); \
             await tools.Glob({pattern:'*.txt'}); store('never', 1);",
        );
        let mut events = first.subscribe();
        first.submit(chat, "Run the script".into()).await.unwrap();
        let request = loop {
            let event = next_event(&mut events).await;
            match event.kind {
                EventKind::PermissionRequested { request } if event.chat_id == chat => {
                    break request;
                }
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        };
        assert_eq!(request.parent_call.as_ref().unwrap().call_id, "script");
        tokio::time::timeout(Duration::from_secs(5), async {
            while server.calls().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // A view that attaches later finds the same pending confirmation under its script.
        let attached = first.snapshot(chat).await.unwrap();
        assert!(
            attached
                .permissions
                .iter()
                .any(|pending| pending.id == request.id
                    && pending.parent_call.as_ref().unwrap().call_id == "script")
        );
        first.sync().await.unwrap();

        // The process is gone now: a new holder sees only what the store holds.
        let crashed = Arc::new(InMemoryTranscriptStore::new());
        crashed.seed(chat, transcripts.lines(chat), StorePosition(1));
        let restarted_model = Arc::new(ScriptedModel::new(vec![]));
        let restarted = agent(&crashed, &restarted_model);
        let snapshot = restarted.resume_chat(chat).await.unwrap();
        let result = script_result(&snapshot, "script");
        assert!(result.contains("interrupted"), "{result}");
        assert!(result.contains("will not be rerun"), "{result}");
        for (name, outcome) in [
            ("Write", "completed"),
            ("mcp__slow__write", "outcome unknown"),
            ("Glob", "not executed"),
        ] {
            assert!(
                result
                    .lines()
                    .chain(result.split("\\n"))
                    .any(|line| line.contains(name) && line.contains(outcome)),
                "{name} {outcome}: {result}"
            );
        }
        assert_eq!(
            snapshot
                .operations
                .iter()
                .filter(|operation| operation.parent_call_id.as_deref() == Some("script"))
                .count(),
            3
        );
        assert!(snapshot.permissions.is_empty());
        assert_eq!(server.calls().len(), 1);
        assert!(restarted_model.requests.lock().unwrap().is_empty());
        restarted.shutdown().await;
        first.shutdown().await;
    }
}
