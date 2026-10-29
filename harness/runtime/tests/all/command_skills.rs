use serde_json::json;
use std::{path::Path, sync::Arc, time::Duration};
use tickloop_harness_policy::{
    settings::{
        MemorySettingsStore, PermissionChanges, SettingsSnapshot, SettingsStore,
        SettingsStoreError, SettingsUpdate,
    },
    skills::{CommandDocument, SkillRegistry},
};
use tickloop_harness_protocol::{Decision, EventKind, ExecutionMode, PermissionRule, SkillSource};
use tickloop_harness_runtime::{
    Agent, HarnessOptions, HarnessSeams, environment::memory::MemoryEnvironment,
};
use tickloop_harness_test_support::{
    ScriptedModel, call, config, harness::ChosenModels, next_event, text,
};

struct CommandSettings {
    memory: MemorySettingsStore,
    markdown: String,
}
impl SettingsStore for CommandSettings {
    fn read(&self, folder: Option<&Path>) -> SettingsSnapshot {
        self.memory.read(folder)
    }
    fn write(&self, update: SettingsUpdate) -> Result<(), SettingsStoreError> {
        self.memory.write(update)
    }
    fn write_permissions(
        &self,
        changes: &PermissionChanges,
        expected: Option<(&Path, &str)>,
    ) -> Result<(), SettingsStoreError> {
        self.memory.write_permissions(changes, expected)
    }
    fn skills(&self, _: Option<&Path>) -> SkillRegistry {
        let mut skills = SkillRegistry::default();
        skills.register_command(
            CommandDocument {
                name: "check".into(),
                markdown: self.markdown.clone(),
                source: SkillSource::Plugin {
                    plugin: "demo".into(),
                    path: "/plugin/commands/check.md".into(),
                },
                base_directory: "/plugin/commands".into(),
                metadata: Default::default(),
                public_config: Default::default(),
            },
            &[],
        );
        skills
    }
}

fn command_agent(
    environment: MemoryEnvironment,
    model: Arc<dyn tickloop_harness_model::ModelClient>,
    markdown: &str,
) -> Agent {
    Agent::with_seams(
        HarnessSeams {
            search: None,
            transcripts: None,
            environment: Some(Arc::new(environment)),
            models: ChosenModels::new(model),
        },
        HarnessOptions {
            settings: Some(Arc::new(CommandSettings {
                memory: Default::default(),
                markdown: markdown.into(),
            })),
            ..HarnessOptions::local(config())
        },
    )
    .unwrap()
}

