use tickloop_code_local_environment::{AgentLocalExt, environment::LocalEnvironment};
// Managed requests keep their resource lifetime separate from ordinary Console Chats.

use std::{sync::Arc, time::Duration};

use serde_json::json;
use tickloop_harness::protocol::{
    AgentHostOptions, CommandState, DirectoryTrustPolicy, ExecutionMode, LoopState, PromptPolicy,
};
use tickloop_harness::runtime::{
    Agent, HarnessOptions, HarnessSeams, ToolPolicy, WaitHosting, transcript::TranscriptStore,
};
use tickloop_harness::test_support::harness::ChosenModels;
use tickloop_harness::test_support::{ScriptedModel, call, config, text};

fn harness(policy: ToolPolicy, model: Arc<ScriptedModel>) -> Agent {
    harness_with_store(policy, model, None)
}

fn harness_with_store(
    policy: ToolPolicy,
    model: Arc<ScriptedModel>,
    transcripts: Option<Arc<dyn TranscriptStore>>,
) -> Agent {
    harness_with_host(
        policy,
        model,
        transcripts,
        AgentHostOptions {
            prompt_policy: PromptPolicy::DenyOnPrompt,
            ..AgentHostOptions::embedded_defaults()
        },
    )
}

fn harness_with_host(
    policy: ToolPolicy,
    model: Arc<ScriptedModel>,
    transcripts: Option<Arc<dyn TranscriptStore>>,
    host: AgentHostOptions,
) -> Agent {
    Agent::with_seams(
        HarnessSeams {
            search: None,
            transcripts,
            environment: Some(Arc::new(LocalEnvironment::bare())),
            models: ChosenModels::new(model),
        },
        HarnessOptions {
            search: Default::default(),
            model: config(),
            settings: None,
            host,
            tools: policy,
            host_rules: Vec::new(),
            task_tools: Some(true),
            waits: WaitHosting::Process,
            citation_style: Default::default(),
            system_prompt_addition: None,
            request_gates: Vec::new(),
            ..HarnessOptions::local(config())
        },
    )
    .unwrap()
}

