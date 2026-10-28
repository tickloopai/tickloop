use std::{sync::Arc, time::Duration};

use serde_json::{Value, json};
use tickloop_harness_protocol::{
    AgentHostOptions, ApprovalPath, ApprovalRecord, Decision, DirectApprovalReason, EventKind,
    ExecutionMode, PermissionRule, PromptPolicy, ToolResult,
};
use tickloop_harness_runtime::{
    Agent, HarnessOptions, HarnessSeams, SubagentLaunch, ToolPolicy,
    environment::{
        ExecutionIsolation, NetworkIsolation, WriteIsolation, memory::MemoryEnvironment,
    },
};
use tickloop_harness_test_support::{
    ScriptedModel, auto_classification, call, config, harness::ChosenModels, next_event, text,
};

const BUILD: &str = "cargo build -p sandboxed-fixture";
const LOCAL_EDIT: &str = "mkdir out";
const FOLDER_ONLY_OFFLINE: ExecutionIsolation = ExecutionIsolation {
    writes: WriteIsolation::FolderAndDirectories,
    network: NetworkIsolation::Disconnected,
};

struct Scenario {
    isolation: ExecutionIsolation,
    skips_approval_when_isolated: bool,
    requires_approval: bool,
    mode: ExecutionMode,
    rule: Option<(Decision, Option<&'static str>)>,
    prompt_policy: PromptPolicy,
    input: Value,
}

impl Scenario {
    fn sandboxed(mode: ExecutionMode) -> Self {
        Self {
            isolation: FOLDER_ONLY_OFFLINE,
            skips_approval_when_isolated: true,
            requires_approval: false,
            mode,
            rule: None,
            prompt_policy: AgentHostOptions::embedded_defaults().prompt_policy,
            input: json!({"command":BUILD}),
        }
    }
}

struct Observed {
    reviewed: bool,
    asked: bool,
    result: ToolResult,
    approval: Option<ApprovalRecord>,
}

async fn run(scenario: Scenario) -> Observed {
    let folder = tempfile::tempdir().unwrap();
    let environment = MemoryEnvironment::new();
    environment.set_isolation(scenario.isolation);
    environment.set_shell_approval_skipped_when_isolated(scenario.skips_approval_when_isolated);
    environment.set_shell_approval_required(scenario.requires_approval);
    environment.define_shell(BUILD, Duration::ZERO, b"build-finished".to_vec(), 0);
    environment.define_shell(LOCAL_EDIT, Duration::ZERO, Vec::new(), 0);
    let model = Arc::new(ScriptedModel::new(vec![
        call("build", "Bash", scenario.input),
        text("done"),
    ]));
    let blocking_reviewer = Arc::new(ScriptedModel::new(vec![
        text(&auto_classification(true).to_string()),
        text(&auto_classification(true).to_string()),
    ]));
    let agent = Agent::with_seams(
        HarnessSeams {
            search: None,
            transcripts: None,
            environment: Some(Arc::new(environment)),
            models: ChosenModels::new(model),
        },
        HarnessOptions {
            host: AgentHostOptions {
                prompt_policy: scenario.prompt_policy,
                ..AgentHostOptions::embedded_defaults()
            },
            ..HarnessOptions::local(config())
        },
    )
    .unwrap();
    agent.set_auto_reviewer(blocking_reviewer.clone()).await;
    let chat = agent.create_chat(folder.path().into()).await.unwrap();
    agent.set_execution_mode(chat, scenario.mode).await.unwrap();
    if let Some((decision, specifier)) = scenario.rule {
        agent
            .set_permission_rules(
                chat,
                vec![PermissionRule {
                    id: tickloop_ids::generate(),
                    decision,
                    tool: "Bash".into(),
                    specifier: specifier.map(Into::into),
                    input: None,
                    loop_id: None,
                    persistent: false,
                }],
            )
            .await
            .unwrap();
    }
    let mut events = agent.subscribe();
    let receipt = agent
        .submit(chat, "Build the project.".into())
        .await
        .unwrap();
    let mut asked = false;
    let mut result = None;
    let mut approval = None;
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != receipt.run_id {
            continue;
        }
        match event.kind {
            EventKind::PermissionRequested { request } => {
                asked = true;
                agent
                    .answer_permission(chat, request.id, false)
                    .await
                    .unwrap();
            }
            EventKind::ApprovalRecorded { record } => approval = Some(*record),
            EventKind::ToolCompleted { result: value, .. } => result = Some(value),
            EventKind::Completed { .. } | EventKind::Interrupted => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    agent.shutdown().await;
    let reviewed = !blocking_reviewer.requests.lock().unwrap().is_empty();
    Observed {
        reviewed,
        asked,
        result: result.expect("Bash returns a result"),
        approval,
    }
}

#[tokio::test]
async fn sandboxed_build_runs_in_auto_without_calling_the_reviewer() {
    let observed = run(Scenario::sandboxed(ExecutionMode::Auto)).await;
    assert!(!observed.reviewed);
    assert!(!observed.asked);
    assert!(!observed.result.is_error, "{:?}", observed.result);
    assert!(observed.result.content.contains("build-finished"));
    let approval = observed.approval.unwrap();
    assert_eq!(approval.path, ApprovalPath::Direct);
    assert_eq!(approval.direct_reason, Some(DirectApprovalReason::Sandbox));
}

#[tokio::test]
async fn only_the_sandbox_fast_path_records_the_sandbox_reason() {
    let unsandboxed_edit = run(Scenario {
        isolation: ExecutionIsolation::default(),
        input: json!({"command":LOCAL_EDIT}),
        ..Scenario::sandboxed(ExecutionMode::AcceptEdits)
    })
    .await;
    assert!(
        !unsandboxed_edit.result.is_error,
        "{:?}",
        unsandboxed_edit.result
    );
    let approval = unsandboxed_edit.approval.unwrap();
    assert_eq!(approval.path, ApprovalPath::Direct);
    assert_eq!(approval.direct_reason, None);

    let full_access = run(Scenario::sandboxed(ExecutionMode::FullAccess)).await;
    assert_eq!(full_access.approval.unwrap().direct_reason, None);

    for (isolation, expected) in [
        (FOLDER_ONLY_OFFLINE, Some(DirectApprovalReason::Sandbox)),
        (ExecutionIsolation::default(), None),
    ] {
        let allowed_by_rule = run(Scenario {
            isolation,
            rule: Some((Decision::Allow, Some("cargo:*"))),
            ..Scenario::sandboxed(ExecutionMode::AcceptEdits)
        })
        .await;
        assert!(!allowed_by_rule.result.is_error, "{isolation:?}");
        assert_eq!(
            allowed_by_rule.approval.unwrap().direct_reason,
            expected,
            "{isolation:?}"
        );
    }
}

#[tokio::test]
async fn shells_that_require_approval_never_skip_it_despite_reported_isolation() {
    for mode in [ExecutionMode::Auto, ExecutionMode::AcceptEdits] {
        let observed = run(Scenario {
            requires_approval: true,
            ..Scenario::sandboxed(mode)
        })
        .await;
        assert_eq!(observed.reviewed, mode == ExecutionMode::Auto, "{mode:?}");
        assert_eq!(
            observed.asked,
            mode == ExecutionMode::AcceptEdits,
            "{mode:?}"
        );
        assert!(observed.result.is_error, "{mode:?}");
    }
}

#[tokio::test]
async fn only_folder_isolation_without_open_network_skips_approval_in_accept_edits_and_auto() {
    use NetworkIsolation::{Allowlist, Disconnected, Unrestricted as OpenNetwork};
    use WriteIsolation::{Environment, FolderAndDirectories, Unrestricted as AnyWrites};
    for (writes, network) in [
        (AnyWrites, OpenNetwork),
        (AnyWrites, Disconnected),
        (FolderAndDirectories, OpenNetwork),
        (FolderAndDirectories, Allowlist),
        (FolderAndDirectories, Disconnected),
        (Environment, Disconnected),
    ] {
        for skips_approval_when_isolated in [true, false] {
            for mode in [
                ExecutionMode::Auto,
                ExecutionMode::AcceptEdits,
                ExecutionMode::Plan,
            ] {
                let observed = run(Scenario {
                    isolation: ExecutionIsolation { writes, network },
                    skips_approval_when_isolated,
                    ..Scenario::sandboxed(mode)
                })
                .await;
                let case = format!(
                    "writes={writes:?} network={network:?} auto={skips_approval_when_isolated} mode={mode:?}"
                );
                let skips_approval = skips_approval_when_isolated
                    && writes == FolderAndDirectories
                    && network != OpenNetwork
                    && mode != ExecutionMode::Plan;
                assert_eq!(
                    observed.reviewed,
                    !skips_approval && mode == ExecutionMode::Auto,
                    "{case}"
                );
                assert_eq!(
                    observed.asked,
                    !skips_approval && mode == ExecutionMode::AcceptEdits,
                    "{case}"
                );
                assert_eq!(observed.result.is_error, !skips_approval, "{case}");
            }
        }
    }
}

#[tokio::test]
async fn explicit_ask_and_deny_rules_still_apply_to_sandboxed_commands() {
    for mode in [ExecutionMode::Auto, ExecutionMode::AcceptEdits] {
        for specifier in [None, Some("cargo:*")] {
            let asked = run(Scenario {
                rule: Some((Decision::Ask, specifier)),
                ..Scenario::sandboxed(mode)
            })
            .await;
            assert!(asked.asked, "{mode:?} {specifier:?}");
            assert!(!asked.reviewed, "{mode:?} {specifier:?}");
            assert!(asked.result.is_error, "{mode:?} {specifier:?}");

            let denied = run(Scenario {
                rule: Some((Decision::Deny, specifier)),
                ..Scenario::sandboxed(mode)
            })
            .await;
            assert!(!denied.asked && !denied.reviewed, "{mode:?} {specifier:?}");
            assert!(denied.result.is_error, "{mode:?} {specifier:?}");
        }
    }
}

#[tokio::test]
async fn deny_on_prompt_runs_sandboxed_commands_and_denies_the_rest() {
    for mode in [ExecutionMode::Auto, ExecutionMode::AcceptEdits] {
        for isolation in [FOLDER_ONLY_OFFLINE, ExecutionIsolation::default()] {
            let observed = run(Scenario {
                isolation,
                prompt_policy: PromptPolicy::DenyOnPrompt,
                ..Scenario::sandboxed(mode)
            })
            .await;
            let sandboxed = isolation == FOLDER_ONLY_OFFLINE;
            assert!(!observed.asked, "{mode:?} {isolation:?}");
            assert_eq!(
                observed.result.is_error, !sandboxed,
                "{mode:?} {isolation:?}"
            );
            if sandboxed {
                assert!(observed.result.content.contains("build-finished"));
            }
        }
    }
}

#[tokio::test]
async fn background_shells_skip_approval_like_foreground_bash() {
    for mode in [ExecutionMode::Auto, ExecutionMode::AcceptEdits] {
        for isolation in [FOLDER_ONLY_OFFLINE, ExecutionIsolation::default()] {
            let observed = run(Scenario {
                isolation,
                input: json!({"command":BUILD, "run_in_background":true}),
                ..Scenario::sandboxed(mode)
            })
            .await;
            let sandboxed = isolation == FOLDER_ONLY_OFFLINE;
            assert_eq!(observed.reviewed || observed.asked, !sandboxed, "{mode:?}");
            assert_eq!(
                observed.result.is_error, !sandboxed,
                "{:?}",
                observed.result
            );
        }
    }
}

#[tokio::test]
async fn subagent_bash_skips_approval_under_the_frozen_parent_permissions() {
    use tickloop_harness_test_support::routed::{RoutedModel, has_tool_result, state};
    for (mode, isolation) in [
        (ExecutionMode::Auto, FOLDER_ONLY_OFFLINE),
        (ExecutionMode::Auto, ExecutionIsolation::default()),
        (ExecutionMode::AcceptEdits, FOLDER_ONLY_OFFLINE),
        (ExecutionMode::AcceptEdits, ExecutionIsolation::default()),
    ] {
        let folder = tempfile::tempdir().unwrap();
        let environment = MemoryEnvironment::new();
        environment.set_isolation(isolation);
        environment.define_shell(BUILD, Duration::ZERO, b"build-finished".to_vec(), 0);
        let model = Arc::new(RoutedModel::new(|request| {
            if !state(request)["delegation"].is_null() {
                if has_tool_result(request, "child-build") {
                    return text("Child finished");
                }
                return call("child-build", "Bash", json!({"command":BUILD}));
            }
            if has_tool_result(request, "delegate") {
                return text("Root finished");
            }
            call(
                "delegate",
                "Subagent",
                json!({"task":"Build the project", "name":"builder", "run_in_background":false}),
            )
        }));
        let build_reviews = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let reviewer = Arc::new(RoutedModel::new({
            let build_reviews = build_reviews.clone();
            move |request| {
                let reviews_build = serde_json::to_string(&request.messages)
                    .unwrap()
                    .contains(BUILD);
                if reviews_build {
                    build_reviews.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                text(&auto_classification(reviews_build).to_string())
            }
        }));
        let agent = Agent::with_seams(
            HarnessSeams {
                search: None,
                transcripts: None,
                environment: Some(Arc::new(environment)),
                models: ChosenModels::new(model),
            },
            HarnessOptions {
                host: AgentHostOptions {
                    prompt_policy: PromptPolicy::DenyOnPrompt,
                    ..AgentHostOptions::embedded_defaults()
                },
                // The root waits for its child's result, which only a non-interactive host
                // offers.
                tools: ToolPolicy {
                    subagent_launch: SubagentLaunch::BackgroundByDefault,
                    ..ToolPolicy::local()
                },
                ..HarnessOptions::local(config())
            },
        )
        .unwrap();
        agent.set_auto_reviewer(reviewer).await;
        let root = agent.create_chat(folder.path().into()).await.unwrap();
        agent.set_execution_mode(root, mode).await.unwrap();
        let mut events = agent.subscribe();
        let receipt = agent
            .submit(root, "Delegate the build".into())
            .await
            .unwrap();
        let mut child_result = None;
        loop {
            let event = next_event(&mut events).await;
            match event.kind {
                EventKind::ToolCompleted {
                    call_id, result, ..
                } if call_id == "child-build" => child_result = Some(result),
                EventKind::Completed { .. } if event.run_id == receipt.run_id => break,
                EventKind::Failed(error) if event.run_id == receipt.run_id => panic!("{error}"),
                _ => {}
            }
        }
        agent.shutdown().await;
        let case = format!("{mode:?} {isolation:?}");
        let skips_approval = isolation == FOLDER_ONLY_OFFLINE;
        let child_result = child_result.unwrap_or_else(|| panic!("child Bash ran: {case}"));
        assert_eq!(
            child_result.is_error, !skips_approval,
            "{case}: {child_result:?}"
        );
        assert_eq!(
            build_reviews.load(std::sync::atomic::Ordering::SeqCst) == 0,
            skips_approval || mode != ExecutionMode::Auto,
            "{case}"
        );
    }
}
