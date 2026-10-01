mod support;

use std::{
    io::Write,
    process::{Command, Stdio},
    sync::Arc,
};
use tickloop_agent::runtime::{Agent, EventKind};

#[test]
fn retired_commands_do_not_execute_or_become_prompts() {
    let directory = tempfile::tempdir().unwrap();
    let retired = [
        "/new",
        "/goal",
        "/theme",
        "/logout",
        "/triggers",
        "/schedule 60 check",
        "/checkpoint",
        "/undo",
        "/effort high",
        "/config",
        "/jev",
        "/commands",
        "/bashes",
        "/subs",
        "/quit",
    ];
    let runtime_directory = support::cli_runtime::directory();
    let mut child = Command::new(env!("CARGO_BIN_EXE_tickloop"))
        .env("TICKLOOP_AGENT_RUNTIME_DIR", &runtime_directory)
        .args(["--working-directory-is-trusted", "--no-session-persistence"])
        .current_dir(directory.path())
        .env("TICKLOOP_CONFIG_DIR", directory.path().join("state"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{}\n/help advanced\n/status\n/exit\n", retired.join("\n")).as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success(), "{stdout}");
    assert_eq!(
        stdout.matches("Unknown command; use /help.").count(),
        retired.len(),
        "{stdout}"
    );
    assert!(stdout.contains("/rule allow|ask|deny"));
    assert!(stdout.contains("/interrupt"));
    // No retired command created an Agent or queued input: the New Agent screen still has none.
    assert!(stdout.contains("No Chat is selected."), "{stdout}");
    assert!(!stdout.contains("Chat: "), "{stdout}");
    assert!(!stdout.contains("Accepted: Goal"));
    assert!(!stdout.contains("Codex login removed"));
    use tickloop_agent::agent_access::{
        AgentAccess as _, LocalAgentClient, ObservationRequest, ObservationValue, Page,
    };
    let catalog = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let client = LocalAgentClient::connect(runtime_directory.join("owner.sock"))
                .await
                .unwrap();
            client
                .observe(ObservationRequest::Overview {
                    filter: Default::default(),
                    page: Page {
                        offset: 0,
                        limit: 50,
                    },
                    after: None,
                })
                .await
                .unwrap()
                .initial
        });
    let ObservationValue::Overview(agents) = catalog else {
        panic!("expected the owner catalog");
    };
    assert!(agents.is_empty(), "{agents:?}");
}

#[tokio::test]
async fn loops_pipe_controls_preserve_history_scope_and_trigger_capabilities() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let archive = root.join("state");
    let model = Arc::new(support::ScriptedModel::new(vec![
        support::text("First answer"),
        support::text("Other answer"),
    ]));
    let agent = Agent::open_embedded(archive.clone(), support::config(), model).unwrap();
    let chat = agent.create_chat(root.clone()).await.unwrap();
    let other_chat = agent.create_chat(root.clone()).await.unwrap();
    let mut events = agent.subscribe();
    let mut goals = Vec::new();
    for chat in [chat, other_chat] {
        let receipt = agent
            .submit(chat, None, "Keep this goal".into())
            .await
            .unwrap();
        goals.push(receipt.loop_id);
        loop {
            match support::next_event(&mut events).await.kind {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
    }
    let before = agent.snapshot(chat).await.unwrap();
    agent.shutdown().await;
    drop(agent);
    let goal = goals[0];
    let other_goal = goals[1];
    let commands = format!(
        "/loops {goal}\n/events keep explicit events\n/loops schedule {goal} 86400 --for 604800 scheduled review\n/loops trigger {goal}\n/loops schedule {other_goal} 86400 wrong chat\n/loops {other_goal}\n/loops schedule {goal} 0 invalid\n/loops trigger\n/loops new\n/loops\n/exit\n"
    );
    let runtime_directory = support::cli_runtime::directory();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_tickloop"))
        .env("TICKLOOP_AGENT_RUNTIME_DIR", &runtime_directory)
        .args([
            "--working-directory-is-trusted",
            "--chat-id",
            &chat.to_string(),
        ])
        .current_dir(&root)
        .env("TICKLOOP_CONFIG_DIR", &archive)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    tokio::io::AsyncWriteExt::write_all(&mut child.stdin.take().unwrap(), commands.as_bytes())
        .await
        .unwrap();
    let output = tokio::time::timeout(std::time::Duration::from_secs(15), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        output.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("explicit events true"), "{stdout}");
    assert!(stdout.contains("Interval: 86400 seconds"), "{stdout}");
    assert_eq!(
        stdout
            .matches("Goal does not belong to the current Chat")
            .count(),
        2,
        "{stdout}"
    );
    assert!(
        stdout.contains("interval must be a positive number"),
        "{stdout}"
    );
    assert!(
        stdout.contains("next input will start a new goal in this Chat"),
        "{stdout}"
    );
    assert!(!stdout.contains("Accepted: Goal"), "{stdout}");
    support::cli_runtime::flush_transcripts(&runtime_directory)
        .await
        .unwrap();
    let agent = Agent::open_embedded(
        archive,
        support::config(),
        Arc::new(support::ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    let after = agent.resume_chat(chat).await.unwrap();
    assert_eq!(after.messages.len(), before.messages.len());
    assert_eq!(after.loops.len(), 1);
    let trigger = after.loops[0].trigger.as_ref().unwrap();
    assert_eq!(trigger.interval_seconds, Some(86400));
    assert!(trigger.expires_at_ms.is_some());
    assert!(trigger.accepts_events);
    assert_eq!(trigger.prompt, "scheduled review");
    assert!(
        agent.resume_chat(other_chat).await.unwrap().loops[0]
            .trigger
            .is_none()
    );
    agent.shutdown().await;
}
