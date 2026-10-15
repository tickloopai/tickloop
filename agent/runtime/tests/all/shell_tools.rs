use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use tickloop_agent_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_agent_protocol::{
    Content, EventKind, ExecutionMode, InputModality, ModelConfig, Protocol, ToolResult, Usage,
};
use tickloop_agent_runtime::Agent;
use tickloop_agent_tools::{ToolError, Tools, Workspace, prepare};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

struct ShellModel {
    command: String,
}
#[async_trait]
impl ModelClient for ShellModel {
    async fn generate_title(
        &self,
        request: tickloop_agent_model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
        Ok(tickloop_agent_test_support::title_output(request))
    }

    fn identity(&self) -> tickloop_agent_protocol::ModelIdentity {
        tickloop_agent_protocol::ModelIdentity {
            provider: tickloop_agent_protocol::ModelProvider::Api,
            profile: "fixture".into(),
            protocol: tickloop_agent_protocol::Protocol::Messages,
            requested_model: "fixture".into(),
        }
    }

    async fn complete(
        &self,
        request: ModelRequest,
        _: UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        let content = if request
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .any(|content| matches!(content, Content::ToolResult { .. }))
        {
            vec![Content::Text {
                text: "Command results received.".into(),
            }]
        } else {
            vec![Content::ToolUse {
                id: "shell-call".into(),
                name: "Bash".into(),
                input: json!({"command":self.command}),
            }]
        };
        Ok(ModelOutput {
            private_source: None,
            content,
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
        })
    }
}
fn config() -> ModelConfig {
    ModelConfig {
        provider: tickloop_agent_protocol::ModelProvider::Api,
        auth_source: tickloop_agent_protocol::ModelAuthSource::ApiKeyEnv,
        name: "test".into(),
        protocol: Protocol::Messages,
        base_url: "http://127.0.0.1".into(),
        model: "test".into(),
        api_key_env: String::new(),
        max_output_tokens: 1024,
        provider_id: String::new(),
        provider_name: String::new(),
        model_id: String::new(),
        connection: None,
        card: None,
        input_modalities: vec![InputModality::Text],
    }
}

#[tokio::test]
async fn a_compound_shell_command_waits_for_approval_and_reports_real_output_and_exit() {
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(
        config(),
        Arc::new(ShellModel {
            command:
                "printf 'made by shell' > result.txt; printf 'stdout'; printf 'stderr' >&2; exit 7"
                    .into(),
        }),
    );
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Run the requested command".into())
        .await
        .unwrap();
    let mut result = None;
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(3), events.recv())
            .await
            .unwrap()
            .unwrap()
            .kind
        {
            EventKind::PermissionRequested { request } => {
                assert!(!directory.path().join("result.txt").exists());
                assert!(
                    request.input["command"]
                        .as_str()
                        .unwrap()
                        .contains("exit 7")
                );
                agent
                    .answer_permission(chat, request.id, true)
                    .await
                    .unwrap();
            }
            EventKind::ToolCompleted { result: output, .. } => {
                result = Some(output);
            }
            EventKind::Completed { .. } | EventKind::Failed(_) => break,
            _ => {}
        }
    }
    assert_eq!(
        std::fs::read_to_string(directory.path().join("result.txt")).unwrap(),
        "made by shell"
    );
    let result = result.expect("real command result");
    assert_eq!(result.content, "Exit code 7\nstdoutstderr");
    assert!(result.is_error);
    agent.shutdown().await;
}

