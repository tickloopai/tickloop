use std::sync::Arc;
use tickloop_code_settings::local_agent::LocalAgent;

use tickloop_harness::protocol::{
    AgentHostOptions, BranchRequest, ChatId, Decision, EventKind, ExecutionMode, PermissionRule,
    Receipt, ToolResult,
};
use tickloop_harness::runtime::{Agent, AgentError};
use tickloop_harness::test_support::{
    ScriptedModel, call, config, next_event, text, transcript_file,
};
use tickloop_harness::tools::ENTER_PLAN_MODE_TOOL_NAME;
use uuid::Uuid;

/// The results of the run's tool calls, answering each permission request with `allow`.
async fn run_answering_permissions(
    agent: &Agent,
    chat: ChatId,
    prompt: &str,
    allow: bool,
) -> (Receipt, Vec<String>, Vec<(String, ToolResult)>) {
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, prompt.into()).await.unwrap();
    let mut requested = Vec::new();
    let mut results = Vec::new();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != receipt.run_id {
            continue;
        }
        match event.kind {
            EventKind::PermissionRequested { request } => {
                requested.push(request.name.clone());
                agent
                    .answer_permission(chat, request.id, allow)
                    .await
                    .unwrap();
            }
            EventKind::ToolCompleted {
                call_id, result, ..
            } => results.push((call_id, result)),
            EventKind::Completed { .. } | EventKind::Interrupted => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    (receipt, requested, results)
}

