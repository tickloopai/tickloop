use std::{sync::Arc, time::Duration};
use tickloop_code_settings::local_agent::LocalAgent;

use serde_json::json;
use tickloop_harness::protocol::{EventKind, MAX_SUBAGENT_DEPTH};
use tickloop_harness::runtime::Agent;
use tickloop_harness::test_support::{ScriptedModel, call, config, text};

#[derive(Default)]
struct MutatingHost(std::sync::atomic::AtomicUsize);

#[async_trait::async_trait]
impl tickloop_harness::runtime::host_tools::HostToolProvider for MutatingHost {
    fn definitions(&self) -> Vec<tickloop_harness::protocol::ToolDefinition> {
        vec![tickloop_harness::protocol::ToolDefinition {
            name: "MutateHost".into(),
            description: "Mutate a test host".into(),
            input_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
        }]
    }
    fn host_endpoint(
        &self,
    ) -> Result<Option<tickloop_harness::protocol::HostEndpointIdentity>, String> {
        Ok(Some(
            tickloop_harness::protocol::HostEndpointIdentity::TrustedAdapter {
                scope: uuid::Uuid::nil(),
            },
        ))
    }
    fn permission(
        &self,
        _: tickloop_harness::protocol::ChatId,
        _: &str,
        _: &serde_json::Value,
    ) -> tickloop_harness::runtime::host_tools::HostToolPermission {
        tickloop_harness::runtime::host_tools::HostToolPermission::Reviewable
    }
    async fn execute(
        &self,
        _: tickloop_harness::protocol::ChatId,
        _: &str,
        _: serde_json::Value,
        _: tokio_util::sync::CancellationToken,
    ) -> tickloop_harness::protocol::ToolResult {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        tickloop_harness::protocol::ToolResult {
            content: "mutated".into(),
            is_error: false,
            output_ref: None,
        }
    }
}

#[tokio::test]
async fn root_plan_mode_blocks_an_existing_grandchilds_host_mutation() {
    use tickloop_harness::protocol::{ExecutionMode, QuestionAnswer};
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call("a", "Subagent", json!({"task":"delegate", "name":"a"})),
        call(
            "b",
            "Subagent",
            json!({"task":"check then mutate", "name":"b"}),
        ),
        call(
            "question",
            "AskUserQuestion",
            tickloop_harness::test_support::single_question("Continue?"),
        ),
        call("mutation", "MutateHost", json!({})),
        text("denied"),
        text("parent done"),
        text("root done"),
    ]));
    let core = Agent::new_embedded(config(), model);
    let host = Arc::new(MutatingHost::default());
    core.install_host_tools(host.clone()).unwrap();
    let root = core.create_chat(directory.path().into()).await.unwrap();
    core.set_execution_mode(root, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let mut events = core.subscribe();
    core.submit(root, "delegate".into()).await.unwrap();
    let (leaf, question) = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = events.recv().await.unwrap();
            if let EventKind::QuestionAsked { request } = event.kind {
                break (event.chat_id, request);
            }
        }
    })
    .await
    .unwrap();
    core.set_execution_mode(root, ExecutionMode::Plan)
        .await
        .unwrap();
    core.answer_question(leaf, question.id, vec![QuestionAnswer::option("Weekly")])
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = events.recv().await.unwrap();
            if let EventKind::ToolCompleted {
                call_id, result, ..
            } = event.kind
                && call_id == "mutation"
            {
                assert!(result.is_error);
                break;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(host.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    core.shutdown().await;
}

fn copy_test_store(source: &std::path::Path, destination: &std::path::Path) {
    std::fs::create_dir_all(destination).unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_test_store(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

#[tokio::test]
async fn unfinished_restored_child_requires_resolution_and_explicit_stop_resolves_it() {
    let store = tempfile::tempdir().unwrap();
    let copied = tempfile::tempdir().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let core =
        Agent::open_embedded(store.path().into(), config(), background_family_model()).unwrap();
    let root = core.create_chat(directory.path().into()).await.unwrap();
    let mut events = core.subscribe();
    core.submit(root, "start".into()).await.unwrap();
    let leaf = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = events.recv().await.unwrap();
            if matches!(event.kind, EventKind::QuestionAsked { .. }) {
                break event.chat_id;
            }
        }
    })
    .await
    .unwrap();
    let parent = core
        .snapshot(leaf)
        .await
        .unwrap()
        .delegation
        .unwrap()
        .parent_chat_id;
    core.flush_transcripts().await;
    copy_test_store(store.path(), copied.path());
    core.shutdown().await;
    let model = Arc::new(tickloop_harness::test_support::routed::RoutedModel::new(
        |_| text("completion"),
    ));
    let restored = Agent::open_embedded(copied.path().into(), config(), model).unwrap();
    restored.resume_chat(leaf).await.unwrap();
    let mut events = restored.subscribe();
    let attempt = restored.submit(parent, "Continue".into()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.run_id == attempt.run_id {
                match event.kind {
                    EventKind::Failed(error) => {
                        assert!(error.contains("without a live executor"), "{error}");
                        break;
                    }
                    EventKind::Completed { .. } => {
                        panic!("an unresolved restored child cannot be silently ignored")
                    }
                    _ => {}
                }
            }
        }
    })
    .await
    .unwrap();
    restored.stop(leaf).await.unwrap();
    assert!(
        restored
            .snapshot(parent)
            .await
            .unwrap()
            .family_completions
            .iter()
            .any(|completion| completion.child_chat_id == leaf
                && completion.state == tickloop_harness::protocol::LoopState::Interrupted)
    );
    let continued = restored
        .submit(parent, "Continue after stopping the child".into())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.run_id == continued.run_id {
                match event.kind {
                    EventKind::Failed(error) => panic!("{error}"),
                    EventKind::Completed { .. } => break,
                    _ => {}
                }
            }
        }
    })
    .await
    .unwrap();
    restored.shutdown().await;
}

