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

    fn hooks(&self, folder: Option<&Path>) -> tickloop_harness_protocol::hooks::HookList {
        self.memory.hooks(folder)
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
        assert_eq!(operations[0].input_message_id.is_some(), user);
        if let Some(input) = operations[0].input_message_id {
            assert!(
                observation
                    .snapshot
                    .message_ids
                    .iter()
                    .any(|(index, id)| *id == input
                        && observation.snapshot.message_runs.get(index) == Some(&receipt.run_id)
                        && observation.snapshot.messages[*index].role
                            == tickloop_harness_protocol::Role::User)
            );
        }
        assert_eq!(
            operations[0].parent_call_id.as_deref(),
            (!user).then_some("skill-call")
        );
        {
            let requests = model.requests.lock().unwrap();
            assert_eq!(requests.len(), if user { 1 } else { 2 });
            assert!(format!("{:?}", requests.last().unwrap()).contains("prepared-value"));
            if user {
                assert!(
                    !requests[0]
                        .messages
                        .iter()
                        .flat_map(|message| &message.content)
                        .any(|content| matches!(
                            content,
                            tickloop_harness_protocol::Content::ToolUse { .. }
                                | tickloop_harness_protocol::Content::ToolResult { .. }
                        ))
                );
                let history = serde_json::to_string(&requests[0].messages).unwrap();
                assert_eq!(history.matches("prepared-value").count(), 1);
            }
        }
        if user {
            let input = operations[0].input_message_id.unwrap();
            agent
                .rewind(
                    chat,
                    input,
                    tickloop_harness_protocol::RewindMode::Conversation,
                )
                .await
                .unwrap();
            assert!(agent.snapshot(chat).await.unwrap().operations.is_empty());
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
async fn user_command_preparation_has_no_model_authored_call_identity() {
    use tickloop_harness_protocol::Content;
    use tickloop_harness_test_support::routed::RoutedModel;
    let folder = tempfile::tempdir().unwrap();
    let environment = MemoryEnvironment::new();
    environment.define_shell("printf once", Duration::ZERO, b"once".to_vec(), 0);
    let model = Arc::new(RoutedModel::new(move |request| {
        assert!(
            !request
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .any(|content| matches!(
                    content,
                    Content::ToolUse { .. } | Content::ToolResult { .. }
                ))
        );
        text("done")
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
    assert!(bash_history(&snapshot).is_empty());
    assert_eq!(snapshot.operations.len(), 2);
    assert!(snapshot.operations.iter().all(
        |operation| operation.input_message_id.is_some() && operation.parent_call_id.is_none()
    ));
    assert!(snapshot.operations.iter().any(|operation| {
        operation.input["command"] == "printf first"
            && operation
                .result
                .as_ref()
                .is_some_and(|result| result.content.contains("first"))
    }));
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
        2
    );
    assert_eq!(
        bash_history(&restored.snapshot(chat).await.unwrap()),
        Vec::<String>::new()
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
            for invocation in
                checkpoint["chats"][0]["snapshot"]["history"]["user_skill_invocations"]
                    .as_object_mut()
                    .unwrap()
                    .values_mut()
            {
                invocation
                    .as_object_mut()
                    .unwrap()
                    .remove("prepared_without_shell");
            }
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
                        break;
                    }
                    EventKind::Failed(error) => {
                        panic!("{error}");
                    }
                    _ => {}
                }
            }
        }
        assert_eq!(model.requests.lock().unwrap().len(), 1);
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
        !agent
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
    assert!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .invoked_skills
            .iter()
            .any(|skill| skill.content.contains("Plain command guidance"))
    );

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

#[tokio::test]
async fn blocked_plain_command_guidance_never_enters_an_active_or_later_request() {
    use tickloop_harness_protocol::hooks::{
        ConfiguredHook, HookEvent, HookList, HookSource, HookTrust,
    };
    use tickloop_harness_runtime::environment::memory::MemoryCommandRun;
    const SENTINEL: &str = "BLOCKED_GUIDANCE_SENTINEL";
    for queued in [false, true] {
        let folder = tempfile::tempdir().unwrap();
        let environment = MemoryEnvironment::new();
        environment.set_shell_approval_required(true);
        environment.define_shell("touch hold", Duration::ZERO, b"held".to_vec(), 0);
        environment.define_command("guard-prompt", |spec| {
            let input: serde_json::Value = serde_json::from_slice(&spec.stdin).unwrap();
            let blocked = input["prompt"].as_str().unwrap().contains(SENTINEL);
            MemoryCommandRun {
                exit_code: if blocked { 2 } else { 0 },
                stderr: if blocked {
                    b"blocked".to_vec()
                } else {
                    Vec::new()
                },
                ..Default::default()
            }
        });
        let memory = MemorySettingsStore::default();
        memory
            .write(SettingsUpdate::TrustDirectory(
                folder.path().canonicalize().unwrap(),
            ))
            .unwrap();
        memory.set_hooks(HookList {
            hooks: vec![ConfiguredHook {
                key: "guard".into(),
                hash: None,
                source: HookSource::User,
                file: "/settings.json".into(),
                event: HookEvent::UserPromptSubmit,
                matcher: None,
                command: "guard-prompt".into(),
                resolved_command: Some("guard-prompt".into()),
                sensitive_values: Vec::new(),
                condition: None,
                timeout_secs: 600,
                background: None,
                plugin: None,
                trust: HookTrust::Trusted,
            }],
            ..Default::default()
        });
        let model = Arc::new(ScriptedModel::new(if queued {
            vec![
                call("hold", "Bash", json!({"command":"touch hold"})),
                text("done"),
            ]
        } else {
            vec![text("done")]
        }));
        let agent = Agent::with_seams(
            HarnessSeams {
                search: None,
                transcripts: None,
                environment: Some(Arc::new(environment)),
                models: ChosenModels::new(model.clone()),
            },
            HarnessOptions {
                settings: Some(Arc::new(CommandSettings {
                    memory,
                    markdown: "---\ndescription: Check\n---\nPlain guidance: $ARGUMENTS".into(),
                })),
                ..HarnessOptions::local(config())
            },
        )
        .unwrap();
        let chat = agent.create_chat(folder.path().into()).await.unwrap();
        agent
            .set_execution_mode(chat, ExecutionMode::AcceptEdits)
            .await
            .unwrap();
        let mut events = agent.subscribe();
        let active = if queued {
            let active = agent.submit(chat, "Start work".into()).await.unwrap();
            let permission = loop {
                if let EventKind::PermissionRequested { request } =
                    next_event(&mut events).await.kind
                {
                    break request;
                }
            };
            Some((active, permission))
        } else {
            None
        };
        let command = agent
            .invoke_skill(chat, "demo:check", SENTINEL)
            .await
            .unwrap();
        assert!(
            !agent
                .snapshot(chat)
                .await
                .unwrap()
                .invoked_skills
                .iter()
                .any(|skill| skill.content.contains(SENTINEL))
        );
        let waiting = if let Some((active, permission)) = active {
            agent
                .answer_permission(chat, permission.id, true)
                .await
                .unwrap();
            active
        } else {
            command
        };
        loop {
            let event = next_event(&mut events).await;
            if event.run_id != waiting.run_id {
                continue;
            }
            match event.kind {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        if !queued {
            let ordinary = agent
                .submit(chat, "Allowed ordinary prompt".into())
                .await
                .unwrap();
            loop {
                let event = next_event(&mut events).await;
                if event.run_id == ordinary.run_id
                    && matches!(event.kind, EventKind::Completed { .. })
                {
                    break;
                }
            }
        }
        assert!(!format!("{:?}", model.requests.lock().unwrap()).contains(SENTINEL));
        assert!(
            !agent
                .snapshot(chat)
                .await
                .unwrap()
                .invoked_skills
                .iter()
                .any(|skill| skill.content.contains(SENTINEL))
        );
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn command_preprocessing_respects_prompt_hooks_and_session_start_model_boundary() {
    use std::sync::Mutex;
    use tickloop_harness_protocol::hooks::{
        ConfiguredHook, HookEvent, HookList, HookSource, HookTrust,
    };
    use tickloop_harness_runtime::environment::memory::MemoryCommandRun;

    for user in [true, false] {
        for blocked in [true, false] {
            let folder = tempfile::tempdir().unwrap();
            let environment = MemoryEnvironment::new();
            environment.define_shell("printf prepared", Duration::ZERO, b"prepared".to_vec(), 0);
            let order = Arc::new(Mutex::new(Vec::new()));
            let memory = MemorySettingsStore::default();
            memory
                .write(SettingsUpdate::TrustDirectory(
                    folder.path().canonicalize().unwrap(),
                ))
                .unwrap();
            let mut hooks = Vec::new();
            for (event, command, matcher) in [
                (HookEvent::UserPromptSubmit, "prompt", None),
                (HookEvent::SessionStart, "session", None),
                (HookEvent::PreToolUse, "pre-bash", Some("Bash")),
            ] {
                let order = order.clone();
                environment.define_command(command, move |_| {
                    order.lock().unwrap().push(command);
                    MemoryCommandRun {
                        exit_code: if command == "prompt" && blocked { 2 } else { 0 },
                        stdout: if command == "session" { serde_json::to_vec(&json!({"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"session-command-context"}})).unwrap() } else { Vec::new() },
                        stderr: if command == "prompt" && blocked { b"blocked prompt".to_vec() } else { Vec::new() },
                        ..Default::default()
                    }
                });
                hooks.push(ConfiguredHook {
                    key: command.into(),
                    hash: None,
                    source: HookSource::User,
                    file: "/settings.json".into(),
                    event,
                    matcher: matcher.map(str::to_owned),
                    command: command.into(),
                    resolved_command: Some(command.into()),
                    sensitive_values: Vec::new(),
                    condition: None,
                    timeout_secs: 600,
                    background: None,
                    plugin: None,
                    trust: HookTrust::Trusted,
                });
            }
            memory.set_hooks(HookList {
                hooks,
                ..Default::default()
            });
            let model = Arc::new(ScriptedModel::new(if user {
                vec![text("done")]
            } else {
                vec![
                    call("skill", "Skill", json!({"skill":"demo:check"})),
                    text("done"),
                ]
            }));
            let agent = Agent::with_seams(
                HarnessSeams {
                    search: None,
                    transcripts: None,
                    environment: Some(Arc::new(environment)),
                    models: ChosenModels::new(model.clone()),
                },
                HarnessOptions {
                    settings: Some(Arc::new(CommandSettings {
                        memory,
                        markdown: "---\ndescription: Check\n---\n!`printf prepared`".into(),
                    })),
                    ..HarnessOptions::local(config())
                },
            )
            .unwrap();
            let chat = agent.create_chat(folder.path().into()).await.unwrap();
            agent
                .set_execution_mode(chat, ExecutionMode::FullAccess)
                .await
                .unwrap();
            let mut events = agent.subscribe();
            let receipt = if user {
                agent.invoke_skill(chat, "demo:check", "").await.unwrap()
            } else {
                agent.submit(chat, "Use demo:check".into()).await.unwrap()
            };
            loop {
                let event = next_event(&mut events).await;
                if event.run_id != receipt.run_id {
                    continue;
                }
                match event.kind {
                    EventKind::Completed { .. } | EventKind::Interrupted => break,
                    EventKind::Failed(error) => panic!("{error}"),
                    _ => {}
                }
            }
            let snapshot = agent.snapshot(chat).await.unwrap();
            if blocked {
                assert_eq!(*order.lock().unwrap(), ["session", "prompt"]);
                assert!(snapshot.invoked_skills.is_empty());
                assert!(
                    !snapshot
                        .operations
                        .iter()
                        .any(|operation| operation.name == "Bash")
                );
                assert!(model.requests.lock().unwrap().is_empty());
            } else {
                assert_eq!(
                    *order.lock().unwrap(),
                    vec!["session", "prompt", "pre-bash"]
                );
                assert!(
                    snapshot
                        .invoked_skills
                        .iter()
                        .any(|skill| skill.content.contains("prepared"))
                );
                let first_request = format!("{:?}", model.requests.lock().unwrap()[0]);
                assert!(first_request.contains("session-command-context"));
            }
            agent.shutdown().await;
        }
    }
}

#[tokio::test]
async fn durable_restore_continues_a_queued_plain_command_with_retained_guidance() {
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
            markdown: "---\ndescription: Check\n---\nRetained plain command guidance".into(),
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
    let mut events = resumed.subscribe();
    resumed.resume_chat(chat).await.unwrap();
    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        resumed.settled_input(chat, input_id),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        matches!(
            outcome,
            SettledOutcome::Completed { .. } | SettledOutcome::Merged { .. }
        ),
        "{outcome:?}"
    );
    loop {
        match next_event(&mut events).await.kind {
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert!(
        resumed_model
            .conversation_requests()
            .iter()
            .any(|request| request.contains("Retained plain command guidance"))
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

#[tokio::test]
#[ignore = "requires DEEPSEEK_API_KEY and calls the real thinking-enabled model"]
async fn live_deepseek_thinking_accepts_user_command_preprocessing() {
    use tickloop_harness_model::{LocalModelClients, ModelClients, cards::ModelCatalog};
    use tickloop_harness_protocol::Content;
    struct ThinkingClient(Arc<dyn tickloop_harness_model::ModelClient>);
    #[async_trait::async_trait]
    impl tickloop_harness_model::ModelClient for ThinkingClient {
        fn identity(&self) -> tickloop_harness_protocol::ModelIdentity {
            self.0.identity()
        }
        fn supports_reasoning_effort(
            &self,
            effort: tickloop_harness_protocol::ReasoningEffort,
        ) -> bool {
            self.0.supports_reasoning_effort(effort)
        }
        async fn generate_title(
            &self,
            request: tickloop_harness_model::ModelRequest,
            events: tokio::sync::mpsc::UnboundedSender<tickloop_harness_model::ModelDelta>,
            cancel: tokio_util::sync::CancellationToken,
        ) -> Result<tickloop_harness_model::ModelOutput, tickloop_harness_model::ModelError>
        {
            tickloop_harness_model::ModelClient::generate_title(
                &ScriptedModel::new(vec![]),
                request,
                events,
                cancel,
            )
            .await
        }
        async fn complete(
            &self,
            request: tickloop_harness_model::ModelRequest,
            events: tokio::sync::mpsc::UnboundedSender<tickloop_harness_model::ModelDelta>,
            cancel: tokio_util::sync::CancellationToken,
        ) -> Result<tickloop_harness_model::ModelOutput, tickloop_harness_model::ModelError>
        {
            assert_eq!(
                request.reasoning_effort,
                Some(tickloop_harness_protocol::ReasoningEffort::High)
            );
            assert!(
                !request
                    .messages
                    .iter()
                    .flat_map(|message| &message.content)
                    .any(|content| matches!(
                        content,
                        Content::ToolUse { .. } | Content::ToolResult { .. }
                    ))
            );
            assert_eq!(
                serde_json::to_string(&request.messages)
                    .unwrap()
                    .matches("ROUND06_PREPARED_OK")
                    .count(),
                1
            );
            let output = self.0.complete(request, events, cancel).await?;
            println!("Live response: thinking blocks={}, text blocks={}, prepared token={}", output.content.iter().filter(|content| matches!(content, Content::Thinking { .. })).count(), output.content.iter().filter(|content| matches!(content, Content::Text { .. })).count(), output.content.iter().any(|content| matches!(content, Content::Text { text } if text.trim() == "ROUND06_PREPARED_OK")));
            Ok(output)
        }
    }
    let root = tempfile::tempdir().unwrap();
    let mut model_config = ModelCatalog::defaults()
        .get("deepseek-api", "deepseek-flash")
        .unwrap();
    model_config.max_output_tokens = 4096;
    model_config.card.as_mut().unwrap().max_output_tokens = 4096;
    let client = LocalModelClients::new(Some(root.path().join("config")))
        .client(&model_config)
        .unwrap();
    let environment = MemoryEnvironment::new();
    environment.define_shell(
        "printf round06",
        Duration::ZERO,
        b"ROUND06_PREPARED_OK".to_vec(),
        0,
    );
    let agent = Agent::with_seams(HarnessSeams {
        search: None, transcripts: None, environment: Some(Arc::new(environment)), models: ChosenModels::new(Arc::new(ThinkingClient(client))),
    }, HarnessOptions {
        settings: Some(Arc::new(CommandSettings { memory: Default::default(), markdown: "---\ndescription: Verify user command preprocessing\n---\nReply with exactly the following prepared token and no other text. Do not call any tools.\n!`printf round06`".into() })),
        ..HarnessOptions::local(model_config)
    }).unwrap();
    let chat = agent.create_chat(root.path().into()).await.unwrap();
    agent.set_thinking_enabled(chat, true).await.unwrap();
    agent
        .set_reasoning_effort(chat, Some(tickloop_harness_protocol::ReasoningEffort::High))
        .await
        .unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let receipt = agent.invoke_skill(chat, "demo:check", "").await.unwrap();
    tokio::time::timeout(Duration::from_secs(180), async {
        loop {
            let event = next_event(&mut events).await;
            if event.run_id != receipt.run_id {
                continue;
            }
            match event.kind {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("Live command failed: {error}"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(snapshot.thinking_enabled);
    assert_eq!(
        snapshot.reasoning_effort,
        Some(tickloop_harness_protocol::ReasoningEffort::High)
    );

    assert!(
        !snapshot
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .any(|content| matches!(
                content,
                Content::ToolUse { .. } | Content::ToolResult { .. }
            ))
    );
    assert!(snapshot.messages.iter().filter(|message| message.role == tickloop_harness_protocol::Role::Assistant).flat_map(|message| &message.content).any(|content| matches!(content, Content::Text { text } if text.trim() == "ROUND06_PREPARED_OK")));
    assert_eq!(snapshot.operations.len(), 1);
    assert!(snapshot.operations[0].input_message_id.is_some());
    println!(
        "DeepSeek deepseek-flash: thinking enabled, explicit high effort verified on the request, prepared token returned, no synthetic tool messages, one persisted input operation"
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn user_command_hook_refusal_cancellation_and_stop_keep_input_records() {
    use tickloop_harness_protocol::{
        Content, OperationState,
        hooks::{ConfiguredHook, HookEvent, HookList, HookSource, HookTrust},
    };
    use tickloop_harness_runtime::environment::memory::MemoryCommandRun;
    use tickloop_harness_test_support::transcripts::InMemoryTranscriptStore;
    for scenario in ["refused", "cancelled", "stopped"] {
        let folder = tempfile::tempdir().unwrap();
        let store = Arc::new(InMemoryTranscriptStore::new());
        let environment = MemoryEnvironment::new();
        environment.define_shell(
            "printf guarded",
            Duration::ZERO,
            b"guarded-output".to_vec(),
            0,
        );
        let started = Arc::new(tokio::sync::Notify::new());
        let signal = started.clone();
        environment.define_command("guard", move |_| {
            signal.notify_one();
            MemoryCommandRun {
                exit_code: if scenario == "refused" { 2 } else { 0 },
                stderr: if scenario == "refused" {
                    b"command refused".to_vec()
                } else {
                    vec![]
                },
                stdout: if scenario == "stopped" {
                    serde_json::to_vec(&json!({"continue":false,"stopReason":"No model request"}))
                        .unwrap()
                } else {
                    vec![]
                },
                duration: if scenario == "cancelled" {
                    Duration::from_secs(60)
                } else {
                    Duration::ZERO
                },
                ..Default::default()
            }
        });
        let memory = MemorySettingsStore::default();
        memory
            .write(SettingsUpdate::TrustDirectory(
                folder.path().canonicalize().unwrap(),
            ))
            .unwrap();
        memory.set_hooks(HookList {
            hooks: vec![ConfiguredHook {
                key: "guard".into(),
                hash: None,
                source: HookSource::User,
                file: "/settings.json".into(),
                event: if scenario == "stopped" {
                    HookEvent::PostToolUse
                } else {
                    HookEvent::PreToolUse
                },
                matcher: Some("Bash".into()),
                command: "guard".into(),
                resolved_command: Some("guard".into()),
                sensitive_values: vec![],
                condition: None,
                timeout_secs: 600,
                background: None,
                plugin: None,
                trust: HookTrust::Trusted,
            }],
            ..Default::default()
        });
        let settings = Arc::new(CommandSettings {
            memory,
            markdown: "---\ndescription: Check\n---\n!`printf guarded`".into(),
        });
        let model = Arc::new(ScriptedModel::new(vec![text("must not call model")]));
        let make = |transcripts: Arc<InMemoryTranscriptStore>| {
            Agent::with_seams(
                HarnessSeams {
                    search: None,
                    transcripts: Some(transcripts),
                    environment: Some(Arc::new(environment.clone())),
                    models: ChosenModels::new(model.clone()),
                },
                HarnessOptions {
                    settings: Some(settings.clone()),
                    ..HarnessOptions::local(config())
                },
            )
            .unwrap()
        };
        let agent = make(store.clone());
        let chat = agent.create_chat(folder.path().into()).await.unwrap();
        agent
            .set_execution_mode(chat, ExecutionMode::FullAccess)
            .await
            .unwrap();
        let mut events = agent.subscribe();
        let receipt = agent.invoke_skill(chat, "demo:check", "").await.unwrap();
        if scenario == "cancelled" {
            tokio::time::timeout(Duration::from_secs(10), started.notified())
                .await
                .unwrap();
            agent.interrupt(chat).await.unwrap();
        }
        let mut dispatched = 0;
        loop {
            let event = next_event(&mut events).await;
            if event.run_id != receipt.run_id {
                continue;
            }
            match event.kind {
                EventKind::ToolStarted { name, .. } if name == "Bash" => dispatched += 1,
                EventKind::Completed { .. } => {
                    assert_eq!(scenario, "stopped");
                    break;
                }
                EventKind::Failed(_) | EventKind::Interrupted => {
                    assert_ne!(scenario, "stopped");
                    break;
                }
                _ => {}
            }
        }
        if scenario == "cancelled" {
            let snapshot = agent.snapshot(chat).await.unwrap();
            assert!(snapshot.messages.iter().flat_map(|message| &message.content).any(|content| matches!(content, Content::Text {text} if text == "[Request interrupted by user for tool use]")));
        }
        assert!(model.requests.lock().unwrap().is_empty(), "{scenario}");
        assert_eq!(dispatched, usize::from(scenario == "stopped"));
        agent.sync().await.unwrap();
        let image = Arc::new(InMemoryTranscriptStore::new());
        image.seed(chat, store.lines(chat), store.position(chat).unwrap());
        let restored = make(image);
        let snapshot = restored.resume_chat(chat).await.unwrap();
        assert_eq!(snapshot.operations.len(), 1, "{scenario}");
        let operation = &snapshot.operations[0];
        assert!(operation.input_message_id.is_some());
        assert_eq!(operation.input["command"], "printf guarded");
        assert_eq!(
            operation.state,
            if scenario == "stopped" {
                OperationState::Completed
            } else {
                OperationState::Failed
            }
        );
        assert_eq!(
            operation.result.as_ref().unwrap().is_error,
            scenario != "stopped"
        );
        assert!(
            !snapshot
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .any(|content| matches!(
                    content,
                    Content::ToolUse { .. } | Content::ToolResult { .. }
                ))
        );
        agent.shutdown().await;
        restored.shutdown().await;
    }
}

#[tokio::test]
async fn command_waiting_for_session_start_survives_upgrade_before_its_only_shell() {
    use tickloop_harness_protocol::hooks::{
        ConfiguredHook, HookEvent, HookList, HookSource, HookTrust,
    };
    use tickloop_harness_protocol::{Content, OperationState};
    use tickloop_harness_runtime::environment::memory::MemoryCommandRun;
    let folder = tempfile::tempdir().unwrap();
    let environment = Arc::new(MemoryEnvironment::new());
    environment.define_shell("printf held", Duration::ZERO, b"after-upgrade".to_vec(), 0);
    let entered = Arc::new(tokio::sync::Notify::new());
    let notify = entered.clone();
    environment.define_command("session", move |_| {
        notify.notify_one();
        MemoryCommandRun {
            duration: Duration::from_secs(300),
            ..Default::default()
        }
    });
    let memory = MemorySettingsStore::default();
    memory
        .write(SettingsUpdate::TrustDirectory(
            folder.path().canonicalize().unwrap(),
        ))
        .unwrap();
    memory.set_hooks(HookList {
        hooks: vec![ConfiguredHook {
            key: "session".into(),
            hash: None,
            source: HookSource::User,
            file: "/settings.json".into(),
            event: HookEvent::SessionStart,
            matcher: None,
            command: "session".into(),
            resolved_command: Some("session".into()),
            sensitive_values: Vec::new(),
            condition: None,
            timeout_secs: 600,
            background: None,
            plugin: None,
            trust: HookTrust::Trusted,
        }],
        ..Default::default()
    });
    let settings = Arc::new(CommandSettings {
        memory,
        markdown: "---\ndescription: Check\n---\n!`printf held`".into(),
    });
    let make = |model: Arc<ScriptedModel>| {
        Agent::with_seams(
            HarnessSeams {
                search: None,
                transcripts: None,
                environment: Some(environment.clone()),
                models: ChosenModels::new(model),
            },
            HarnessOptions {
                settings: Some(settings.clone()),
                ..HarnessOptions::local(config())
            },
        )
        .unwrap()
    };
    let predecessor = make(Arc::new(ScriptedModel::new(Vec::new())));
    let chat = predecessor.create_chat(folder.path().into()).await.unwrap();
    predecessor
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let mut events = predecessor.subscribe();
    let receipt = predecessor
        .invoke_skill(chat, "demo:check", "")
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    assert!(
        predecessor
            .snapshot(chat)
            .await
            .unwrap()
            .operations
            .is_empty()
    );
    predecessor.interrupt(chat).await.unwrap();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id == receipt.run_id && matches!(event.kind, EventKind::Interrupted) {
            break;
        }
    }
    let checkpoint = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(checkpoint) = predecessor.prepare_upgrade().await.unwrap() {
                break checkpoint;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let checkpoint = serde_json::from_value(serde_json::to_value(checkpoint).unwrap()).unwrap();
    let model = Arc::new(ScriptedModel::new(vec![text("done")]));
    let successor = make(model.clone());
    successor.restore_upgrade(checkpoint).await.unwrap();
    successor.activate_upgrade().await.unwrap();
    predecessor.shutdown_for_upgrade().await;
    successor.release_upgrade().await.unwrap();
    assert!(
        successor
            .snapshot(chat)
            .await
            .unwrap()
            .operations
            .is_empty()
    );
    environment.define_command("session", |_| MemoryCommandRun::default());
    let mut events = successor.subscribe();
    successor.continue_loop(chat).await.unwrap();
    let mut shells = 0;
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != receipt.run_id {
            continue;
        }
        match event.kind {
            EventKind::ToolStarted { name, .. } if name == "Bash" => shells += 1,
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert_eq!(shells, 1);
    let snapshot = successor.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.operations.len(), 1);
    assert_eq!(snapshot.operations[0].state, OperationState::Completed);
    let input = snapshot.operations[0].input_message_id.unwrap();
    assert!(
        snapshot
            .message_ids
            .iter()
            .any(|(index, id)| *id == input && snapshot.prompt_execution_modes.contains_key(index))
    );
    assert!(
        !snapshot
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .any(|content| matches!(
                content,
                Content::ToolUse { .. } | Content::ToolResult { .. }
            ))
    );
    assert_eq!(model.requests.lock().unwrap().len(), 1);
    successor.shutdown().await;
}

#[tokio::test]
async fn a_prior_plain_invocation_cannot_prove_a_later_shell_invocation_prepared() {
    use tickloop_harness_protocol::hooks::{
        ConfiguredHook, HookEvent, HookList, HookSource, HookTrust,
    };
    use tickloop_harness_runtime::{WaitHosting, environment::memory::MemoryCommandRun};
    use tickloop_harness_test_support::transcripts::InMemoryTranscriptStore;
    let folder = tempfile::tempdir().unwrap();
    let store = Arc::new(InMemoryTranscriptStore::new());
    let entered = Arc::new(tokio::sync::Notify::new());
    let memory = MemorySettingsStore::default();
    memory
        .write(SettingsUpdate::TrustDirectory(
            folder.path().canonicalize().unwrap(),
        ))
        .unwrap();
    memory.set_hooks(HookList {
        hooks: vec![ConfiguredHook {
            key: "hold".into(),
            hash: None,
            source: HookSource::User,
            file: "/settings.json".into(),
            event: HookEvent::PreToolUse,
            matcher: Some("Bash".into()),
            command: "hold".into(),
            resolved_command: Some("hold".into()),
            sensitive_values: Vec::new(),
            condition: None,
            timeout_secs: 600,
            background: None,
            plugin: None,
            trust: HookTrust::Trusted,
        }],
        ..Default::default()
    });
    let settings = Arc::new(CommandSettings {
        memory,
        markdown: "---\ndescription: Check\n---\n!$ARGUMENTS".into(),
    });
    let make = |store: Arc<InMemoryTranscriptStore>, model: Arc<ScriptedModel>, wait: bool| {
        let environment = MemoryEnvironment::new();
        environment.define_shell(
            "printf must-not-repeat",
            Duration::ZERO,
            b"unexpected".to_vec(),
            0,
        );
        let notify = entered.clone();
        environment.define_command("hold", move |_| {
            notify.notify_one();
            MemoryCommandRun {
                duration: if wait {
                    Duration::from_secs(300)
                } else {
                    Duration::ZERO
                },
                ..Default::default()
            }
        });
        let mut options = HarnessOptions::local(config());
        options.settings = Some(settings.clone());
        options.waits = WaitHosting::Durable;
        options.host.prompt_policy = tickloop_harness_protocol::PromptPolicy::DenyOnPrompt;
        options.tools.tick_subscriptions = false;
        options.tools.user_skills = true;
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
    let first_model = Arc::new(ScriptedModel::new(vec![text("first done")]));
    let first = make(store.clone(), first_model, true);
    let chat = first.create_chat(folder.path().into()).await.unwrap();
    first
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let mut events = first.subscribe();
    let plain = first
        .invoke_skill(chat, "demo:check", "OLD_PLAIN_GUIDANCE")
        .await
        .unwrap();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id == plain.run_id && matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
    }
    assert_eq!(
        first.snapshot(chat).await.unwrap().invoked_skills[0].shell_preprocessing,
        Some(false)
    );
    let shell = first
        .invoke_skill(chat, "demo:check", "`printf must-not-repeat`")
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    first.sync().await.unwrap();
    let crash = Arc::new(InMemoryTranscriptStore::new());
    crash.seed(chat, store.lines(chat), store.position(chat).unwrap());
    first.shutdown().await;
    let model = Arc::new(ScriptedModel::new(vec![text(
        "must not run with stale guidance",
    )]));
    let restored = make(crash, model.clone(), false);
    restored.resume_chat(chat).await.unwrap();
    let mut events = restored.subscribe();
    restored.continue_loop(chat).await.unwrap();
    let mut dispatched = 0;
    let failure = loop {
        let event = next_event(&mut events).await;
        if event.run_id != shell.run_id {
            continue;
        }
        match event.kind {
            EventKind::ToolStarted { name, .. } if name == "Bash" => dispatched += 1,
            EventKind::Failed(error) => break Some(error),
            EventKind::Completed { .. } => break None,
            _ => {}
        }
    };
    assert!(
        failure
            .as_deref()
            .is_some_and(|error| error.contains("interrupted or not retained")),
        "{failure:?}"
    );
    assert!(model.requests.lock().unwrap().is_empty());
    assert_eq!(dispatched, 0);
    assert!(
        restored
            .snapshot(chat)
            .await
            .unwrap()
            .operations
            .iter()
            .all(|operation| operation.run_id != shell.run_id)
    );
    restored.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn merged_plain_guidance_does_not_invalidate_the_started_invocations_preparation() {
    use super::durable_queue::{Script, Step};
    use tickloop_harness_protocol::hooks::{
        ConfiguredHook, HookEvent, HookList, HookSource, HookTrust,
    };
    use tickloop_harness_runtime::{WaitHosting, environment::memory::MemoryCommandRun};
    use tickloop_harness_test_support::transcripts::InMemoryTranscriptStore;
    let folder = tempfile::tempdir().unwrap();
    std::fs::write(folder.path().join("notes.txt"), "note").unwrap();
    let store = Arc::new(InMemoryTranscriptStore::new());
    let entered = Arc::new(tokio::sync::Notify::new());
    let memory = MemorySettingsStore::default();
    memory
        .write(SettingsUpdate::TrustDirectory(
            folder.path().canonicalize().unwrap(),
        ))
        .unwrap();
    memory.set_hooks(HookList {
        hooks: vec![ConfiguredHook {
            key: "read".into(),
            hash: None,
            source: HookSource::User,
            file: "/settings.json".into(),
            event: HookEvent::PreToolUse,
            matcher: Some("Read".into()),
            command: "hold-read".into(),
            resolved_command: Some("hold-read".into()),
            sensitive_values: Vec::new(),
            condition: None,
            timeout_secs: 600,
            background: None,
            plugin: None,
            trust: HookTrust::Trusted,
        }],
        ..Default::default()
    });
    let settings = Arc::new(CommandSettings {
        memory,
        markdown: "---\ndescription: Check\n---\nGuidance: $ARGUMENTS".into(),
    });
    let make = |store: Arc<InMemoryTranscriptStore>, model: Arc<Script>| {
        let environment = MemoryEnvironment::new();
        let notify = entered.clone();
        environment.define_command("hold-read", move |_| {
            notify.notify_one();
            MemoryCommandRun {
                duration: Duration::from_secs(300),
                ..Default::default()
            }
        });
        let mut options = HarnessOptions::local(config());
        options.settings = Some(settings.clone());
        options.waits = WaitHosting::Durable;
        options.host.prompt_policy = tickloop_harness_protocol::PromptPolicy::DenyOnPrompt;
        options.tools.tick_subscriptions = false;
        options.tools.user_skills = true;
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
    let model = Script::new(vec![
        Step::Reply(call("read", "Read", json!({"file_path":"notes.txt"}))),
        Step::Hang,
    ]);
    let first = make(store.clone(), model.clone());
    let chat = first.create_chat(folder.path().into()).await.unwrap();
    first
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let original = first
        .invoke_skill(chat, "demo:check", "GUIDANCE_A")
        .await
        .unwrap();
    entered.notified().await;
    first
        .invoke_skill(chat, "demo:check", "GUIDANCE_B")
        .await
        .unwrap();
    tokio::time::advance(Duration::from_secs(300)).await;
    model.until_holding().await;
    assert!(model.requests().last().unwrap().contains("GUIDANCE_B"));
    first.sync().await.unwrap();
    let crash = Arc::new(InMemoryTranscriptStore::new());
    crash.seed(chat, store.lines(chat), store.position(chat).unwrap());
    first.shutdown().await;
    let model = Script::new(vec![Step::Reply(text("continued"))]);
    let restored = make(crash, model.clone());
    restored.resume_chat(chat).await.unwrap();
    let mut events = restored.subscribe();
    restored.continue_loop(chat).await.unwrap();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != original.run_id {
            continue;
        }
        match event.kind {
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => {
                panic!("a prepared plain invocation must continue: {error}")
            }
            _ => {}
        }
    }
    assert_eq!(model.requests().len(), 1);
    assert!(model.requests()[0].contains("GUIDANCE_B"));
    restored.shutdown().await;
}

/// On a host that keeps durable approvals, a command's shell that would ask a person is denied
/// as before: only an `ExecuteTools` batch's calls wait durably, and a command never activates
/// on a shell nobody ran.
#[tokio::test]
async fn a_command_shell_that_would_ask_is_denied_on_a_durable_approval_host() {
    use tickloop_harness_runtime::WaitHosting;
    use tickloop_harness_test_support::transcripts::InMemoryTranscriptStore;
    let folder = tempfile::tempdir().unwrap();
    let environment = MemoryEnvironment::new();
    environment.define_shell(
        "printf asked",
        Duration::ZERO,
        b"must-not-appear".to_vec(),
        0,
    );
    let model = Arc::new(ScriptedModel::new(vec![
        call("skill-call", "Skill", json!({"skill":"demo:check"})),
        text("denied"),
    ]));
    let mut options = HarnessOptions::local(config());
    options.waits = WaitHosting::Durable;
    options.host.prompt_policy = tickloop_harness_protocol::PromptPolicy::DurableApproval;
    options.tools.tick_subscriptions = false;
    options.settings = Some(Arc::new(CommandSettings {
        memory: Default::default(),
        markdown: "---\ndescription: Check\n---\nGuidance !`printf asked`".into(),
    }));
    let agent = Agent::with_seams(
        HarnessSeams {
            search: None,
            transcripts: Some(Arc::new(InMemoryTranscriptStore::new())),
            environment: Some(Arc::new(environment)),
            models: ChosenModels::new(model.clone()),
        },
        options,
    )
    .unwrap();
    let chat = agent.create_chat(folder.path().into()).await.unwrap();
    agent
        .set_permission_rules(
            chat,
            vec![PermissionRule {
                id: tickloop_ids::generate(),
                decision: Decision::Ask,
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
    let receipt = agent.submit(chat, "Use command".into()).await.unwrap();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != receipt.run_id {
            continue;
        }
        match event.kind {
            EventKind::PermissionRequested { .. } => panic!("a command's shell never waits"),
            EventKind::Completed { .. } | EventKind::Failed(_) => break,
            _ => {}
        }
    }
    let observation = agent.observe(chat, 0).await.unwrap();
    assert!(observation.snapshot.invoked_skills.is_empty());
    assert!(observation.snapshot.permissions.is_empty());
    assert!(!format!("{:?}", observation.snapshot.messages).contains("must-not-appear"));
    agent.shutdown().await;
}