#[tokio::test]
async fn a_read_only_command_uses_each_chats_own_directory_without_approval() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(
        config(),
        Arc::new(ShellModel {
            command: "pwd".into(),
        }),
    );
    agent.install_own_tickhost();
    for directory in [&first, &second] {
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let mut events = agent.subscribe();
        agent
            .submit(chat, None, "What is this chat's directory?".into())
            .await
            .unwrap();
        loop {
            match tokio::time::timeout(std::time::Duration::from_secs(3), events.recv())
                .await
                .unwrap()
                .unwrap()
                .kind
            {
                EventKind::PermissionRequested { .. } => {
                    panic!("ordinary pwd unexpectedly needs permission")
                }
                EventKind::ToolCompleted { result, .. } => {
                    assert_eq!(
                        result.content,
                        directory.path().canonicalize().unwrap().to_str().unwrap()
                    );
                }
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
    }
    agent.shutdown().await;
}

#[tokio::test]
async fn stopping_a_goal_collects_the_real_shell_cancellation_receipt() {
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(
        config(),
        Arc::new(ShellModel {
            command: "echo $$ > command.pid; sleep 30".into(),
        }),
    );
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent
        .submit(chat, None, "Start the long command".into())
        .await
        .unwrap();
    loop {
        if let EventKind::PermissionRequested { request } =
            tokio::time::timeout(std::time::Duration::from_secs(3), events.recv())
                .await
                .unwrap()
                .unwrap()
                .kind
        {
            agent
                .answer_permission(chat, request.id, true)
                .await
                .unwrap();
            break;
        }
    }
    let pid: i32 = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if let Ok(content) = std::fs::read_to_string(directory.path().join("command.pid"))
                && let Ok(pid) = content.trim().parse()
            {
                break pid;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let operations = agent.stop(chat, receipt.loop_id).await.unwrap();
    assert_eq!(operations.len(), 1);
    loop {
        if let EventKind::ToolCompleted { result, .. } =
            tokio::time::timeout(std::time::Duration::from_secs(3), events.recv())
                .await
                .unwrap()
                .unwrap()
                .kind
        {
            assert!(
                result
                    .content
                    .ends_with("<error>Command was aborted before completion</error>"),
                "{}",
                result.content
            );
            assert!(result.is_error);
            break;
        }
    }
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        -1,
        "direct process remains alive"
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn large_shell_output_has_a_bounded_preview_and_complete_output_file() {
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(
        config(),
        Arc::new(ShellModel {
            command: "i=0; while [ $i -lt 5000 ]; do printf 'delivery item\n'; i=$((i+1)); done"
                .into(),
        }),
    );
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Produce the output".into())
        .await
        .unwrap();
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(3), events.recv())
            .await
            .unwrap()
            .unwrap()
            .kind
        {
            EventKind::PermissionRequested { request } => agent
                .answer_permission(chat, request.id, true)
                .await
                .unwrap(),
            EventKind::ToolCompleted { result, .. } => {
                let output_file = result.output_ref.unwrap();
                assert!(result.content.starts_with(&format!(
                    "<persisted-output>\nOutput too large (68.4KB). Full output saved to: {}\n\nPreview (first 2KB):\ndelivery item\n",
                    output_file.display()
                )));
                assert!(result.content.len() < 3000);
                assert_eq!(
                    std::fs::read_to_string(output_file)
                        .unwrap()
                        .lines()
                        .count(),
                    5000
                );
            }
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    agent.shutdown().await;
}

#[tokio::test]
async fn a_foreground_command_moves_to_background_without_starting_a_second_process() {
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(
        config(),
        Arc::new(ShellModel {
            command: "env sh -c 'echo started >> starts.txt; printf ready; sleep 30'".into(),
        }),
    );
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Run the long command".into())
        .await
        .unwrap();
    loop {
        if let EventKind::PermissionRequested { request } =
            tokio::time::timeout(std::time::Duration::from_secs(3), events.recv())
                .await
                .unwrap()
                .unwrap()
                .kind
        {
            agent
                .answer_permission(chat, request.id, true)
                .await
                .unwrap();
            break;
        }
    }
    let running = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if let Some(command) = agent.commands(chat).await.unwrap().into_iter().next()
                && command.output_bytes >= 5
            {
                break command;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let background = agent
        .background_command(chat, running.operation_id)
        .await
        .unwrap();
    assert_eq!(background.pid, running.pid);
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(3), events.recv())
            .await
            .unwrap()
            .unwrap()
            .kind
        {
            EventKind::ToolCompleted { result, .. } => assert_eq!(
                result.content,
                format!(
                    "Command was manually backgrounded by user with ID: {}. Output is being written to: {}",
                    running.operation_id,
                    running.output_ref.display()
                )
            ),
            EventKind::Completed { .. } => break,
            _ => {}
        }
    }
    let output = agent
        .read_command(chat, running.operation_id, 0, 4096)
        .await
        .unwrap();
    assert!(output.content.contains("ready"));
    let stopped = agent
        .stop_command(chat, running.operation_id)
        .await
        .unwrap();
    assert!(stopped.signal.is_some() || stopped.exit_code.is_some());
    assert_eq!(
        std::fs::read_to_string(directory.path().join("starts.txt")).unwrap(),
        "started\n"
    );
    assert_eq!(unsafe { libc::kill(running.pid.unwrap() as i32, 0) }, -1);
    agent.shutdown().await;
}

/// Starts one background command, then optionally stops it through `TaskStop` with the given
/// identifier parameter once its start result arrives.
struct BackgroundModel {
    stop_parameter: Option<&'static str>,
}
#[async_trait]
impl ModelClient for BackgroundModel {
    async fn generate_title(
        &self,
        request: tickloop_agent_model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
        Ok(tickloop_agent_test_support::title_output(request))
    }

    fn identity(&self) -> tickloop_agent_protocol::ModelIdentity {
        tickloop_agent_protocol::ModelIdentity {
            provider: tickloop_agent_protocol::ModelProvider::Api,
            profile: "fixture".into(),
            protocol: tickloop_agent_protocol::Protocol::Messages,
            requested_model: "fixture".into(),
        }
    }

    async fn complete(
        &self,
        request: ModelRequest,
        _: UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        let results = tool_results(&request);
        let content = match (results.as_slice(), self.stop_parameter) {
            ([], _) => vec![Content::ToolUse {
                id: "start".into(),
                name: "Bash".into(),
                input: json!({"command":"printf started; sleep 30","run_in_background":true}),
            }],
            ([started], Some(parameter)) => vec![Content::ToolUse {
                id: "stop".into(),
                name: "TaskStop".into(),
                input: json!({ parameter: task_id(started) }),
            }],
            _ => vec![Content::Text {
                text: "The background command has been stopped.".into(),
            }],
        };
        Ok(ModelOutput {
            private_source: None,
            content,
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

fn tool_results(request: &ModelRequest) -> Vec<String> {
    request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::ToolResult { content, .. } => Some(content.text().into_owned()),
            _ => None,
        })
        .collect()
}

/// The task ID a Bash background start reports.
fn task_id(result: &str) -> String {
    result
        .split("with ID: ")
        .nth(1)
        .and_then(|rest| rest.split(". Output is being written to: ").next())
        .unwrap_or_else(|| panic!("no task ID in {result:?}"))
        .to_owned()
}

async fn run_to_completion(
    agent: &Agent,
    chat: uuid::Uuid,
    events: &mut tokio::sync::broadcast::Receiver<tickloop_agent_protocol::Event>,
) -> Vec<(String, ToolResult)> {
    let mut results = Vec::new();
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(3), events.recv())
            .await
            .unwrap()
            .unwrap()
            .kind
        {
            EventKind::PermissionRequested { request } => agent
                .answer_permission(chat, request.id, true)
                .await
                .unwrap(),
            EventKind::ToolCompleted { call_id, result } => results.push((call_id, result)),
            EventKind::Completed { .. } => return results,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
}

#[tokio::test]
async fn the_agent_stops_its_own_background_command_by_task_id_or_shell_id() {
    let directory = tempfile::tempdir().unwrap();
    for parameter in ["task_id", "shell_id"] {
        let agent = Agent::new_embedded(
            config(),
            Arc::new(BackgroundModel {
                stop_parameter: Some(parameter),
            }),
        );
        agent.install_own_tickhost();
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let mut events = agent.subscribe();
        agent
            .submit(
                chat,
                None,
                "Start a background command, then stop it".into(),
            )
            .await
            .unwrap();
        let results = run_to_completion(&agent, chat, &mut events).await;
        let commands = agent.commands(chat).await.unwrap();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].reason.as_deref(), Some("cancelled"));
        assert!(commands[0].exit_code.is_some() || commands[0].signal.is_some());
        let task_id = commands[0].operation_id;
        assert_eq!(results.len(), 2, "{parameter}");
        assert_eq!(
            results[0].1.content,
            format!(
                "Command running in background with ID: {task_id}. Output is being written to: {}",
                commands[0].output_ref.display()
            )
        );
        assert!(!results[1].1.is_error, "{}", results[1].1.content);
        assert_eq!(
            results[1].1.content,
            format!(
                r#"{{"message":"Successfully stopped task: {task_id} (printf started; sleep 30)","task_id":"{task_id}","task_type":"local_bash","command":"printf started; sleep 30"}}"#
            )
        );
        let recorded = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let snapshot = agent.snapshot(chat).await.unwrap();
                if snapshot
                    .commands
                    .iter()
                    .any(|command| command.reason.as_deref() == Some("cancelled"))
                {
                    break snapshot;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            !recorded
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .any(|content| matches!(content, Content::Text { text } if text.starts_with("<task-notification>"))),
            "a task stopped by TaskStop needs no notification"
        );
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn task_stop_runs_without_approval_even_where_other_tools_ask() {
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(
        config(),
        Arc::new(BackgroundModel {
            stop_parameter: Some("task_id"),
        }),
    );
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(
            chat,
            None,
            "Start a background command, then stop it".into(),
        )
        .await
        .unwrap();
    let mut asked = Vec::new();
    let mut results = Vec::new();
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(3), events.recv())
            .await
            .unwrap()
            .unwrap()
            .kind
        {
            EventKind::PermissionRequested { request } => {
                asked.push(request.name.clone());
                agent
                    .answer_permission(chat, request.id, true)
                    .await
                    .unwrap();
            }
            EventKind::ToolCompleted { result, .. } => results.push(result),
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert!(
        !asked.iter().any(|name| name == "TaskStop"),
        "TaskStop asked for approval: {asked:?}"
    );
    assert_eq!(results.len(), 2);
    assert!(
        results[1]
            .content
            .starts_with(r#"{"message":"Successfully stopped task: "#),
        "{}",
        results[1].content
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn stopping_a_goal_after_its_turn_completed_still_stops_its_background_process() {
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(
        config(),
        Arc::new(BackgroundModel {
            stop_parameter: None,
        }),
    );
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent
        .submit(chat, None, "Run the long command in the background".into())
        .await
        .unwrap();
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(3), events.recv())
            .await
            .unwrap()
            .unwrap()
            .kind
        {
            EventKind::PermissionRequested { request } => agent
                .answer_permission(chat, request.id, true)
                .await
                .unwrap(),
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    let running = agent
        .commands(chat)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert!(running.exit_code.is_none() && running.signal.is_none());
    agent.stop(chat, receipt.loop_id).await.unwrap();
    let stopped = agent
        .commands(chat)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(stopped.reason.as_deref(), Some("cancelled"));
    assert!(stopped.exit_code.is_some() || stopped.signal.is_some());
    assert_eq!(unsafe { libc::kill(running.pid.unwrap() as i32, 0) }, -1);
    agent.shutdown().await;
}

#[tokio::test]
async fn a_shell_that_leaves_its_process_group_running_has_an_unresolved_result() {
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(
        config(),
        Arc::new(ShellModel {
            command: "sleep 30 & echo $! > descendant.pid".into(),
        }),
    );
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(
            chat,
            None,
            "Launch the explicitly requested shell command".into(),
        )
        .await
        .unwrap();
    let output = loop {
        match tokio::time::timeout(std::time::Duration::from_secs(3), events.recv())
            .await
            .unwrap()
            .unwrap()
            .kind
        {
            EventKind::PermissionRequested { request } => agent
                .answer_permission(chat, request.id, true)
                .await
                .unwrap(),
            EventKind::ToolCompleted { result, .. } => break result,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    };
    let descendant: i32 = std::fs::read_to_string(directory.path().join("descendant.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    unsafe { libc::kill(descendant, libc::SIGKILL) };
    assert!(output.content.contains("descendants"), "{}", output.content);
    assert!(output.is_error);
    agent.shutdown().await;
}

#[tokio::test]
async fn stopping_a_shell_also_terminates_descendants_that_ignore_sigterm() {
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(config(), Arc::new(ShellModel {
        command: "/bin/sh -c 'trap \"\" TERM; echo ready > child-ready; sleep 0.8; echo late > late-write' & wait".into(),
    }));
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent
        .submit(chat, None, "Run the test command".into())
        .await
        .unwrap();
    loop {
        if let EventKind::PermissionRequested { request } =
            tokio::time::timeout(std::time::Duration::from_secs(3), events.recv())
                .await
                .unwrap()
                .unwrap()
                .kind
        {
            agent
                .answer_permission(chat, request.id, true)
                .await
                .unwrap();
            break;
        }
    }
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !directory.path().join("child-ready").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    agent.stop(chat, receipt.loop_id).await.unwrap();
    agent.shutdown().await;
    tokio::time::sleep(std::time::Duration::from_millis(900)).await;
    assert!(
        !directory.path().join("late-write").exists(),
        "a descendant still produced a side effect after cancellation was acknowledged"
    );
}

#[test]
fn bash_takes_claude_code_parameters_and_the_old_command_tools_are_gone() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = Workspace {
        workdir: directory.path().into(),
        additional_directories: Vec::new(),
    };
    let definitions = Tools::definitions();
    let names = definitions
        .iter()
        .map(|definition| definition.name.as_str())
        .collect::<Vec<_>>();
    assert!(names.contains(&"Bash") && names.contains(&"TaskStop"));
    for removed in ["shell", "command_status", "read_command", "stop_command"] {
        assert!(!names.contains(&removed), "{removed}");
        assert!(matches!(
            prepare(&workspace, removed, &json!({"command":"pwd"})),
            Err(ToolError::UnknownTool(_))
        ));
    }
    let properties = |name: &str| {
        let definition = definitions
            .iter()
            .find(|definition| definition.name == name)
            .unwrap();
        let mut keys = definition.input_schema["properties"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        keys.sort();
        (keys, definition.input_schema["required"].clone())
    };
    assert_eq!(
        properties("Bash"),
        (
            vec![
                "command".to_owned(),
                "cwd".into(),
                "description".into(),
                "run_in_background".into(),
                "timeout".into()
            ],
            json!(["command"])
        )
    );
    assert_eq!(
        properties("TaskStop"),
        (
            vec!["shell_id".to_owned(), "task_id".into()],
            serde_json::Value::Null
        )
    );

    let prepared = prepare(
        &workspace,
        "Bash",
        &json!({
            "command": "ls",
            "timeout": "5000",
            "description": "List files in current directory",
            "run_in_background": "false"
        }),
    )
    .unwrap();
    assert_eq!(prepared.input["timeout"], 5000);
    assert_eq!(prepared.input["run_in_background"], false);
    assert_eq!(
        prepared.input["description"],
        "List files in current directory"
    );
    assert_eq!(
        prepared.input["cwd"],
        json!(directory.path().canonicalize().unwrap())
    );
    assert!(prepared.input.get("path").is_none());
    for old in [
        json!({"command":"ls","background":true}),
        json!({"command":"ls","timeout_ms":5000}),
        json!({"command":"ls","timeout":-1}),
    ] {
        assert!(
            matches!(
                prepare(&workspace, "Bash", &old),
                Err(ToolError::InvalidInput(_))
            ),
            "{old}"
        );
    }
    assert_eq!(
        prepare(&workspace, "TaskStop", &json!({}))
            .unwrap_err()
            .to_string(),
        "Missing required parameter: task_id"
    );
    assert_eq!(
        prepare(&workspace, "TaskStop", &json!({"shell_id":"legacy"}))
            .unwrap()
            .input,
        json!({"task_id":"legacy"})
    );
    assert!(matches!(
        prepare(&workspace, "TaskStop", &json!({"operation_id":"legacy"})),
        Err(ToolError::InvalidInput(_))
    ));
}

#[tokio::test]
async fn bash_runs_commands_with_bash_or_zsh() {
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(
        config(),
        Arc::new(ShellModel {
            command: r#"printf '%s' "${BASH_VERSION:+bash}${ZSH_VERSION:+zsh}""#.into(),
        }),
    );
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Which shell runs commands?".into())
        .await
        .unwrap();
    let results = run_to_completion(&agent, chat, &mut events).await;
    assert!(
        matches!(results[0].1.content.as_str(), "bash" | "zsh"),
        "{}",
        results[0].1.content
    );
    agent.shutdown().await;
}

/// Replies to each request with whatever `reply` computes from it.
struct FunctionModel<F>(F);
#[async_trait]
impl<F> ModelClient for FunctionModel<F>
where
    F: Fn(&ModelRequest) -> Vec<Content> + Send + Sync,
{
    async fn generate_title(
        &self,
        request: tickloop_agent_model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
        Ok(tickloop_agent_test_support::title_output(request))
    }

    fn identity(&self) -> tickloop_agent_protocol::ModelIdentity {
        tickloop_agent_protocol::ModelIdentity {
            provider: tickloop_agent_protocol::ModelProvider::Api,
            profile: "fixture".into(),
            protocol: tickloop_agent_protocol::Protocol::Messages,
            requested_model: "fixture".into(),
        }
    }

    async fn complete(
        &self,
        request: ModelRequest,
        _: UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        Ok(ModelOutput {
            private_source: None,
            content: (self.0)(&request),
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

fn latest_user_text(request: &ModelRequest) -> String {
    request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::Text { text } => Some(text.clone()),
            _ => None,
        })
        .next_back()
        .unwrap_or_default()
}

#[tokio::test]
async fn task_stop_rejects_unknown_tasks_before_asking_for_permission() {
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(
        config(),
        Arc::new(FunctionModel(|request: &ModelRequest| {
            if tool_results(request).is_empty() {
                vec![Content::ToolUse {
                    id: "stop".into(),
                    name: "TaskStop".into(),
                    input: json!({"task_id":"missing-task"}),
                }]
            } else {
                vec![Content::Text {
                    text: "No such task.".into(),
                }]
            }
        })),
    );
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Stop the task".into())
        .await
        .unwrap();
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(3), events.recv())
            .await
            .unwrap()
            .unwrap()
            .kind
        {
            EventKind::PermissionRequested { .. } => panic!("an unknown task needs no approval"),
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    let snapshot = agent.snapshot(chat).await.unwrap();
    let result = snapshot
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|content| match content {
            Content::ToolResult {
                content, is_error, ..
            } => Some((content.text().into_owned(), *is_error)),
            _ => None,
        })
        .unwrap();
    assert!(result.1);
    assert!(
        result.0.contains("No task found with ID: missing-task"),
        "{}",
        result.0
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn task_stop_leaves_another_goals_command_to_the_user() {
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(
        config(),
        Arc::new(FunctionModel(|request: &ModelRequest| {
            let results = tool_results(request);
            match (latest_user_text(request).as_str(), results.len()) {
                ("Start the server", 0) => vec![Content::ToolUse {
                    id: "start".into(),
                    name: "Bash".into(),
                    input: json!({"command":"sleep 30","run_in_background":true}),
                }],
                ("Stop the other goal's server", 1) => vec![Content::ToolUse {
                    id: "stop".into(),
                    name: "TaskStop".into(),
                    input: json!({"task_id": task_id(&results[0])}),
                }],
                _ => vec![Content::Text {
                    text: "Done.".into(),
                }],
            }
        })),
    );
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let first = agent
        .submit(chat, None, "Start the server".into())
        .await
        .unwrap();
    run_to_completion(&agent, chat, &mut events).await;
    agent
        .submit(chat, None, "Stop the other goal's server".into())
        .await
        .unwrap();
    let results = run_to_completion(&agent, chat, &mut events).await;
    let command = agent.commands(chat).await.unwrap().remove(0);
    assert_eq!(results.len(), 1);
    assert!(results[0].1.is_error);
    assert_eq!(
        results[0].1.content,
        format!(
            "Task {} does not belong to this goal; only the user can stop it",
            command.operation_id
        )
    );
    assert_eq!(
        command.state,
        tickloop_agent_protocol::CommandState::Background
    );
    agent.stop(chat, first.loop_id).await.unwrap();
    agent.shutdown().await;
}