#[tokio::test]
async fn grandparent_denial_wins_over_a_pending_grandchild_approval() {
    use tickloop_harness::protocol::{Decision, ExecutionMode, PermissionRule};
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call("a", "Subagent", json!({"task":"delegate", "name":"a"})),
        call("b", "Subagent", json!({"task":"write report", "name":"b"})),
        call(
            "write",
            "Write",
            json!({"file_path":"blocked.txt","content":"must not be written"}),
        ),
        text("blocked"),
        text("A done"),
        text("root done"),
    ]));
    let core = Agent::new_embedded(config(), model);
    let root = core.create_chat(directory.path().into()).await.unwrap();
    core.set_execution_mode(root, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let rule = |decision| PermissionRule {
        id: uuid::Uuid::new_v4(),
        decision,
        tool: "Write".into(),
        specifier: None,
        input: None,
        loop_id: None,
        persistent: false,
    };
    core.set_permission_rules(root, vec![rule(Decision::Ask)])
        .await
        .unwrap();
    let mut events = core.subscribe();
    core.submit(root, "delegate with an approval".into())
        .await
        .unwrap();
    let (leaf, permission) = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = events.recv().await.unwrap();
            if let EventKind::ToolCompleted {
                call_id, result, ..
            } = &event.kind
            {
                assert!(!result.is_error, "{call_id}: {}", result.content);
            }
            if let EventKind::Failed(error) = &event.kind {
                panic!("{error}");
            }
            if let EventKind::PermissionRequested { request } = event.kind {
                break (event.chat_id, request);
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        core.snapshot(leaf)
            .await
            .unwrap()
            .delegation
            .unwrap()
            .depth(),
        2
    );
    core.set_permission_rules(root, vec![rule(Decision::Deny)])
        .await
        .unwrap();
    core.answer_permission(leaf, permission.id, true)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.chat_id == leaf {
                match &event.kind {
                    EventKind::ToolStarted { call_id, .. } if call_id == "write" => {
                        panic!("grandparent denial must prevent dispatch")
                    }
                    EventKind::Failed(error) => {
                        assert!(
                            error.contains("permission") || error.contains("deni"),
                            "{error}"
                        );
                        break;
                    }
                    _ => {}
                }
            }
            if let EventKind::ToolCompleted {
                call_id, result, ..
            } = event.kind
                && call_id == "write"
            {
                assert!(result.is_error);
                break;
            }
        }
    })
    .await
    .unwrap();
    assert!(!directory.path().join("blocked.txt").exists());
    core.shutdown().await;
}