#[tokio::test]
async fn launch_profiles_must_match_the_actual_harness_policies() {
    let host = AgentHostOptions {
        prompt_policy: PromptPolicy::DenyOnPrompt,
        ..AgentHostOptions::embedded_defaults()
    };
    for tools in [
        ToolPolicy::local(),
        ToolPolicy::durable_chat(),
        ToolPolicy::one_shot(),
        ToolPolicy::acp(),
    ] {
        let agent = harness(tools, Arc::new(ScriptedModel::new(vec![])));
        let managed = tools == ToolPolicy::one_shot() || tools == ToolPolicy::acp();
        assert_eq!(
            agent
                .validate_tool_policy(ToolPolicy::local(), host)
                .is_ok(),
            !managed
        );
        for profile in [ToolPolicy::one_shot(), ToolPolicy::one_shot()] {
            assert_eq!(agent.validate_tool_policy(profile, host).is_ok(), managed);
            for mismatched in [
                AgentHostOptions {
                    execution_mode_override: Some(ExecutionMode::FullAccess),
                    ..host
                },
                AgentHostOptions {
                    directory_trust: DirectoryTrustPolicy::RequireUserTrust,
                    ..host
                },
                AgentHostOptions {
                    prompt_policy: PromptPolicy::Interactive,
                    ..host
                },
            ] {
                assert!(agent.validate_tool_policy(profile, mismatched).is_err());
            }
        }
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn managed_cancellation_waits_for_parked_permission_acknowledgment() {
    use tickloop_harness::protocol::{Decision, PermissionRule};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("not-written");
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "write",
            "Write",
            json!({"file_path":path,"content":"must not write"}),
        ),
        text("next prompt"),
    ]));
    let agent = harness_with_host(
        ToolPolicy::acp(),
        model,
        None,
        AgentHostOptions::embedded_defaults(),
    );
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_permission_rules(
            chat,
            vec![PermissionRule {
                id: tickloop_harness::ids::generate(),
                decision: Decision::Ask,
                tool: "Write".into(),
                specifier: None,
                input: None,
                loop_id: None,
                persistent: false,
            }],
        )
        .await
        .unwrap();
    agent.submit(chat, "write a file".into()).await.unwrap();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(10), agent.quiesce(chat))
            .await
            .unwrap(),
        tickloop_harness::runtime::Quiescence::Waiting(_)
    ));
    let snapshot = agent.settle_managed(chat, true).await.unwrap();
    assert!(snapshot.permissions.is_empty());
    assert_eq!(snapshot.chat_loop.state, LoopState::Interrupted);
    assert!(!path.exists());
    let next = agent.submit(chat, "next".into()).await.unwrap();
    let snapshot = agent.settle_managed(chat, false).await.unwrap();
    assert_eq!(
        snapshot.run_response_text(next.run_id).as_deref(),
        Some("next prompt")
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn managed_tools_exclude_unrouted_waits_and_autonomous_work() {
    for policy in [ToolPolicy::one_shot(), ToolPolicy::acp()] {
        let model = Arc::new(ScriptedModel::new(vec![text("done")]));
        let agent = harness(policy, model.clone());
        let directory = tempfile::tempdir().unwrap();
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        agent.submit(chat, "finish".into()).await.unwrap();
        let snapshot = agent.settle_managed(chat, false).await.unwrap();
        assert_eq!(snapshot.chat_loop.state, LoopState::Idle);
        let tools = model.requests.lock().unwrap()[0].tools.clone();
        for forbidden in [
            "AskUserQuestion",
            "EnterPlanMode",
            "ExitPlanMode",
            "GetGoal",
            "CreateGoal",
            "UpdateGoal",
            "CronCreate",
            "CronList",
            "CronUpdate",
            "CronDelete",
            "ScheduleWakeup",
            "Subagent",
            "ListSubagents",
            "SendMessage",
            "StopSubagent",
            "TaskCreate",
            "TaskList",
            "TaskGet",
            "TaskUpdate",
        ] {
            assert!(
                !tools.iter().any(|tool| tool.name == forbidden),
                "{forbidden}"
            );
        }
        assert!(tools.iter().any(|tool| tool.name == "Bash"));
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn managed_settlement_flushes_the_final_answer() {
    let store =
        Arc::new(tickloop_harness::test_support::transcripts::InMemoryTranscriptStore::new());
    let agent = harness_with_store(
        ToolPolicy::one_shot(),
        Arc::new(ScriptedModel::new(vec![text("durable final answer")])),
        Some(store.clone()),
    );
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent.submit(chat, "finish durably".into()).await.unwrap();
    agent.settle_managed(chat, false).await.unwrap();
    assert!(
        serde_json::to_string(&store.lines(chat))
            .unwrap()
            .contains("durable final answer")
    );
    assert!(agent.transcript_write_error().is_none());
    agent.shutdown().await;
}

#[tokio::test]
async fn ordinary_chats_cannot_be_settled_as_managed() {
    let agent = harness(ToolPolicy::local(), Arc::new(ScriptedModel::new(vec![])));
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let error = agent.settle_managed(chat, true).await.unwrap_err();
    assert!(error.to_string().contains("ordinary Chats"));
    agent.shutdown().await;
}

#[tokio::test]
async fn managed_settlement_stops_background_shell_without_another_model_run() {
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "background",
            "Bash",
            json!({
                "command": "sleep 60", "run_in_background": true,
                "description": "Wait in a managed test"
            }),
        ),
        text("done"),
    ]));
    let agent = harness(ToolPolicy::one_shot(), model.clone());
    agent.install_own_tickhost();
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let receipt = agent
        .submit(chat, "start a background sleep".into())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(15), agent.quiesce(chat))
        .await
        .unwrap();
    assert!(
        agent
            .commands(chat)
            .await
            .unwrap()
            .iter()
            .any(|command| command.state == CommandState::Background)
    );
    let snapshot = agent.settle_managed(chat, false).await.unwrap();
    assert_eq!(
        snapshot.run_response_text(receipt.run_id).as_deref(),
        Some("done")
    );
    assert_eq!(model.requests.lock().unwrap().len(), 2);
    assert!(snapshot.commands.iter().all(|command| matches!(
        command.state,
        CommandState::Completed | CommandState::Failed
    )));
    assert!(snapshot.pending_inputs.is_empty());
    agent.shutdown().await;
}