#[tokio::test]
async fn command_shells_run_before_activation_through_both_invocation_paths() {
    for user in [true, false] {
        let folder = tempfile::tempdir().unwrap();
        let environment = MemoryEnvironment::new();
        environment.define_shell(
            "printf hello",
            Duration::ZERO,
            b"prepared-value".to_vec(),
            0,
        );
        let responses = if user {
            vec![text("done")]
        } else {
            vec![
                call(
                    "skill-call",
                    "Skill",
                    json!({"skill":"demo:check", "args":"hello"}),
                ),
                text("done"),
            ]
        };
        let model = Arc::new(ScriptedModel::new(responses));
        let agent = Agent::with_seams(HarnessSeams { search:None, transcripts:None, environment:Some(Arc::new(environment)), models:ChosenModels::new(model.clone()) }, HarnessOptions { settings:Some(Arc::new(CommandSettings { memory:Default::default(), markdown:"---\ndescription: Check\nallowed-tools: [Bash]\n---\nPrepared: !`printf $0`\n@README.md".into() })), ..HarnessOptions::local(config()) }).unwrap();
        let chat = agent.create_chat(folder.path().into()).await.unwrap();
        agent
            .set_execution_mode(chat, ExecutionMode::FullAccess)
            .await
            .unwrap();
        let mut events = agent.subscribe();
        let receipt = if user {
            agent
                .invoke_skill(chat, "demo:check", "hello")
                .await
                .unwrap()
        } else {
            agent
                .submit(chat, "Use demo:check hello".into())
                .await
                .unwrap()
        };
        loop {
            let event = next_event(&mut events).await;
            if event.run_id != receipt.run_id {
                continue;
            }
            match event.kind {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        let observation = agent.observe(chat, 0).await.unwrap();
        let skill = observation
            .snapshot
            .invoked_skills
            .iter()
            .find(|skill| skill.name == "demo:check")
            .unwrap();
        assert!(
            skill.content.contains("prepared-value"),
            "{}",
            skill.content
        );
        assert!(!skill.content.contains("!`printf"));
        assert!(skill.content.contains("@README.md"));
        let operations = observation
            .snapshot
            .operations
            .iter()
            .filter(|operation| operation.name == "Bash")
            .collect::<Vec<_>>();
        assert_eq!(operations.len(), 1);
        assert_eq!(operations[0].input["command"], "printf hello");
        assert_eq!(
            operations[0].parent_call_id.as_deref(),
            (!user).then_some("skill-call")
        );
        {
            let requests = model.requests.lock().unwrap();
            assert_eq!(requests.len(), if user { 1 } else { 2 });
            assert!(format!("{:?}", requests.last().unwrap()).contains("prepared-value"));
        }
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn denied_command_shells_never_activate_guidance_despite_allowed_tools() {
    for user in [true, false] {
        let folder = tempfile::tempdir().unwrap();
        let environment = MemoryEnvironment::new();
        environment.define_shell(
            "printf forbidden",
            Duration::ZERO,
            b"must-not-appear".to_vec(),
            0,
        );
        let model = Arc::new(ScriptedModel::new(if user {
            vec![]
        } else {
            vec![
                call("skill-call", "Skill", json!({"skill":"demo:check"})),
                text("denied"),
            ]
        }));
        let agent = Agent::with_seams(HarnessSeams { search:None, transcripts:None, environment:Some(Arc::new(environment)), models:ChosenModels::new(model.clone()) }, HarnessOptions { settings:Some(Arc::new(CommandSettings { memory:Default::default(), markdown:"---\ndescription: Check\nallowed-tools: [Bash]\n---\nSecret guidance !`printf forbidden`".into() })), ..HarnessOptions::local(config()) }).unwrap();
        let chat = agent.create_chat(folder.path().into()).await.unwrap();
        agent
            .set_execution_mode(chat, ExecutionMode::FullAccess)
            .await
            .unwrap();
        agent
            .set_permission_rules(
                chat,
                vec![PermissionRule {
                    id: tickloop_ids::generate(),
                    decision: Decision::Deny,
                    tool: "Bash".into(),
                    specifier: None,
                    input: None,
                    loop_id: None,
                    persistent: false,
                }],
            )
            .await
            .unwrap();
        let mut events = agent.subscribe();
        let receipt = if user {
            agent.invoke_skill(chat, "demo:check", "").await.unwrap()
        } else {
            agent.submit(chat, "Use command".into()).await.unwrap()
        };
        loop {
            let event = next_event(&mut events).await;
            if event.run_id != receipt.run_id {
                continue;
            }
            if matches!(
                event.kind,
                EventKind::Completed { .. } | EventKind::Failed(_)
            ) {
                break;
            }
        }
        let observation = agent.observe(chat, 0).await.unwrap();
        assert!(observation.snapshot.invoked_skills.is_empty());
        assert!(!format!("{:?}", observation.snapshot.messages).contains("must-not-appear"));
        if user {
            assert!(model.requests.lock().unwrap().is_empty());
        }
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn command_shell_approval_uses_ask_auto_and_plan_without_metadata_grants() {
    for (mode, block) in [
        (ExecutionMode::AcceptEdits, false),
        (ExecutionMode::AcceptEdits, true),
        (ExecutionMode::Auto, false),
        (ExecutionMode::Auto, true),
        (ExecutionMode::Plan, true),
    ] {
        let mut ordinary_routing = None;
        for caller in ["bash", "user", "model"] {
            let folder = tempfile::tempdir().unwrap();
            let environment = MemoryEnvironment::new();
            environment.set_shell_approval_required(true);
            environment.define_shell(
                "touch marker",
                Duration::ZERO,
                b"approved-output".to_vec(),
                0,
            );
            let responses = match caller {
                "user" => vec![text("done")],
                "model" => vec![
                    call("skill", "Skill", json!({"skill":"demo:check"})),
                    text("done"),
                ],
                _ => vec![
                    call(
                        "ordinary",
                        "Bash",
                        json!({"command":"touch marker","run_in_background":false}),
                    ),
                    text("done"),
                ],
            };
            let model = Arc::new(ScriptedModel::new(responses));
            let reviewer = Arc::new(ScriptedModel::new(vec![
                text(
                    &tickloop_harness_test_support::auto_classification(block).to_string()
                );
                3
            ]));
            let agent = command_agent(
                environment,
                model,
                "---\ndescription: Check\nallowed-tools: [Bash]\n---\n!`touch marker`",
            );
            agent.set_auto_reviewer(reviewer.clone()).await;
            let chat = agent.create_chat(folder.path().into()).await.unwrap();
            agent.set_execution_mode(chat, mode).await.unwrap();
            let mut events = agent.subscribe();
            let receipt = if caller == "user" {
                agent.invoke_skill(chat, "demo:check", "").await.unwrap()
            } else {
                agent
                    .submit(chat, "Run the local command".into())
                    .await
                    .unwrap()
            };
            let mut asked = false;
            loop {
                let event = next_event(&mut events).await;
                if event.run_id != receipt.run_id {
                    continue;
                }
                match event.kind {
                    EventKind::PermissionRequested { request } => {
                        asked = true;
                        agent
                            .answer_permission(chat, request.id, !block)
                            .await
                            .unwrap();
                    }
                    EventKind::Completed { .. } | EventKind::Failed(_) | EventKind::Interrupted => {
                        break;
                    }
                    _ => {}
                }
            }
            let snapshot = agent.snapshot(chat).await.unwrap();
            if caller != "bash" {
                assert_eq!(
                    !snapshot.invoked_skills.is_empty(),
                    !block,
                    "caller={caller}, mode={mode:?}, block={block}"
                );
            }
            let routing = (asked, !reviewer.requests.lock().unwrap().is_empty());
            if let Some(expected) = ordinary_routing {
                assert_eq!(
                    routing, expected,
                    "caller={caller}, mode={mode:?}, block={block}"
                );
            } else {
                ordinary_routing = Some(routing);
            }
            assert_eq!(routing.1, mode == ExecutionMode::Auto);
            agent.shutdown().await;
        }
    }
}

#[tokio::test]
async fn command_unknown_outcome_stops_the_remaining_blocks_without_resending() {
    for user in [true, false] {
        let folder = tempfile::tempdir().unwrap();
        let environment = MemoryEnvironment::new();
        environment.define_shell("printf first", Duration::ZERO, b"first".to_vec(), 0);
        environment.define_shell("printf second", Duration::ZERO, b"second".to_vec(), 0);
        environment.lose_next_reply();
        let model = Arc::new(ScriptedModel::new(if user {
            vec![]
        } else {
            vec![
                call("skill", "Skill", json!({"skill":"demo:check"})),
                text("unknown"),
            ]
        }));
        let agent = command_agent(
            environment,
            model,
            "---\ndescription: Check\n---\n!`printf first`\n!`printf second`",
        );
        let chat = agent.create_chat(folder.path().into()).await.unwrap();
        agent
            .set_execution_mode(chat, ExecutionMode::FullAccess)
            .await
            .unwrap();
        let mut events = agent.subscribe();
        let receipt = if user {
            agent.invoke_skill(chat, "demo:check", "").await.unwrap()
        } else {
            agent.submit(chat, "Run command".into()).await.unwrap()
        };
        loop {
            let event = next_event(&mut events).await;
            if event.run_id == receipt.run_id
                && matches!(
                    event.kind,
                    EventKind::Completed { .. } | EventKind::Failed(_)
                )
            {
                break;
            }
        }
        let snapshot = agent.snapshot(chat).await.unwrap();
        assert!(snapshot.invoked_skills.is_empty());
        let shells = snapshot
            .operations
            .iter()
            .filter(|operation| operation.name == "Bash")
            .collect::<Vec<_>>();
        assert_eq!(shells.len(), 1);
        assert_eq!(shells[0].input["command"], "printf first");
        assert_eq!(
            shells[0].state,
            tickloop_harness_protocol::OperationState::Unknown
        );
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn queued_command_waits_for_its_own_turn_and_cancelled_preparation_never_activates() {
    let folder = tempfile::tempdir().unwrap();
    let environment = MemoryEnvironment::new();
    environment.set_shell_approval_required(true);
    for value in ["first", "second"] {
        environment.define_shell(
            &format!("printf {value}"),
            Duration::ZERO,
            value.as_bytes().to_vec(),
            0,
        );
    }
    let model = Arc::new(ScriptedModel::new(vec![text("done")]));
    let agent = command_agent(
        environment,
        model.clone(),
        "---\ndescription: Check\n---\n!`printf $0`",
    );
    let chat = agent.create_chat(folder.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let first = agent
        .invoke_skill(chat, "demo:check", "first")
        .await
        .unwrap();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id == first.run_id
            && matches!(event.kind, EventKind::PermissionRequested { .. })
        {
            break;
        }
    }
    let second = agent
        .invoke_skill(chat, "demo:check", "second")
        .await
        .unwrap();
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(
        snapshot
            .pending_inputs
            .iter()
            .any(|input| input.receipt.run_id == second.run_id)
    );
    assert!(snapshot.invoked_skills.is_empty());
    assert_eq!(
        snapshot
            .operations
            .iter()
            .filter(|operation| operation.name == "Bash")
            .count(),
        1
    );
    agent.stop(chat).await.unwrap();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id == first.run_id && matches!(event.kind, EventKind::Interrupted) {
            break;
        }
    }
    assert!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .invoked_skills
            .is_empty()
    );
    agent.take_queued_inputs(chat, true).await.unwrap();
    let checkpoint = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(checkpoint) = agent.prepare_upgrade().await.unwrap() {
                break checkpoint;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let checkpoint = serde_json::to_value(checkpoint).unwrap();
    assert!(
        checkpoint["chats"][0]["user_skill_preparations"]
            .as_object()
            .unwrap()
            .is_empty()
    );
    assert!(model.requests.lock().unwrap().is_empty());
    agent.cancel_upgrade().await;
    agent.shutdown().await;
}

#[tokio::test]
async fn model_cannot_repeat_a_user_command_preparation_call_id() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use tickloop_harness_protocol::Content;
    use tickloop_harness_test_support::routed::RoutedModel;
    let folder = tempfile::tempdir().unwrap();
    let environment = MemoryEnvironment::new();
    environment.define_shell("printf once", Duration::ZERO, b"once".to_vec(), 0);
    let repeated = AtomicBool::new(false);
    let model = Arc::new(RoutedModel::new(move |request| {
        if repeated.swap(true, Ordering::SeqCst) {
            return text("done");
        }
        let id = request
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .find_map(|content| match content {
                Content::ToolUse { id, name, .. } if name == "Bash" => Some(id.clone()),
                _ => None,
            })
            .unwrap();
        call(&id, "Bash", json!({"command":"printf once"}))
    }));
    let agent = command_agent(
        environment,
        model,
        "---\ndescription: Check\n---\n!`printf once`",
    );
    let chat = agent.create_chat(folder.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let receipt = agent.invoke_skill(chat, "demo:check", "").await.unwrap();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id == receipt.run_id
            && matches!(
                event.kind,
                EventKind::Completed { .. } | EventKind::Failed(_)
            )
        {
            break;
        }
    }
    assert_eq!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .operations
            .iter()
            .filter(|operation| operation.name == "Bash")
            .count(),
        1
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn pending_command_survives_planned_upgrade_and_waits_for_its_queue_barrier() {
    let folder = tempfile::tempdir().unwrap();
    let markdown = "---\ndescription: Check\n---\n!`printf $0`";
    let agent = command_agent(
        MemoryEnvironment::new(),
        Arc::new(ScriptedModel::new(vec![])),
        markdown,
    );
    let chat = agent.create_chat(folder.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let barrier = agent.queue_command(chat, "/help".into()).await.unwrap();
    let queued = agent
        .invoke_skill(chat, "demo:check", "second")
        .await
        .unwrap();
    assert!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .pending_inputs
            .iter()
            .any(|input| input.receipt.run_id == queued.run_id)
    );
    let checkpoint = agent.prepare_upgrade().await.unwrap().unwrap();
    let environment = MemoryEnvironment::new();
    environment.define_shell(
        "printf second",
        Duration::ZERO,
        b"after-upgrade".to_vec(),
        0,
    );
    let model = Arc::new(ScriptedModel::new(vec![text("done")]));
    let successor = command_agent(environment, model.clone(), markdown);
    successor.restore_upgrade(checkpoint).await.unwrap();
    successor.activate_upgrade().await.unwrap();
    successor.release_upgrade().await.unwrap();
    assert!(model.requests.lock().unwrap().is_empty());
    let mut events = successor.subscribe();
    assert_eq!(
        successor
            .take_next_queued_command(chat)
            .await
            .unwrap()
            .unwrap()
            .id,
        barrier
    );
    successor
        .finish_queued_command(chat, barrier)
        .await
        .unwrap();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id == queued.run_id {
            match event.kind {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
    }
    let snapshot = successor.snapshot(chat).await.unwrap();
    assert!(
        snapshot
            .invoked_skills
            .iter()
            .any(|skill| skill.content.contains("after-upgrade"))
    );
    assert_eq!(
        snapshot
            .operations
            .iter()
            .filter(|operation| operation.name == "Bash")
            .count(),
        1
    );
    agent.cancel_upgrade().await;
    agent.shutdown().await;
    successor.shutdown().await;
}

#[tokio::test]
async fn cold_restore_keeps_preparation_records_without_resending_shells() {
    use tickloop_harness_runtime::transcript::StorePosition;
    use tickloop_harness_test_support::transcripts::InMemoryTranscriptStore;
    let folder = tempfile::tempdir().unwrap();
    let transcripts = Arc::new(InMemoryTranscriptStore::new());
    let make = |store: Arc<InMemoryTranscriptStore>, model: Arc<ScriptedModel>| {
        let environment = MemoryEnvironment::new();
        environment.define_shell("printf first", Duration::ZERO, b"first".to_vec(), 0);
        environment.define_shell("touch second", Duration::ZERO, b"second".to_vec(), 0);
        Agent::with_seams(
            HarnessSeams {
                search: None,
                transcripts: Some(store),
                environment: Some(Arc::new(environment)),
                models: ChosenModels::new(model),
            },
            HarnessOptions {
                settings: Some(Arc::new(CommandSettings {
                    memory: Default::default(),
                    markdown: "---\ndescription: Check\n---\n!`printf first`\n!`touch second`"
                        .into(),
                })),
                ..HarnessOptions::local(config())
            },
        )
        .unwrap()
    };
    let first = make(transcripts.clone(), Arc::new(ScriptedModel::new(vec![])));
    let chat = first.create_chat(folder.path().into()).await.unwrap();
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
                tool: "Bash".into(),
                specifier: Some("touch second".into()),
                input: None,
                loop_id: None,
                persistent: false,
            }],
        )
        .await
        .unwrap();
    let mut events = first.subscribe();
    first.invoke_skill(chat, "demo:check", "").await.unwrap();
    loop {
        if matches!(
            next_event(&mut events).await.kind,
            EventKind::PermissionRequested { .. }
        ) {
            break;
        }
    }
    first.sync().await.unwrap();
    let crashed = Arc::new(InMemoryTranscriptStore::new());
    crashed.seed(chat, transcripts.lines(chat), StorePosition(1));
    let model = Arc::new(ScriptedModel::new(vec![text("No retry")]));
    let restored = make(crashed, model.clone());
    let snapshot = restored.resume_chat(chat).await.unwrap();
    assert!(model.requests.lock().unwrap().is_empty());
    assert!(snapshot.invoked_skills.is_empty());
    let bash_history = |snapshot: &tickloop_harness_protocol::ChatSnapshot| {
        snapshot
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|content| match content {
                tickloop_harness_protocol::Content::ToolUse { name, input, .. }
                    if name == "Bash" =>
                {
                    input["command"].as_str().map(str::to_owned)
                }
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(bash_history(&snapshot), ["printf first"]);
    assert!(format!("{:?}", snapshot.messages).contains("first"));
    assert!(
        snapshot.operations.is_empty(),
        "ordinary operations are live state, not replayed history"
    );
    let mut events = restored.subscribe();
    let receipt = restored
        .submit(chat, "Continue without retrying the command".into())
        .await
        .unwrap();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id == receipt.run_id && matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
    }
    assert_eq!(
        restored
            .snapshot(chat)
            .await
            .unwrap()
            .operations
            .iter()
            .filter(|operation| operation.name == "Bash")
            .count(),
        0
    );
    assert_eq!(
        bash_history(&restored.snapshot(chat).await.unwrap()),
        ["printf first"]
    );
    first.shutdown().await;
    restored.shutdown().await;
}

#[tokio::test]
async fn old_checkpoint_retains_queued_guidance_without_rebuilding_an_execution_plan() {
    use tickloop_harness_runtime::upgrade::AgentUpgradeCheckpoint;
    for legacy in [true, false] {
        let folder = tempfile::tempdir().unwrap();
        let markdown = "---\ndescription: Check\n---\nLegacy guidance";
        let agent = command_agent(
            MemoryEnvironment::new(),
            Arc::new(ScriptedModel::new(vec![])),
            markdown,
        );
        let chat = agent.create_chat(folder.path().into()).await.unwrap();
        let barrier = agent.queue_command(chat, "/help".into()).await.unwrap();
        let queued = agent.invoke_skill(chat, "demo:check", "").await.unwrap();
        let checkpoint = agent.prepare_upgrade().await.unwrap().unwrap();
        let settings = CommandSettings {
            memory: Default::default(),
            markdown: markdown.into(),
        };
        let activated = tickloop_harness_protocol::InvokedSkill::try_from(
            settings
                .skills(Some(folder.path()))
                .expand(
                    "demo:check",
                    "",
                    tickloop_harness_policy::skills::SkillCaller::User,
                )
                .unwrap(),
        )
        .unwrap();
        let mut checkpoint = serde_json::to_value(checkpoint).unwrap();
        checkpoint["chats"][0]["snapshot"]["history"]["invoked_skills"] = json!([activated]);
        if legacy {
            checkpoint["chats"][0]
                .as_object_mut()
                .unwrap()
                .remove("user_skill_preparations");
        } else {
            checkpoint["chats"][0]["user_skill_preparations"] = json!({});
        }
        let checkpoint: AgentUpgradeCheckpoint = serde_json::from_value(checkpoint).unwrap();
        let model = Arc::new(ScriptedModel::new(vec![text("done")]));
        let successor = command_agent(MemoryEnvironment::new(), model.clone(), markdown);
        successor.restore_upgrade(checkpoint).await.unwrap();
        successor.activate_upgrade().await.unwrap();
        successor.release_upgrade().await.unwrap();
        let mut events = successor.subscribe();
        successor
            .take_next_queued_command(chat)
            .await
            .unwrap()
            .unwrap();
        successor
            .finish_queued_command(chat, barrier)
            .await
            .unwrap();
        loop {
            let event = next_event(&mut events).await;
            if event.run_id == queued.run_id {
                match event.kind {
                    EventKind::Completed { .. } => {
                        assert!(legacy);
                        break;
                    }
                    EventKind::Failed(error) => {
                        assert!(!legacy, "{error}");
                        break;
                    }
                    _ => {}
                }
            }
        }
        assert_eq!(model.requests.lock().unwrap().len(), usize::from(legacy));
        assert!(
            successor
                .snapshot(chat)
                .await
                .unwrap()
                .operations
                .is_empty()
        );
        agent.cancel_upgrade().await;
        agent.shutdown().await;
        successor.shutdown().await;
    }
}

#[tokio::test]
async fn plain_command_guidance_keeps_the_existing_active_turn_merge_behavior() {
    let folder = tempfile::tempdir().unwrap();
    let environment = MemoryEnvironment::new();
    environment.set_shell_approval_required(true);
    environment.define_shell("touch hold", Duration::ZERO, b"held-output".to_vec(), 0);
    let model = Arc::new(ScriptedModel::new(vec![
        call("hold", "Bash", json!({"command":"touch hold"})),
        text("done"),
    ]));
    let agent = command_agent(
        environment,
        model.clone(),
        "---\ndescription: Check\n---\nPlain command guidance",
    );
    let chat = agent.create_chat(folder.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let active = agent.submit(chat, "Start work".into()).await.unwrap();
    let permission = loop {
        if let EventKind::PermissionRequested { request } = next_event(&mut events).await.kind {
            break request;
        }
    };
    let command = agent.invoke_skill(chat, "demo:check", "").await.unwrap();
    assert!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .invoked_skills
            .iter()
            .any(|skill| skill.content.contains("Plain command guidance"))
    );
    agent
        .answer_permission(chat, permission.id, true)
        .await
        .unwrap();
    let mut merged = false;
    loop {
        let event = next_event(&mut events).await;
        match event.kind {
            EventKind::InputMerged { into_run_id } if event.run_id == command.run_id => {
                assert_eq!(into_run_id, active.run_id);
                merged = true;
            }
            EventKind::Completed { .. } if event.run_id == active.run_id => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert!(merged);
    assert_eq!(model.requests.lock().unwrap().len(), 2);
    agent.shutdown().await;
}

#[tokio::test]
async fn durable_restore_rejects_a_queued_command_whose_plan_and_source_were_not_retained() {
    use super::durable_queue::{Script, Step};
    use tickloop_harness_runtime::SkillInvocation;
    use tickloop_harness_runtime::{Delivery, ParentSelection, TurnOp, TurnRequest, WaitHosting};
    use tickloop_harness_test_support::transcripts::InMemoryTranscriptStore;
    use tickloop_harness_transcript::SettledOutcome;
    let folder = tempfile::tempdir().unwrap();
    let store = Arc::new(InMemoryTranscriptStore::new());
    let make = |store: Arc<InMemoryTranscriptStore>, model: Arc<Script>| {
        let environment = MemoryEnvironment::new();
        environment.define_shell("printf active", Duration::ZERO, b"active".to_vec(), 0);
        environment.define_shell("touch must-not-run", Duration::ZERO, b"wrong".to_vec(), 0);
        let mut options = HarnessOptions::local(config());
        options.waits = WaitHosting::Durable;
        options.host.prompt_policy = tickloop_harness_protocol::PromptPolicy::DenyOnPrompt;
        options.tools.tick_subscriptions = false;
        options.tools.user_skills = true;
        options.settings = Some(Arc::new(CommandSettings {
            memory: Default::default(),
            markdown: "---\ndescription: Check\n---\n!`touch must-not-run`".into(),
        }));
        Agent::with_seams(
            HarnessSeams {
                search: None,
                transcripts: Some(store),
                environment: Some(Arc::new(environment)),
                models: ChosenModels::new(model),
            },
            options,
        )
        .unwrap()
    };
    let model = Script::new(vec![Step::Hang]);
    let first = make(store.clone(), model.clone());
    let chat = first.create_chat(folder.path().into()).await.unwrap();
    first
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    first.submit(chat, "Do ordinary work".into()).await.unwrap();
    model.until_holding().await;
    let input_id = tickloop_ids::generate().into();
    first
        .advance(
            chat,
            TurnRequest {
                input_id,
                op: TurnOp::Skill {
                    after: ParentSelection::CurrentLeaf,
                    skill: SkillInvocation {
                        name: "demo:check".into(),
                        arguments: String::new(),
                    },
                },
                delivery: Delivery::Queue,
                host_context: None,
            },
        )
        .await
        .unwrap();
    first.sync().await.unwrap();
    let image = Arc::new(InMemoryTranscriptStore::new());
    image.seed(chat, store.lines(chat), store.position(chat).unwrap());
    first.shutdown().await;
    let resumed_model = Script::new(vec![
        Step::Reply(call("active", "Bash", json!({"command":"printf active"}))),
        Step::Reply(text("done")),
    ]);
    let resumed = make(image, resumed_model.clone());
    resumed.resume_chat(chat).await.unwrap();
    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        resumed.settled_input(chat, input_id),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        matches!(outcome, SettledOutcome::Failed { ref error } if error.contains("Skill preparation was interrupted or not retained")),
        "{outcome:?}"
    );
    assert!(
        resumed_model
            .conversation_requests()
            .iter()
            .all(|request| !request.contains("/demo:check"))
    );
    assert!(
        !resumed
            .snapshot(chat)
            .await
            .unwrap()
            .operations
            .iter()
            .any(|operation| operation.name == "Bash"
                && operation.input["command"] == "touch must-not-run")
    );
    resumed.shutdown().await;
}