fn background_family_model() -> Arc<tickloop_harness::test_support::routed::RoutedModel> {
    use tickloop_harness::test_support::{
        routed::{has_tool_result, state},
        single_question,
    };
    Arc::new(tickloop_harness::test_support::routed::RoutedModel::new(
        |request| match state(request)["delegation"]["task"].as_str() {
            Some("parent") if !has_tool_result(request, "b") => call(
                "b",
                "Subagent",
                json!({"task":"leaf","name":"b","run_in_background":true}),
            ),
            Some("parent") => text("parent report"),
            Some("leaf") if !has_tool_result(request, "question") => call(
                "question",
                "AskUserQuestion",
                single_question("Wait for the user"),
            ),
            Some("leaf") => text("leaf report"),
            _ if !has_tool_result(request, "a") => {
                call("a", "Subagent", json!({"task":"parent","name":"a"}))
            }
            _ => text("root report"),
        },
    ))
}

#[tokio::test]
async fn delegated_parent_waits_for_its_background_child_without_model_polling() {
    use tickloop_harness::protocol::QuestionAnswer;
    let directory = tempfile::tempdir().unwrap();
    let model = background_family_model();
    let core = Agent::new_embedded(config(), model.clone());
    let root = core.create_chat(directory.path().into()).await.unwrap();
    let mut events = core.subscribe();
    core.submit(root, "start".into()).await.unwrap();
    let (leaf, question) = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = events.recv().await.unwrap();
            assert!(!(event.chat_id == root && matches!(event.kind, EventKind::Completed { .. })));
            if let EventKind::QuestionAsked { request } = event.kind {
                break (event.chat_id, request);
            }
        }
    })
    .await
    .unwrap();
    let parent = core
        .snapshot(leaf)
        .await
        .unwrap()
        .delegation
        .unwrap()
        .parent_chat_id;
    assert!(
        core.snapshot(parent)
            .await
            .unwrap()
            .family_completions
            .is_empty()
    );
    core.answer_question(leaf, question.id, vec![QuestionAnswer::option("Weekly")])
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.chat_id == root && matches!(event.kind, EventKind::Completed { .. }) {
                break;
            }
        }
    })
    .await
    .unwrap();
    {
        let requests = model.requests.lock().unwrap();
        assert!(
            requests.len() <= 7,
            "waiting must not poll the model: {}",
            requests.len()
        );
    }
    assert_eq!(
        core.snapshot(root).await.unwrap().family_completions.len(),
        1
    );
    core.shutdown().await;
}

#[tokio::test]
async fn stopping_a_member_cancels_its_waiting_descendants() {
    use tickloop_harness::protocol::LoopState;
    let directory = tempfile::tempdir().unwrap();
    let core = Agent::new_embedded(config(), background_family_model());
    let root = core.create_chat(directory.path().into()).await.unwrap();
    let mut events = core.subscribe();
    core.submit(root, "start".into()).await.unwrap();
    let leaf = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = events.recv().await.unwrap();
            if matches!(event.kind, EventKind::QuestionAsked { .. }) {
                break event.chat_id;
            }
        }
    })
    .await
    .unwrap();
    let parent = core
        .snapshot(leaf)
        .await
        .unwrap()
        .delegation
        .unwrap()
        .parent_chat_id;
    core.stop(parent).await.unwrap();
    for member in [parent, leaf] {
        tokio::time::timeout(Duration::from_secs(5), async {
            while matches!(
                core.snapshot(member).await.unwrap().chat_loop.state,
                LoopState::Running
                    | LoopState::Queued
                    | LoopState::WaitingPermission
                    | LoopState::WaitingQuestion
                    | LoopState::WaitingPlan
            ) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the stopped member's turn ends");
        assert!(core.snapshot(member).await.unwrap().questions.is_empty());
    }
    core.shutdown().await;
}

#[tokio::test]
async fn three_levels_keep_direct_parents_and_one_flat_family() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call("a", "Subagent", json!({"task":"delegate to b", "name":"a"})),
        call("b", "Subagent", json!({"task":"delegate to c", "name":"b"})),
        call("c", "Subagent", json!({"task":"report", "name":"c"})),
        text("C report"),
        text("B report"),
        text("A report"),
        text("root report"),
    ]));
    let core = Agent::new_embedded(config(), model.clone());
    let root = core.create_chat(directory.path().into()).await.unwrap();
    let mut events = core.subscribe();
    let run = core
        .submit(root, "Delegate three levels".into())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.run_id == run.run_id && matches!(event.kind, EventKind::Completed { .. }) {
                break;
            }
        }
    })
    .await
    .unwrap();
    let a = core.subagents(root).await.unwrap()[0].child_chat_id;
    let b = core.subagents(a).await.unwrap()[0].child_chat_id;
    let c = core.subagents(b).await.unwrap()[0].child_chat_id;
    let shared_board = core.snapshot(root).await.unwrap().task_board.unwrap();
    assert_eq!(shared_board.root_loop_id, run.loop_id);
    for member in [root, a, b, c] {
        assert_eq!(
            core.snapshot(member).await.unwrap().task_board,
            Some(shared_board.clone())
        );
        assert_eq!(
            core.status(member).await.unwrap().task_board,
            Some(shared_board.clone())
        );
        let family = core.family_agents(member).await.unwrap();
        assert_eq!(family.len(), 4);
        assert!(
            family
                .iter()
                .any(|row| row.chat_id == root && row.handle == "main")
        );
    }
    let leaf = core.snapshot(c).await.unwrap();
    let delegation = leaf.delegation.as_ref().unwrap();
    assert_eq!(delegation.depth(), MAX_SUBAGENT_DEPTH);
    assert_eq!(delegation.parent_chat_id, b);
    assert_eq!(delegation.root_chat_id(), root);
    {
        let requests = model.requests.lock().unwrap();
        let leaf_request = requests
            .iter()
            .find(|request| {
                tickloop_harness::test_support::routed::state(request)["delegation"]["task"]
                    == "report"
            })
            .unwrap();
        assert!(
            !leaf_request
                .tools
                .iter()
                .any(|tool| tool.name == "Subagent")
        );
    }
    assert_eq!(
        core.snapshot(root).await.unwrap().family_completions.len(),
        1
    );
    core.shutdown().await;
}