#[tokio::test]
async fn managed_settlement_stops_user_shell_without_autonomous_continuation() {
    for wait_for_start in [false, true] {
        let model = Arc::new(ScriptedModel::new(vec![text("done")]));
        let agent = harness(ToolPolicy::acp(), model.clone());
        agent.install_own_tickhost();
        let directory = tempfile::tempdir().unwrap();
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        agent
            .set_execution_mode(chat, ExecutionMode::FullAccess)
            .await
            .unwrap();
        agent.submit(chat, "finish".into()).await.unwrap();
        agent.quiesce(chat).await;
        agent
            .submit_user_shell(chat, "sleep 60".into(), true)
            .await
            .unwrap();
        if wait_for_start {
            tokio::time::timeout(Duration::from_secs(10), async {
                while !agent
                    .snapshot(chat)
                    .await
                    .unwrap()
                    .user_shells
                    .iter()
                    .any(|shell| {
                        shell.state == tickloop_harness::protocol::UserShellState::Background
                    })
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        }
        let snapshot = agent.settle_managed(chat, false).await.unwrap();
        assert!(snapshot.user_shells.iter().all(|shell| matches!(
            shell.state,
            tickloop_harness::protocol::UserShellState::Completed
                | tickloop_harness::protocol::UserShellState::Failed
        )));
        assert_eq!(model.requests.lock().unwrap().len(), 1);
        assert!(snapshot.pending_inputs.is_empty());
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn acp_claims_are_exclusive_across_harnesses_and_reject_symlink_replacement() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![]));
    let first = harness(ToolPolicy::acp(), model.clone());
    let next = harness(ToolPolicy::acp(), model.clone());
    let chat = tickloop_harness::ids::generate();
    first
        .claim_acp_session(chat.into(), directory.path(), directory.path())
        .await
        .unwrap();
    assert!(matches!(
        next.claim_acp_session(chat.into(), directory.path(), directory.path())
            .await,
        Err(tickloop_harness::runtime::AgentError::Busy)
    ));
    first.release_acp_session_claim(chat.into()).await;
    next.claim_acp_session(chat.into(), directory.path(), directory.path())
        .await
        .unwrap();
    next.release_acp_session_claim(chat.into()).await;
    let path = directory
        .path()
        .join("acp-leases")
        .join(format!("{chat}.lock"));
    std::fs::remove_file(&path).unwrap();
    let target = directory.path().join("unrelated");
    std::fs::write(&target, "unchanged").unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();
    assert!(
        first
            .claim_acp_session(chat.into(), directory.path(), directory.path())
            .await
            .is_err()
    );
    assert_eq!(std::fs::read_to_string(target).unwrap(), "unchanged");
    assert!(model.requests.lock().unwrap().is_empty());
    first.shutdown().await;
    next.shutdown().await;
}

#[tokio::test]
async fn acp_restore_checks_durable_origin_cwd_identity_root_and_trust_without_execution() {
    use tickloop_harness::runtime::transcript::JsonlTranscriptStore;
    let directory = tempfile::tempdir().unwrap();
    let cwd = directory.path().join("project");
    let root = directory.path().join("state");
    std::fs::create_dir(&cwd).unwrap();
    let model = Arc::new(ScriptedModel::new(vec![]));
    let store = Arc::new(JsonlTranscriptStore::open(root.clone()).unwrap());
    let first = harness_with_store(ToolPolicy::acp(), model.clone(), Some(store.clone()));
    let ordinary = first.create_chat(cwd.clone()).await.unwrap();
    first.flush_transcripts().await;
    assert!(
        first
            .resume_acp_session(ordinary, &cwd, &root)
            .await
            .is_err()
    );
    let chat = first.create_chat(cwd.clone()).await.unwrap();
    first
        .set_model(
            chat,
            tickloop_harness::model::cards::ModelCatalog::defaults()
                .get("deepseek-api", "deepseek-flash")
                .unwrap(),
        )
        .await
        .unwrap();
    first
        .record_new_acp_session(chat, &cwd, &root)
        .await
        .unwrap();
    first.release_acp_session_claim(chat).await;
    first.shutdown().await;
    drop(first);
    let next = harness_with_store(ToolPolicy::acp(), model.clone(), Some(store.clone()));
    let other_root = directory.path().join("other-state");
    std::fs::create_dir(&other_root).unwrap();
    assert!(
        next.resume_acp_session(chat, &cwd, &other_root)
            .await
            .is_err()
    );
    let untrusted = harness_with_host(
        ToolPolicy::acp(),
        model.clone(),
        Some(store),
        AgentHostOptions {
            directory_trust: DirectoryTrustPolicy::RequireUserTrust,
            prompt_policy: PromptPolicy::DenyOnPrompt,
            ..AgentHostOptions::embedded_defaults()
        },
    );
    assert!(
        untrusted
            .resume_acp_session(chat, &cwd, &root)
            .await
            .is_err()
    );
    let snapshot = next.resume_acp_session(chat, &cwd, &root).await.unwrap();
    assert_eq!(snapshot.id, chat);
    assert!(snapshot.messages.is_empty());
    next.release_acp_session_claim(chat).await;
    std::fs::rename(&cwd, directory.path().join("old-project")).unwrap();
    std::fs::create_dir(&cwd).unwrap();
    assert!(next.resume_acp_session(chat, &cwd, &root).await.is_err());
    assert!(model.requests.lock().unwrap().is_empty());
    next.shutdown().await;
    untrusted.shutdown().await;
}

#[tokio::test]
async fn managed_cancellation_keeps_the_chat_reusable_and_other_chats_running() {
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "slow",
            "Bash",
            json!({"command": "sleep 60", "description": "Wait for cancellation"}),
        ),
        call(
            "independent",
            "Bash",
            json!({"command": "sleep 60", "run_in_background": true, "description": "Keep other Chat running"}),
        ),
        text("other chat"),
        text("next prompt"),
    ]));
    let agent = harness(ToolPolicy::acp(), model.clone());
    agent.install_own_tickhost();
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    agent
        .submit(chat, "start a foreground sleep".into())
        .await
        .unwrap();
    // The command is listed as Running before the Tickhost answers its start; cancelling while
    // that start is in flight leaves its outcome unknown, so wait for the started process.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if agent
                .commands(chat)
                .await
                .unwrap()
                .iter()
                .any(|command| command.state == CommandState::Running && command.pid.is_some())
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let other = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(other, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let receipt = agent
        .submit(other, "independent work".into())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(15), agent.quiesce(other))
        .await
        .unwrap();
    let cancelled = agent.settle_managed(chat, true).await.unwrap();
    assert_eq!(cancelled.chat_loop.state, LoopState::Interrupted);
    assert!(
        agent
            .commands(other)
            .await
            .unwrap()
            .iter()
            .any(|command| command.state == CommandState::Background)
    );
    let snapshot = agent.settle_managed(other, false).await.unwrap();
    assert_eq!(
        snapshot.run_response_text(receipt.run_id).as_deref(),
        Some("other chat")
    );
    let next = agent
        .submit(chat, "continue with another prompt".into())
        .await
        .unwrap();
    let snapshot = agent.settle_managed(chat, false).await.unwrap();
    assert_eq!(
        snapshot.run_response_text(next.run_id).as_deref(),
        Some("next prompt")
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn only_managed_chats_record_proposed_edits_for_history_replay() {
    for (policy, records) in [(ToolPolicy::local(), false), (ToolPolicy::acp(), true)] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("edited.txt");
        let store =
            Arc::new(tickloop_harness::test_support::transcripts::InMemoryTranscriptStore::new());
        let agent = harness_with_store(
            policy,
            Arc::new(ScriptedModel::new(vec![
                call(
                    "write",
                    "Write",
                    json!({"file_path":path,"content":"edit\n"}),
                ),
                text("done"),
            ])),
            Some(store.clone()),
        );
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        agent.submit(chat, "write a file".into()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), agent.quiesce(chat))
            .await
            .unwrap();
        agent.flush_transcripts().await;
        assert_eq!(
            store
                .lines(chat)
                .iter()
                .any(|line| matches!(line, tickloop_harness::transcript::Line::RecordedEdit(_))),
            records,
            "a managed Chat records its edits, a Console Chat does not"
        );
        agent.shutdown().await;
    }
}