/// Changing the execution mode between requests keeps the request prefix byte for byte the same:
/// the system prompt never names the mode, and a host note at the end of the conversation tells
/// the model the new mode once.
#[tokio::test]
async fn changing_the_execution_mode_keeps_the_prefix_and_announces_the_mode_at_the_end() {
    let directory = tempfile::tempdir().unwrap();
    // The embedded host starts new Chats in Ask, so its first request needs no note.
    let modes = [
        ExecutionMode::AcceptEdits,
        ExecutionMode::Plan,
        ExecutionMode::Plan,
        ExecutionMode::Auto,
        ExecutionMode::FullAccess,
        ExecutionMode::AcceptEdits,
    ];
    let model = Arc::new(ScriptedModel::new(
        modes.iter().map(|_| text("Done.")).collect(),
    ));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    for mode in modes {
        agent.set_execution_mode(chat, mode).await.unwrap();
        run_answering_permissions(&agent, chat, "Continue", true).await;
    }
    let requests = model.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), modes.len());
    for request in &requests {
        assert_eq!(
            serde_json::to_vec(&request.messages[..2]).unwrap(),
            serde_json::to_vec(&requests[0].messages[..2]).unwrap()
        );
        assert_eq!(
            serde_json::to_vec(&request.tools).unwrap(),
            serde_json::to_vec(&requests[0].tools).unwrap()
        );
    }
    let system = serde_json::to_string(&requests[0].messages[0]).unwrap();
    for absent in ["execution_mode", "acceptEdits", "Plan mode is active"] {
        assert!(!system.contains(absent), "{absent}");
    }
    let new_notes = requests
        .iter()
        .map(|request| {
            let last = request.messages.last().unwrap();
            tickloop_harness::test_support::is_host_note(last).then(|| {
                let tickloop_harness::protocol::Content::Text { text } = &last.content[0] else {
                    unreachable!("a host note is text");
                };
                text.clone()
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(new_notes[0], None);
    assert!(
        new_notes[1]
            .as_ref()
            .unwrap()
            .contains("Plan mode is active.")
    );
    assert_eq!(
        new_notes[2], None,
        "an unchanged mode is not announced again"
    );
    let to_auto = new_notes[3].as_ref().unwrap();
    assert!(to_auto.contains("## Exited Plan Mode\n"), "{to_auto}");
    assert!(to_auto.contains("## Auto Mode Active\n"), "{to_auto}");
    assert!(
        new_notes[4]
            .as_ref()
            .unwrap()
            .contains("## Full Access Mode Active\n")
    );
    let to_ask = new_notes[5].as_ref().unwrap();
    assert!(to_ask.contains("## Ask Mode Active\n"), "{to_ask}");
    assert!(!to_ask.contains("Exited Plan Mode"), "{to_ask}");
    agent.shutdown().await;
}

#[tokio::test]
async fn the_agent_enters_plan_mode_only_after_the_user_confirms() {
    let directory = tempfile::tempdir().unwrap();
    let enter = |id| call(id, ENTER_PLAN_MODE_TOOL_NAME, serde_json::json!({}));
    let model = Arc::new(ScriptedModel::new(vec![
        enter("declined"),
        enter("full-access-declined"),
        enter("accepted"),
        text("Planning first."),
        enter("again"),
        text("Still planning."),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Auto)
        .await
        .unwrap();

    // Auto mode still asks; a bare refusal ends the run and keeps the mode.
    let (receipt, requested, results) =
        run_answering_permissions(&agent, chat, "Add caching", false).await;
    let goal = receipt.loop_id;
    assert_eq!(requested, [ENTER_PLAN_MODE_TOOL_NAME]);
    let (_, declined) = &results[0];
    assert!(declined.is_error);
    assert_eq!(declined.content, "User declined to enter plan mode.");
    assert_eq!(
        agent.snapshot(chat).await.unwrap().policy.mode,
        ExecutionMode::Auto
    );

    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let (_, requested, _) = run_answering_permissions(&agent, chat, "Plan it anyway", false).await;
    assert_eq!(requested, [ENTER_PLAN_MODE_TOOL_NAME]);
    assert_eq!(
        agent.snapshot(chat).await.unwrap().policy.mode,
        ExecutionMode::FullAccess
    );

    let (_, requested, results) =
        run_answering_permissions(&agent, chat, "Plan it first", true).await;
    assert_eq!(requested, [ENTER_PLAN_MODE_TOOL_NAME]);
    let (_, accepted) = &results[0];
    assert!(!accepted.is_error, "{}", accepted.content);
    assert!(
        accepted.content.starts_with(
            "Entered plan mode. You should now focus on exploring the codebase and designing an implementation approach."
        ),
        "{}",
        accepted.content
    );
    assert!(
        accepted
            .content
            .contains("6. When ready, use ExitPlanMode to present your plan for approval"),
        "{}",
        accepted.content
    );
    assert_eq!(
        agent.snapshot(chat).await.unwrap().policy.mode,
        ExecutionMode::Plan
    );
    let requests = model.requests.lock().unwrap().clone();
    let after_entering =
        tickloop_harness::test_support::reminded_plan_file(requests.last().unwrap());
    assert!(
        after_entering.is_some_and(|path| path.ends_with(&format!("-loop-{}.md", goal.uuid()))),
        "the request after entering plan mode names the plan file"
    );

    let (_, requested, results) =
        run_answering_permissions(&agent, chat, "Keep planning", true).await;
    assert!(requested.is_empty(), "{requested:?}");
    let (_, again) = &results[0];
    assert!(!again.is_error);
    assert!(
        again.content.starts_with("You are already in plan mode."),
        "{}",
        again.content
    );
    assert_eq!(
        agent.snapshot(chat).await.unwrap().policy.mode,
        ExecutionMode::Plan
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn entering_plan_mode_is_refused_when_prompts_are_unavailable_or_denied_by_rule() {
    let directory = tempfile::tempdir().unwrap();
    let enter = |id| call(id, ENTER_PLAN_MODE_TOOL_NAME, serde_json::json!({}));
    let headless = Agent::new_with_options(
        config(),
        Arc::new(ScriptedModel::new(vec![
            enter("headless"),
            text("Continuing."),
        ])),
        AgentHostOptions {
            prompt_policy: tickloop_harness::protocol::PromptPolicy::DenyOnPrompt,
            directory_trust: tickloop_harness::protocol::DirectoryTrustPolicy::CallerManaged,
            ..AgentHostOptions::default()
        },
    );
    let chat = headless.create_chat(directory.path().into()).await.unwrap();
    let initial_mode = headless.snapshot(chat).await.unwrap().policy.mode;
    assert_ne!(initial_mode, ExecutionMode::Plan);
    let (_, requested, results) =
        run_answering_permissions(&headless, chat, "Add caching", true).await;
    assert!(requested.is_empty());
    assert!(results[0].1.is_error);
    assert_eq!(
        headless.snapshot(chat).await.unwrap().policy.mode,
        initial_mode
    );
    headless.shutdown().await;

    let agent = Agent::new_embedded(
        config(),
        Arc::new(ScriptedModel::new(vec![
            enter("ruled"),
            text("Continuing."),
        ])),
    );
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_permission_rules(
            chat,
            vec![PermissionRule {
                id: Uuid::new_v4(),
                decision: Decision::Deny,
                tool: ENTER_PLAN_MODE_TOOL_NAME.into(),
                specifier: None,
                input: None,
                loop_id: None,
                persistent: false,
            }],
        )
        .await
        .unwrap();
    let (_, requested, results) =
        run_answering_permissions(&agent, chat, "Add caching", true).await;
    assert!(requested.is_empty());
    assert!(results[0].1.is_error);
    assert_eq!(
        agent.snapshot(chat).await.unwrap().policy.mode,
        ExecutionMode::AcceptEdits
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn agent_exposes_only_three_execution_modes() {
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new(config(), Arc::new(ScriptedModel::new(Vec::new())));
    assert!(matches!(
        agent.create_chat(directory.path().to_path_buf()).await,
        Err(AgentError::UntrustedWorkingDirectory { .. })
    ));
    agent
        .trust_working_directory(directory.path().to_path_buf())
        .await
        .unwrap();
    let chat = agent
        .create_chat(directory.path().to_path_buf())
        .await
        .unwrap();

    assert_eq!(
        agent.snapshot(chat).await.unwrap().policy.mode,
        ExecutionMode::Auto
    );

    for mode in [
        ExecutionMode::Plan,
        ExecutionMode::AcceptEdits,
        ExecutionMode::Auto,
    ] {
        agent.set_execution_mode(chat, mode).await.unwrap();
        assert_eq!(agent.snapshot(chat).await.unwrap().policy.mode, mode);
    }

    agent.shutdown().await;
}

#[tokio::test]
async fn directory_trust_precedes_chat_creation_and_survives_restart() {
    let directory = tempfile::tempdir().unwrap();
    let store = directory.path().join("state");
    let workdir = directory.path().join("workspace");
    std::fs::create_dir(&workdir).unwrap();
    let model = Arc::new(ScriptedModel::new(Vec::new()));
    let options = AgentHostOptions::product_defaults();
    assert_eq!(AgentHostOptions::default(), options);
    let agent = Agent::open_with_options(store.clone(), config(), model.clone(), options).unwrap();
    let error = agent.create_chat(workdir.clone()).await.unwrap_err();
    assert!(matches!(
        error,
        AgentError::UntrustedWorkingDirectory { .. }
    ));
    assert!(agent.list_chats().await.is_empty());
    assert!(model.requests.lock().unwrap().is_empty());

    let alias = directory.path().join("workspace-alias");
    std::os::unix::fs::symlink(&workdir, &alias).unwrap();
    agent.trust_working_directory(alias).await.unwrap();
    let chat = agent.create_chat(workdir.clone()).await.unwrap();
    assert_eq!(
        agent.snapshot(chat).await.unwrap().policy.mode,
        ExecutionMode::Auto
    );
    let other_workdir = directory.path().join("other-workspace");
    std::fs::create_dir(&other_workdir).unwrap();
    assert!(matches!(
        agent
            .set_directories(chat, other_workdir.clone(), Vec::new())
            .await,
        Err(AgentError::UntrustedWorkingDirectory { .. })
    ));
    assert_eq!(
        agent.snapshot(chat).await.unwrap().folder,
        Some(workdir.canonicalize().unwrap())
    );
    agent
        .trust_working_directory(other_workdir.clone())
        .await
        .unwrap();
    agent
        .set_directories(chat, other_workdir, Vec::new())
        .await
        .unwrap();
    agent
        .set_directories(chat, workdir.clone(), Vec::new())
        .await
        .unwrap();
    agent
        .save_default_execution_mode(ExecutionMode::Plan)
        .await
        .unwrap();
    let next = agent.create_chat(workdir.clone()).await.unwrap();
    assert_eq!(
        agent.snapshot(next).await.unwrap().policy.mode,
        ExecutionMode::Plan
    );
    assert_eq!(
        agent.snapshot(chat).await.unwrap().policy.mode,
        ExecutionMode::Auto
    );
    agent.shutdown().await;
    drop(agent);

    let restored = Agent::open_with_options(
        store,
        config(),
        Arc::new(ScriptedModel::new(Vec::new())),
        options,
    )
    .unwrap();
    let restored_chat = restored.create_chat(workdir).await.unwrap();
    assert_eq!(
        restored.snapshot(restored_chat).await.unwrap().policy.mode,
        ExecutionMode::Plan
    );
    restored.shutdown().await;
}

#[tokio::test]
async fn revoked_directory_trust_blocks_final_side_effect_admission() {
    let directory = tempfile::tempdir().unwrap();
    let store = directory.path().join("state");
    let workdir = directory.path().join("workspace");
    std::fs::create_dir(&workdir).unwrap();
    let agent = Agent::open(
        store.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![
            tickloop_harness::test_support::call(
                "write",
                "Write",
                serde_json::json!({"file_path":"blocked.txt","content":"blocked"}),
            ),
        ])),
    )
    .unwrap();
    agent
        .trust_working_directory(workdir.clone())
        .await
        .unwrap();
    let chat = agent.create_chat(workdir.clone()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    agent
        .set_permission_rules(
            chat,
            vec![PermissionRule {
                id: Uuid::new_v4(),
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
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Create blocked.txt".into())
        .await
        .unwrap();
    let request = loop {
        if let EventKind::PermissionRequested { request } = next_event(&mut events).await.kind {
            break request;
        }
    };

    let settings_path = store.join("settings.json");
    let mut settings: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
    let project_key = workdir
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    settings["projects"][project_key]["hasTrustDialogAccepted"] = serde_json::Value::Bool(false);
    std::fs::write(
        &settings_path,
        serde_json::to_vec_pretty(&settings).unwrap(),
    )
    .unwrap();
    agent
        .answer_permission(chat, request.id, true)
        .await
        .unwrap();

    loop {
        match next_event(&mut events).await.kind {
            EventKind::Failed(error) => {
                assert!(error.contains("working directory is not trusted"));
                break;
            }
            EventKind::ToolStarted { .. } => panic!("revoked trust admitted a side effect"),
            _ => {}
        }
    }
    assert!(!workdir.join("blocked.txt").exists());
    agent.shutdown().await;
}

#[tokio::test]
async fn resume_branch_and_process_override_use_current_defaults_not_temporary_chat_mode() {
    let directory = tempfile::tempdir().unwrap();
    let store = directory.path().join("state");
    let workdir = directory.path().join("workspace");
    std::fs::create_dir(&workdir).unwrap();
    let product = AgentHostOptions::product_defaults();
    let agent = Agent::open_with_options(
        store.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![text("recorded")])),
        product,
    )
    .unwrap();
    agent
        .trust_working_directory(workdir.clone())
        .await
        .unwrap();
    agent
        .save_default_execution_mode(ExecutionMode::Auto)
        .await
        .unwrap();
    let source = agent.create_chat(workdir.clone()).await.unwrap();
    agent
        .set_execution_mode(source, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(source, "Record this conversation".into())
        .await
        .unwrap();
    loop {
        if matches!(
            next_event(&mut events).await.kind,
            EventKind::Completed { .. }
        ) {
            break;
        }
    }
    agent.shutdown().await;
    drop(agent);

    let reopened = Agent::open_with_options(
        store.clone(),
        config(),
        Arc::new(ScriptedModel::new(Vec::new())),
        product,
    )
    .unwrap();
    let resumed = reopened.resume_chat(source).await.unwrap();
    assert_eq!(resumed.policy.mode, ExecutionMode::Auto);
    reopened
        .set_execution_mode(source, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    let branch = reopened
        .branch_chat(BranchRequest {
            request_id: Uuid::new_v4(),
            source_chat_id: source,
            name: Some("default mode branch".into()),
        })
        .await
        .unwrap();
    assert_eq!(branch.snapshot.policy.mode, ExecutionMode::Auto);
    assert_eq!(
        reopened.snapshot(source).await.unwrap().policy.mode,
        ExecutionMode::AcceptEdits
    );
    reopened.shutdown().await;
    drop(reopened);

    let overridden = Agent::open_with_options(
        store,
        config(),
        Arc::new(ScriptedModel::new(Vec::new())),
        AgentHostOptions {
            execution_mode_override: Some(ExecutionMode::Plan),
            ..AgentHostOptions::product_defaults()
        },
    )
    .unwrap();
    assert_eq!(
        overridden.resume_chat(source).await.unwrap().policy.mode,
        ExecutionMode::Plan
    );
    overridden.shutdown().await;
}

#[test]
fn execution_mode_json_accepts_only_current_names() {
    for (mode, encoded) in [
        (ExecutionMode::Plan, "\"plan\""),
        (ExecutionMode::AcceptEdits, "\"acceptEdits\""),
        (ExecutionMode::Auto, "\"auto\""),
    ] {
        assert_eq!(serde_json::to_string(&mode).unwrap(), encoded);
        assert_eq!(
            serde_json::from_str::<ExecutionMode>(encoded).unwrap(),
            mode
        );
    }

    for legacy in ["default", "manual", "dontAsk", "bypass"] {
        assert!(
            serde_json::from_value::<ExecutionMode>(legacy.into()).is_err(),
            "legacy mode {legacy} must remain confined to Transcript compatibility"
        );
    }
}

#[tokio::test]
async fn legacy_transcript_modes_are_mapped_conservatively() {
    let directory = tempfile::tempdir().unwrap();
    let cases = [
        (
            serde_json::json!("default"),
            ExecutionMode::AcceptEdits,
            true,
        ),
        (
            serde_json::json!("acceptEdits"),
            ExecutionMode::AcceptEdits,
            false,
        ),
        (serde_json::json!("dontAsk"), ExecutionMode::Plan, true),
        (serde_json::json!("bypass"), ExecutionMode::Plan, true),
        (
            serde_json::json!("manual"),
            ExecutionMode::AcceptEdits,
            true,
        ),
        (serde_json::json!("plan"), ExecutionMode::Plan, false),
        (serde_json::json!("auto"), ExecutionMode::Auto, false),
        (serde_json::json!("futureMode"), ExecutionMode::Plan, true),
        (
            serde_json::json!({"mode":"bypass"}),
            ExecutionMode::Plan,
            true,
        ),
    ];

    for (index, (stored_mode, expected, should_warn)) in cases.into_iter().enumerate() {
        let root = directory.path().join(format!("state-{index}"));
        let first = Agent::open_embedded(
            root.clone(),
            config(),
            Arc::new(ScriptedModel::new(vec![text("Recorded")])),
        )
        .unwrap();
        let chat = first
            .create_chat(directory.path().to_path_buf())
            .await
            .unwrap();
        let mut events = first.subscribe();
        let receipt = first.submit(chat, "Remember this".into()).await.unwrap();
        loop {
            let event = next_event(&mut events).await;
            if event.run_id == receipt.run_id && matches!(event.kind, EventKind::Completed { .. }) {
                break;
            }
        }
        first.shutdown().await;

        let path = transcript_file(&root, chat.uuid());
        let transcript = std::fs::read_to_string(&path).unwrap();
        assert!(
            transcript.contains("\"executionMode\":\"acceptEdits\""),
            "{transcript}"
        );
        assert!(!transcript.contains("\"permissionMode\""));
        let rewritten = transcript
            .lines()
            .map(|line| {
                let mut value = serde_json::from_str::<serde_json::Value>(line).unwrap();
                if value.get("type").and_then(serde_json::Value::as_str) == Some("user")
                    && value.get("executionMode").is_some()
                {
                    value.as_object_mut().unwrap().remove("executionMode");
                    value["permissionMode"] = stored_mode.clone();
                }
                serde_json::to_string(&value).unwrap()
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, format!("{rewritten}\n")).unwrap();

        let restored =
            Agent::open_embedded(root, config(), Arc::new(ScriptedModel::new(Vec::new()))).unwrap();
        let snapshot = restored.resume_chat(chat).await.unwrap();
        assert_eq!(snapshot.policy.mode, ExecutionMode::AcceptEdits);
        assert_eq!(snapshot.rewind_points()[0].execution_mode, expected);
        assert_eq!(
            !restored.transcript_load_warnings(chat).is_empty(),
            should_warn,
            "stored mode: {stored_mode}"
        );
        restored.shutdown().await;
    }
}