#[tokio::test]
async fn restoring_only_a_grandchild_loads_all_ancestors_without_running_models() {
    let store = tempfile::tempdir().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call("a", "Subagent", json!({"task":"delegate", "name":"a"})),
        call("b", "Subagent", json!({"task":"report", "name":"b"})),
        text("B done"),
        text("A done"),
        text("root done"),
    ]));
    let core = Agent::open_embedded(store.path().into(), config(), model).unwrap();
    let root = core.create_chat(directory.path().into()).await.unwrap();
    let mut events = core.subscribe();
    let run = core.submit(root, "delegate".into()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.run_id == run.run_id && matches!(event.kind, EventKind::Completed { .. }) {
                break;
            }
        }
    })
    .await
    .unwrap();
    let parent = core.subagents(root).await.unwrap()[0].child_chat_id;
    let leaf = core.subagents(parent).await.unwrap()[0].child_chat_id;
    core.flush_transcripts().await;
    core.shutdown().await;
    let restored_model = Arc::new(ScriptedModel::new(vec![]));
    let restored =
        Agent::open_embedded(store.path().into(), config(), restored_model.clone()).unwrap();
    let snapshot = restored.resume_chat(leaf).await.unwrap();
    assert_eq!(snapshot.delegation.unwrap().root_chat_id(), root);
    assert!(restored.snapshot(parent).await.is_ok());
    assert!(restored.snapshot(root).await.is_ok());
    assert_eq!(restored.family_agents(leaf).await.unwrap().len(), 3);
    assert!(restored_model.requests.lock().unwrap().is_empty());
    restored.shutdown().await;
}

#[tokio::test]
async fn family_handles_remain_unique_across_delegating_parents() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "a",
            "Subagent",
            json!({"task":"delegate another helper", "name":"helper"}),
        ),
        call("b", "Subagent", json!({"task":"report", "name":"helper"})),
        text("nested report"),
        text("parent report"),
        text("root report"),
    ]));
    let core = Agent::new_embedded(config(), model);
    let root = core.create_chat(directory.path().into()).await.unwrap();
    let mut events = core.subscribe();
    let run = core.submit(root, "Delegate".into()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.run_id == run.run_id && matches!(event.kind, EventKind::Completed { .. }) {
                break;
            }
        }
    })
    .await
    .unwrap();
    let names = core
        .family_registrations(root)
        .await
        .unwrap()
        .into_iter()
        .map(|member| member.handle)
        .collect::<Vec<_>>();
    assert_eq!(names, ["helper", "helper-2"]);
    core.shutdown().await;
}

