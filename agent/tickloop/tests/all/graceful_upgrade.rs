use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use tickloop_agent_client::{LocalAgentClient, connect_or_start_at};
use tickloop_agent_protocol::{
    AgentAccess, AgentCommand, AgentDetailQuery, CommandEnvelope, CommandResult, EnvironmentValue,
    ExecutionRef, LaunchSpec, ObservationRequest, ObservationValue, RuntimeVersion, UserInput,
    UserShellState,
};
use uuid::Uuid;

use crate::support;

pub(super) struct PrivateRuntime(pub(super) tempfile::TempDir);
impl Drop for PrivateRuntime {
    fn drop(&mut self) {
        let directory = self.0.path().to_path_buf();
        let _ = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                if let Ok(client) = LocalAgentClient::connect(directory.join("daemon.sock")).await {
                    let _ = client.shutdown_daemon().await;
                }
            });
            support::stop_tickhost(&directory.join("tickhost"), &directory.join("tickhost.key"));
        })
        .join();
    }
}

pub(super) fn command(epoch: Uuid, command: AgentCommand) -> CommandEnvelope {
    CommandEnvelope {
        daemon_epoch: epoch,
        command_id: Uuid::new_v4(),
        command,
    }
}

pub(super) async fn version(client: &LocalAgentClient) -> RuntimeVersion {
    match client
        .observe(ObservationRequest::RuntimeVersion)
        .await
        .unwrap()
        .initial
    {
        ObservationValue::RuntimeVersion(version) => *version,
        other => panic!("unexpected runtime version: {other:?}"),
    }
}

pub(super) async fn snapshot(
    client: &LocalAgentClient,
    execution: ExecutionRef,
) -> tickloop_agent_protocol::LiveChatSnapshot {
    match client
        .observe(ObservationRequest::Agent {
            execution,
            detail: AgentDetailQuery::Snapshot,
            after: None,
        })
        .await
        .unwrap()
        .initial
    {
        ObservationValue::Agent(snapshot) => *snapshot,
        other => panic!("unexpected Agent snapshot: {other:?}"),
    }
}

pub(super) fn copy_bundle(source: &Path, directory: &Path) -> PathBuf {
    std::fs::create_dir(directory).unwrap();
    let executable = directory.join("tickloop");
    std::fs::copy(source, &executable).unwrap();
    let source_host = source.with_file_name("tickhost");
    std::fs::copy(
        if source_host.is_file() {
            source_host
        } else {
            support::fixture_cli::tickhost()
        },
        directory.join("tickhost"),
    )
    .unwrap();
    let warm = std::process::Command::new(&executable)
        .arg("--runtime-info")
        .output()
        .unwrap();
    assert!(
        warm.status.success(),
        "cannot warm runtime binary: {warm:?}"
    );
    let warm_host = std::process::Command::new(directory.join("tickhost"))
        .arg("--build-identity")
        .output()
        .unwrap();
    assert!(
        warm_host.status.success(),
        "cannot warm Tickhost binary: {warm_host:?}"
    );
    executable.canonicalize().unwrap()
}

#[tokio::test]
async fn daemon_and_worker_upgrade_preserve_background_shell_and_receipts() {
    let executable = support::fixture_cli::executable();
    run_probe(&executable, &executable).await;
}

