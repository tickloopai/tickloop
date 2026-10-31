//! Deny and Ask Permission Rules see through wrappers, program paths and command substitutions,
//! while Allow rules keep admitting only the commands they name.

use std::{sync::Arc, time::Duration};

use serde_json::json;
use tickloop_harness_protocol::{
    AgentHostOptions, Decision, EventKind, ExecutionMode, PermissionRule, ToolResult,
};
use tickloop_harness_runtime::{
    Agent, HarnessOptions, HarnessSeams, environment::memory::MemoryEnvironment,
};
use tickloop_harness_test_support::{
    ScriptedModel, auto_classification, call, config, harness::ChosenModels, next_event, text,
};

const RAN: &[u8] = b"command-ran";
const WRAPPED_PUSHES: [&str; 4] = [
    "env GIT_TRACE=1 git push origin main",
    "/usr/bin/git push origin main",
    "echo \"$(git push origin main)\"",
    "timeout 60 git push origin main",
];

struct Observed {
    asked: bool,
    reviewed: bool,
    result: ToolResult,
}

async fn run(mode: ExecutionMode, rule: (Decision, &str), command: &str) -> Observed {
    let folder = tempfile::tempdir().unwrap();
    let environment = MemoryEnvironment::new();
    environment.define_shell(command, Duration::ZERO, RAN.to_vec(), 0);
    let model = Arc::new(ScriptedModel::new(vec![
        call("shell", "Bash", json!({ "command": command })),
        text("done"),
    ]));
    let agent = Agent::with_seams(
        HarnessSeams {
            search: None,
            transcripts: None,
            environment: Some(Arc::new(environment)),
            models: ChosenModels::new(model),
        },
        HarnessOptions {
            host: AgentHostOptions::embedded_defaults(),
            ..HarnessOptions::local(config())
        },
    )
    .unwrap();
    let approving_reviewer = Arc::new(ScriptedModel::new(vec![text(
        &auto_classification(false).to_string(),
    )]));
    agent.set_auto_reviewer(approving_reviewer.clone()).await;
    let chat = agent.create_chat(folder.path().into()).await.unwrap();
    agent.set_execution_mode(chat, mode).await.unwrap();
    let (decision, pattern) = rule;
    agent
        .set_permission_rules(
            chat,
            vec![PermissionRule {
                id: tickloop_ids::generate(),
                decision,
                tool: "Bash".into(),
                specifier: Some(pattern.into()),
                input: None,
                loop_id: None,
                persistent: false,
            }],
        )
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, "Run it.".into()).await.unwrap();
    let mut asked = false;
    let mut result = None;
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
            EventKind::ToolCompleted { result: value, .. } => result = Some(value),
            EventKind::Completed { .. } | EventKind::Interrupted => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    agent.shutdown().await;
    let reviewed = !approving_reviewer.requests.lock().unwrap().is_empty();
    Observed {
        asked,
        reviewed,
        result: result.expect("Bash returns a result"),
    }
}

fn ran(observed: &Observed) -> bool {
    observed.result.content.contains("command-ran")
}

#[tokio::test]
async fn a_deny_rule_refuses_wrapped_and_substituted_commands_even_in_full_access() {
    for command in WRAPPED_PUSHES {
        let observed = run(
            ExecutionMode::FullAccess,
            (Decision::Deny, "git push*"),
            command,
        )
        .await;
        assert!(!observed.asked, "{command}");
        assert!(observed.result.is_error, "{command}: {:?}", observed.result);
        assert!(!ran(&observed), "{command}");
    }
}

#[tokio::test]
async fn an_ask_rule_asks_about_wrapped_and_substituted_commands_even_in_full_access() {
    for command in WRAPPED_PUSHES {
        let observed = run(
            ExecutionMode::FullAccess,
            (Decision::Ask, "git push*"),
            command,
        )
        .await;
        assert!(observed.asked, "{command}");
        assert!(!ran(&observed), "{command}");
    }
}

#[tokio::test]
async fn in_auto_an_ask_rule_asks_about_a_wrapped_command_without_the_reviewer() {
    for command in WRAPPED_PUSHES {
        let observed = run(ExecutionMode::Auto, (Decision::Ask, "git push*"), command).await;
        assert!(observed.asked, "{command}");
        assert!(!observed.reviewed, "{command}");
        assert!(!ran(&observed), "{command}");
    }
}

#[tokio::test]
async fn in_auto_unrelated_ask_rules_leave_ordinary_commands_to_the_reviewer() {
    for command in ["echo \"built at $(date)\"", "env RUST_LOG=debug cargo test"] {
        let observed = run(ExecutionMode::Auto, (Decision::Ask, "git push*"), command).await;
        assert!(!observed.asked, "{command}");
        assert!(observed.reviewed, "{command}");
        assert!(ran(&observed), "{command}: {:?}", observed.result);
    }
}

#[tokio::test]
async fn unrelated_deny_and_ask_rules_let_ordinary_commands_run() {
    for decision in [Decision::Deny, Decision::Ask] {
        for command in [
            "echo \"built at $(date)\"",
            "env RUST_LOG=debug cargo test",
            "/usr/bin/git status",
        ] {
            let observed = run(ExecutionMode::FullAccess, (decision, "git push*"), command).await;
            assert!(!observed.asked, "{decision:?} {command}");
            assert!(
                ran(&observed),
                "{decision:?} {command}: {:?}",
                observed.result
            );
        }
    }
}

#[tokio::test]
async fn an_allow_rule_still_admits_only_the_commands_it_names() {
    let named = run(
        ExecutionMode::AcceptEdits,
        (Decision::Allow, "git push*"),
        "git push origin main",
    )
    .await;
    assert!(!named.asked);
    assert!(ran(&named), "{:?}", named.result);
    for command in WRAPPED_PUSHES {
        let observed = run(
            ExecutionMode::AcceptEdits,
            (Decision::Allow, "git push*"),
            command,
        )
        .await;
        assert!(observed.asked, "{command}");
        assert!(!ran(&observed), "{command}");
    }
}