async fn stopped_subtree_notifications(stop_root: bool) {
    use tickloop_harness::protocol::{Content, LoopState};
    use tickloop_harness::test_support::routed::{RoutedModel, has_tool_result, state};
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(RoutedModel::new(
        |request| match state(request)["delegation"]["task"].as_str() {
            Some("parent") if !has_tool_result(request, "leaf") => call(
                "leaf",
                "Subagent",
                json!({"task":"leaf", "name":"leaf", "run_in_background":true}),
            ),
            Some("parent") => text("parent partial report"),
            Some("leaf") if !has_tool_result(request, "question") => call(
                "question",
                "AskUserQuestion",
                tickloop_harness::test_support::single_question("Wait"),
            ),
            Some("leaf") => text("leaf report"),
            _ if !has_tool_result(request, "parent") => call(
                "parent",
                "Subagent",
                json!({"task":"parent", "name":"middle", "run_in_background":true}),
            ),
            _ if serde_json::to_string(&request.messages)
                .unwrap()
                .contains("STOP-MIDDLE")
                && !has_tool_result(request, "stop-middle") =>
            {
                call("stop-middle", "StopSubagent", json!({"to":"middle"}))
            }
            _ => text("root report"),
        },
    ));
    let core = Agent::new_embedded(config(), model.clone());
    let root = core.create_chat(directory.path().into()).await.unwrap();
    let mut events = core.subscribe();
    let initial = core.submit(root, "start".into()).await.unwrap();
    let leaf = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = events.recv().await.unwrap();
            if matches!(event.kind, EventKind::QuestionAsked { .. }) {
                break event.chat_id;
            }
        }
    })
    .await
    .unwrap();
    let parent = core
        .snapshot(leaf)
        .await
        .unwrap()
        .delegation
        .unwrap()
        .parent_chat_id;
    tokio::time::timeout(Duration::from_secs(10), async {
        while core.snapshot(root).await.unwrap().chat_loop.state != LoopState::Idle {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    if stop_root {
        core.stop(root).await.unwrap();
    } else {
        core.submit(root, "STOP-MIDDLE".into()).await.unwrap();
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let middle = core.snapshot(parent).await.unwrap();
            let top = core.snapshot(root).await.unwrap();
            if middle.chat_loop.state == LoopState::Interrupted
                && middle.family_completions.len() == 1
                && middle
                    .pending_inputs
                    .iter()
                    .any(|pending| pending.agent_message.is_some() && !pending.armed)
                && top.family_completions.len() == 1
                && (stop_root || top.pending_inputs.is_empty())
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let has_notification = |snapshot: &tickloop_harness::protocol::ChatSnapshot| {
        snapshot.messages.iter().flat_map(|message| &message.content).any(|content| {
            matches!(content, Content::Text { text } if text.starts_with("<task-notification>"))
        })
    };
    let stopped = core.snapshot(parent).await.unwrap();
    assert!(!has_notification(&stopped));
    assert!(stopped.family_completions[0].result.is_empty());
    let top = core.snapshot(root).await.unwrap();
    assert_eq!(has_notification(&top), !stop_root);
    if stop_root {
        assert_eq!(top.chat_loop.run_id, Some(initial.run_id));
        assert!(
            top.pending_inputs
                .iter()
                .any(|pending| pending.agent_message.is_some() && !pending.armed)
        );
        core.submit(root, "ROOT-CONTINUE".into()).await.unwrap();
    }
    core.continue_loop(parent).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if has_notification(&core.snapshot(parent).await.unwrap())
                && has_notification(&core.snapshot(root).await.unwrap())
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    if stop_root {
        let resumed = core.snapshot(root).await.unwrap();
        let human = resumed
            .messages
            .iter()
            .position(|message| {
                message.content.iter().any(
                    |content| matches!(content, Content::Text { text } if text == "ROOT-CONTINUE"),
                )
            })
            .unwrap();
        let notice = resumed.messages.iter().position(|message| message.content.iter().any(|content| matches!(content, Content::Text { text } if text.starts_with("<task-notification>")))).unwrap();
        assert!(human < notice, "user input precedes completion context");
    }
    assert_eq!(
        core.snapshot(leaf).await.unwrap().chat_loop.state,
        LoopState::Interrupted
    );
    assert!(core.snapshot(leaf).await.unwrap().questions.is_empty());
    core.shutdown().await;
}

#[tokio::test]
async fn root_stop_holds_descendant_notifications_until_continue() {
    stopped_subtree_notifications(true).await;
}

#[tokio::test]
async fn stop_subagent_holds_internal_notifications_and_delivers_outside_the_subtree() {
    stopped_subtree_notifications(false).await;
}