pub(crate) async fn run_probe(old_source: &Path, new_source: &Path) {
    let runtime = PrivateRuntime(support::runtime_dir());
    let bundles = tempfile::tempdir().unwrap();
    let workdir = tempfile::tempdir().unwrap();
    let reply = |text: &str| {
        format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            serde_json::json!({"id":"upgrade-model","choices":[{"index":0,"delta":{"role":"assistant","content":text},"finish_reason":null}]}),
            serde_json::json!({"id":"upgrade-model","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})
        )
    };
    let (model_url, mut model_requests, finish_first_turn) =
        tickloop_agent_test_support::model_http_support::serve_responses_with_gated_response(
            vec![
                (200, reply("answer-before-upgrade")),
                (200, reply("answer-after-upgrade")),
            ],
            0,
        )
        .await;
    let transport_path = workdir.path().join("transport.json");
    let mut transport = tickloop_agent_test_support::config();
    transport.protocol = tickloop_agent_protocol::Protocol::ChatCompletions;
    transport.base_url = model_url.trim_end_matches("/anthropic").into();
    transport.api_key_env = "TICKLOOP_FIXTURE_API_KEY".into();
    std::fs::write(&transport_path, serde_json::to_vec(&transport).unwrap()).unwrap();
    let old_executable = copy_bundle(old_source, &bundles.path().join("old"));
    let new_executable = copy_bundle(new_source, &bundles.path().join("new"));
    let old = connect_or_start_at(runtime.0.path().to_path_buf(), old_executable.clone())
        .await
        .unwrap();
    let old_daemon_pid = old.server_process_id();
    let original_session = old.session_id();
    let epoch = old.daemon_epoch();
    let mut launch = LaunchSpec::embedded(workdir.path().to_path_buf());
    launch.session_persistence = false;
    launch.host_options.execution_mode_override =
        Some(tickloop_agent_protocol::ExecutionMode::FullAccess);
    launch.host_options.prompt_policy = tickloop_agent_protocol::PromptPolicy::Unrestricted;
    let mut model = tickloop_agent_test_support::config();
    model.api_key_env = "TICKLOOP_FIXTURE_API_KEY".into();
    launch.model = Some(model);
    launch.environment = [
        ("PATH", "/usr/bin:/bin"),
        ("TICKLOOP_FIXTURE_API_KEY", "fixture-key"),
        ("DEEPSEEK_API_KEY", "fixture-key"),
    ]
    .into_iter()
    .map(|(name, value)| EnvironmentValue {
        name: name.as_bytes().to_vec(),
        value: value.as_bytes().to_vec(),
    })
    .collect();
    launch.environment.push(EnvironmentValue {
        name: b"TICKLOOP_TRANSPORT_FIXTURE".to_vec(),
        value: transport_path.as_os_str().as_encoded_bytes().to_vec(),
    });
    let creation = command(
        epoch,
        AgentCommand::Create {
            launch,
            initial: None,
            origin: None,
        },
    );
    let receipt = old.execute(creation.clone()).await.unwrap();
    let execution = match receipt.result.clone().unwrap() {
        CommandResult::Created { execution, .. } => execution,
        other => panic!("unexpected create {other:?}"),
    };
    let old_worker_pid = version(&old).await.workers[0].process_id;
    let shell = old.execute(command(epoch, AgentCommand::Submit {
        execution,
        input: UserInput::Shell { command: "printf before-upgrade; while [ ! -f release ]; do sleep 0.05; done; printf after-upgrade".into(), background: true },
        origin: None,
    })).await.unwrap().into_result().unwrap();
    let CommandResult::ShellSubmitted(shell) = shell else {
        panic!("background shell was not submitted")
    };
    let shell_pid = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let observed = snapshot(&old, execution).await;
            if observed.user_shells.iter().any(|record| {
                matches!(
                    record.state,
                    UserShellState::Failed | UserShellState::Unknown
                )
            }) {
                panic!(
                    "Shell did not start: {:?}; approvals: {:?}",
                    observed.user_shells, observed.permissions
                );
            }
            if let Some(shell) = observed
                .user_shells
                .iter()
                .find(|record| record.id == shell.command_id)
                && shell.state == UserShellState::Background
                && let Some(pid) = shell.pid
            {
                break pid;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    old.execute(command(
        epoch,
        AgentCommand::Submit {
            execution,
            input: UserInput::Text {
                text: "first turn".into(),
                contexts: Vec::new(),
                paths: Vec::new(),
            },
            origin: None,
        },
    ))
    .await
    .unwrap()
    .into_result()
    .unwrap();
    tokio::time::timeout(Duration::from_secs(10), model_requests.recv())
        .await
        .unwrap()
        .expect("first model request");
    old.execute(command(
        epoch,
        AgentCommand::UpgradeRuntime {
            executable: new_executable.clone(),
        },
    ))
    .await
    .unwrap()
    .into_result()
    .unwrap();
    let upgraded_daemon = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Ok(client) =
                LocalAgentClient::connect(runtime.0.path().join("daemon.sock")).await
                && client.server_process_id() != old_daemon_pid
            {
                break client;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("daemon did not transfer the busy Worker");
    assert_eq!(
        version(&upgraded_daemon).await.workers[0].process_id,
        old_worker_pid
    );
    finish_first_turn.send(()).unwrap();
    let replacement = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Ok(client) =
                LocalAgentClient::connect(runtime.0.path().join("daemon.sock")).await
                && client.server_process_id() != old_daemon_pid
            {
                let state = version(&client).await;
                if state.executable == new_executable
                    && state.workers.len() == 1
                    && state.workers[0].executable == new_executable
                {
                    assert_ne!(state.workers[0].process_id, old_worker_pid);
                    break client;
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("daemon and Worker did not activate the selected executable");
    assert_eq!(replacement.daemon_epoch(), epoch);
    tokio::time::timeout(Duration::from_secs(10), async {
        while old.server_process_id() != replacement.server_process_id() {
            assert!(
                old.is_connected(),
                "the original client lost its planned handoff"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the original client did not resume its transferred session");
    assert_eq!(old.session_id(), original_session);
    assert_eq!(old.daemon_epoch(), epoch);
    let replay = replacement.execute(creation).await.unwrap();
    assert_eq!(
        serde_json::to_value(&receipt).unwrap(),
        serde_json::to_value(replay).unwrap()
    );
    let current = snapshot(&replacement, execution).await;
    let still_running = current
        .user_shells
        .iter()
        .find(|record| record.id == shell.command_id)
        .unwrap();
    assert_eq!(still_running.pid, Some(shell_pid));
    assert_eq!(still_running.state, UserShellState::Background);
    assert!(!support::has_exited(shell_pid));
    old.execute(command(
        epoch,
        AgentCommand::Submit {
            execution,
            input: UserInput::Text {
                text: "second turn".into(),
                contexts: Vec::new(),
                paths: Vec::new(),
            },
            origin: None,
        },
    ))
    .await
    .unwrap()
    .into_result()
    .unwrap();
    let (_, second_request) = tokio::time::timeout(Duration::from_secs(10), model_requests.recv())
        .await
        .unwrap()
        .expect("second model request");
    assert!(second_request.to_string().contains("answer-before-upgrade"));
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let current = snapshot(&replacement, execution).await;
            if current.conversation.messages.iter().any(|message| message.content.iter().any(|content| matches!(content, tickloop_agent_protocol::Content::Text { text } if text.contains("answer-after-upgrade")))) { break; }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }).await.expect("new Worker did not finish the second model turn");
    std::fs::write(workdir.path().join("release"), b"continue").unwrap();
    let completed = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let current = snapshot(&replacement, execution).await;
            if let Some(shell) = current
                .user_shells
                .into_iter()
                .find(|record| record.id == shell.command_id)
                && shell.state == UserShellState::Completed
            {
                break shell;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("transferred shell completion was not delivered");
    assert_eq!(completed.exit_code, Some(0));
    let output = std::fs::read_to_string(completed.output_ref.unwrap()).unwrap();
    assert!(output.contains("before-upgrade") && output.contains("after-upgrade"));
    let continued = replacement
        .execute(command(
            epoch,
            AgentCommand::Submit {
                execution,
                input: UserInput::Shell {
                    command: "printf next-input".into(),
                    background: false,
                },
                origin: None,
            },
        ))
        .await
        .unwrap()
        .into_result()
        .unwrap();
    assert!(matches!(continued, CommandResult::ShellSubmitted(_)));
    let final_worker_pid = version(&replacement).await.workers[0].process_id;
    replacement.shutdown_daemon().await.unwrap();
    println!(
        "Upgrade probe passed: daemon {old_daemon_pid} -> {}, Worker {old_worker_pid} -> {}, Shell {shell_pid}, epoch {epoch}; model continuity and receipts preserved",
        replacement.server_process_id(),
        final_worker_pid
    );
}
