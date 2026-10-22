use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tickloop_agent_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_agent_protocol::{
    Content, Decision, DirectedSummaryRequest, Event, EventKind, InputModality, ModelConfig,
    PermissionRule, Protocol, SummaryDirection, ToolDefinition, ToolResult, Usage,
};
use tickloop_agent_runtime::Agent;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

struct ReadAndAnswer {
    offered_tools: std::sync::Mutex<Vec<ToolDefinition>>,
}

struct ToolsAndAnswer(Vec<Content>);

#[async_trait]
impl ModelClient for ToolsAndAnswer {
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
                text: "Finished reading the actual tool results.".into(),
            }]
        } else {
            self.0.clone()
        };
        Ok(ModelOutput {
            private_source: None,
            content,
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

#[async_trait]
impl ModelClient for ReadAndAnswer {
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
        *self.offered_tools.lock().unwrap() = request.tools.clone();
        let content = if let Some(result) = request
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .find_map(|content| {
                if let Content::ToolResult {
                    content,
                    is_error: false,
                    ..
                } = content
                {
                    Some(content)
                } else {
                    None
                }
            }) {
            vec![Content::Text {
                text: format!("The source says: {}", result.text()),
            }]
        } else {
            vec![Content::ToolUse {
                id: "read-1".into(),
                name: "Read".into(),
                input: json!({"file_path":"notes.txt"}),
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

async fn require_file_change_approval(agent: &Agent, chat: Uuid, tool: &str) {
    agent
        .set_permission_rules(
            chat,
            vec![PermissionRule {
                id: Uuid::new_v4(),
                decision: Decision::Ask,
                tool: tool.into(),
                specifier: None,
                input: None,
                loop_id: None,
                persistent: false,
            }],
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn agent_answers_from_a_real_file_in_its_chat_directory() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("notes.txt"),
        "The delivery date is Friday.",
    )
    .unwrap();
    let model = Arc::new(ReadAndAnswer {
        offered_tools: Default::default(),
    });
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Read notes.txt and tell me the delivery date".into())
        .await
        .unwrap();
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(3), events.recv())
            .await
            .unwrap()
            .unwrap();
        match event.kind {
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("agent failed: {error}"),
            _ => {}
        }
    }
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(snapshot.messages.iter().flat_map(|message| &message.content).any(|content|
        matches!(content,Content::Text {text} if text == "The source says:      1\u{2192}The delivery date is Friday.")
    ));
    let offered_tools = model.offered_tools.lock().unwrap().clone();
    assert!(offered_tools.iter().all(|tool| tool.name != "read_file"));
    let read = offered_tools
        .iter()
        .find(|tool| tool.name == "Read")
        .expect("the model is offered Read");
    assert_eq!(read.input_schema["required"], json!(["file_path"]));
    assert_eq!(
        read.input_schema["properties"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["file_path", "limit", "offset"]
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn agent_finds_paths_and_matching_lines_in_real_files() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("docs")).unwrap();
    std::fs::write(
        directory.path().join("docs/notes.md"),
        "first line\nDelivery is Friday\n",
    )
    .unwrap();
    std::fs::write(directory.path().join("ignore.txt"), "nothing here").unwrap();
    std::fs::write(
        directory.path().join("service-credentials.md"),
        "Delivery is Friday; API token must not reach the model",
    )
    .unwrap();
    let agent = Agent::new_embedded(
        config(),
        Arc::new(ToolsAndAnswer(vec![
            Content::ToolUse {
                id: "paths".into(),
                name: "Glob".into(),
                input: json!({"pattern":"**/*.md"}),
            },
            Content::ToolUse {
                id: "contents".into(),
                name: "Grep".into(),
                input: json!({"pattern":"Delivery.*Friday","glob":"**/*.md","output_mode":"content"}),
            },
        ])),
    );
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Find the delivery note".into())
        .await
        .unwrap();
    let mut results = Vec::new();
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(3), events.recv())
            .await
            .unwrap()
            .unwrap()
            .kind
        {
            EventKind::ToolCompleted { result, .. } => {
                assert!(!result.is_error, "{}", result.content);
                results.push(result.content);
            }
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert_eq!(
        results,
        ["docs/notes.md", "docs/notes.md:2:Delivery is Friday"]
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn searching_outside_the_workspace_asks_for_approval_first() {
    let directory = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("orders.txt"), "delivery Monday\n").unwrap();
    std::fs::write(outside.path().join("orders.txt"), "delivery Friday\n").unwrap();
    let model = Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![
        vec![
            Content::ToolUse {
                id: "inside".into(),
                name: "Grep".into(),
                input: json!({"pattern":"delivery"}),
            },
            Content::ToolUse {
                id: "outside".into(),
                name: "Grep".into(),
                input: json!({"pattern":"delivery","path":outside.path(),"output_mode":"content"}),
            },
        ],
        tickloop_agent_test_support::text("done"),
    ]));
    let agent = Agent::new_embedded(config(), model);
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Find the deliveries".into())
        .await
        .unwrap();

    let (results, approvals) = finish_turn(&agent, chat, &mut events).await;
    assert_eq!(approvals, 1, "only the search outside the workspace asks");
    assert_eq!(
        result(&results, "inside").content,
        "Found 1 file\norders.txt"
    );
    assert_eq!(
        result(&results, "outside").content,
        format!(
            "{}:1:delivery Friday",
            outside
                .path()
                .canonicalize()
                .unwrap()
                .join("orders.txt")
                .display()
        )
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn an_outside_symlink_requires_the_resolved_path_approval_and_cannot_change_after_approval() {
    let directory = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("first.txt"), "FIRST SECRET").unwrap();
    std::fs::write(outside.path().join("second.txt"), "SECOND SECRET").unwrap();
    let link = directory.path().join("link.txt");
    std::os::unix::fs::symlink(outside.path().join("first.txt"), &link).unwrap();
    let agent = Agent::new_embedded(
        config(),
        Arc::new(ToolsAndAnswer(vec![Content::ToolUse {
            id: "read-link".into(),
            name: "Read".into(),
            input: json!({"file_path":"link.txt"}),
        }])),
    );
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Read the linked note".into())
        .await
        .unwrap();
    let permission = loop {
        match tokio::time::timeout(std::time::Duration::from_secs(3), events.recv())
            .await
            .unwrap()
            .unwrap()
            .kind
        {
            EventKind::PermissionRequested { request } => break request,
            EventKind::ToolCompleted { .. } => {
                panic!("outside content was read without permission")
            }
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    };
    assert_eq!(
        permission.input["file_path"],
        json!(outside.path().join("first.txt").canonicalize().unwrap())
    );
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(outside.path().join("second.txt"), &link).unwrap();
    agent
        .answer_permission(chat, permission.id, true)
        .await
        .unwrap();
    let mut path_change_error = None;
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(3), events.recv())
            .await
            .unwrap()
            .unwrap()
            .kind
        {
            EventKind::ToolCompleted { result, .. } => {
                assert!(result.is_error, "changed symlink was accepted");
                path_change_error = Some(result.content);
            }
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    let path_change_error = path_change_error.expect("the changed symlink was not reported");
    assert!(
        path_change_error.contains("path changed"),
        "{path_change_error}"
    );
    assert!(
        agent
            .answer_permission(chat, permission.id, true)
            .await
            .is_err()
    );
    assert!(
        !serde_json::to_string(&agent.snapshot(chat).await.unwrap())
            .unwrap()
            .contains("SECOND SECRET")
    );
    agent.shutdown().await;
}

async fn read_results(directory: &std::path::Path, reads: Vec<serde_json::Value>) -> Vec<String> {
    let count = reads.len();
    let agent = Agent::new_embedded(
        config(),
        Arc::new(ToolsAndAnswer(
            reads
                .into_iter()
                .enumerate()
                .map(|(index, input)| Content::ToolUse {
                    id: format!("read-{index}"),
                    name: "Read".into(),
                    input,
                })
                .collect(),
        )),
    );
    let chat = agent.create_chat(directory.into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Read the notes".into()).await.unwrap();
    let mut results = std::collections::HashMap::new();
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(3), events.recv())
            .await
            .unwrap()
            .unwrap()
            .kind
        {
            EventKind::ToolCompleted {
                call_id, result, ..
            } => {
                assert!(!result.is_error, "{}", result.content);
                assert!(result.output_ref.is_none());
                assert!(results.insert(call_id, result.content).is_none());
            }
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    agent.shutdown().await;
    (0..count)
        .map(|index| results.remove(&format!("read-{index}")).unwrap())
        .collect()
}

#[tokio::test]
async fn read_returns_numbered_lines_for_the_requested_window() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("notes.txt"), "你好\r\nabc\n第三行\n").unwrap();
    std::fs::write(directory.path().join("empty.txt"), "").unwrap();
    let results = read_results(
        directory.path(),
        vec![
            json!({"file_path":"notes.txt"}),
            json!({"file_path":"notes.txt","offset":2,"limit":1}),
            json!({"file_path":"notes.txt","offset":9}),
            json!({"file_path":"empty.txt"}),
        ],
    )
    .await;
    assert_eq!(
        results,
        [
            "     1\u{2192}你好\n     2\u{2192}abc\n     3\u{2192}第三行\n     4\u{2192}",
            "     2\u{2192}abc",
            "<system-reminder>Warning: the file exists but is shorter than the provided offset (9). The file has 4 lines.</system-reminder>",
            "<system-reminder>Warning: the file exists but the contents are empty.</system-reminder>",
        ]
    );
}

#[tokio::test]
async fn a_large_read_reaches_the_model_without_being_saved_to_a_file() {
    let directory = tempfile::tempdir().unwrap();
    let content = format!("{}\n", "r".repeat(99)).repeat(600);
    std::fs::write(directory.path().join("large.txt"), &content).unwrap();
    let results = read_results(directory.path(), vec![json!({"file_path":"large.txt"})]).await;
    assert!(results[0].chars().count() > 60_000);
    assert!(results[0].starts_with("     1\u{2192}rrrr"));
    assert!(!results[0].contains("<persisted-output>"));
}

#[tokio::test]
async fn approved_file_creation_and_local_edit_change_the_actual_file() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("report.txt");
    let agent = Agent::new_embedded(
        config(),
        Arc::new(ToolsAndAnswer(vec![
            Content::ToolUse {
                id: "write".into(),
                name: "Write".into(),
                input: json!({"file_path":"report.txt","content":"Delivery Monday\n"}),
            },
            Content::ToolUse {
                id: "edit".into(),
                name: "Edit".into(),
                input: json!({"file_path":"report.txt","old_string":"Monday","new_string":"Friday"}),
            },
        ])),
    );
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_permission_rules(
            chat,
            ["Write", "Edit"]
                .into_iter()
                .map(|tool| PermissionRule {
                    id: Uuid::new_v4(),
                    decision: Decision::Ask,
                    tool: tool.into(),
                    specifier: None,
                    input: None,
                    loop_id: None,
                    persistent: false,
                })
                .collect(),
        )
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(
            chat,
            "Create the report and correct the delivery day".into(),
        )
        .await
        .unwrap();
    let mut approvals = 0;
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(3), events.recv())
            .await
            .unwrap()
            .unwrap()
            .kind
        {
            EventKind::PermissionRequested { request } => {
                assert_eq!(
                    request.input["file_path"],
                    json!(directory.path().canonicalize().unwrap().join("report.txt"))
                );
                if approvals == 0 {
                    assert!(!path.exists());
                } else {
                    assert_eq!(std::fs::read_to_string(&path).unwrap(), "Delivery Monday\n");
                }
                approvals += 1;
                agent
                    .answer_permission(chat, request.id, true)
                    .await
                    .unwrap();
            }
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert_eq!(approvals, 2);
    assert_eq!(std::fs::read_to_string(path).unwrap(), "Delivery Friday\n");
    assert!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .operations
            .iter()
            .all(|operation| operation
                .result
                .as_ref()
                .is_some_and(|result| !result.is_error))
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn a_file_changed_while_approval_is_pending_is_not_overwritten() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("report.txt");
    std::fs::write(&path, "Delivery Monday").unwrap();
    let agent = Agent::new_embedded(
        config(),
        Arc::new(ToolsAndAnswer(vec![
            Content::ToolUse {
                id: "read".into(),
                name: "Read".into(),
                input: json!({"file_path":"report.txt"}),
            },
            Content::ToolUse {
                id: "edit".into(),
                name: "Edit".into(),
                input: json!({"file_path":"report.txt","old_string":"Monday","new_string":"Friday"}),
            },
        ])),
    );
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    require_file_change_approval(&agent, chat, "Edit").await;
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Correct the delivery day".into())
        .await
        .unwrap();
    let mut file_change_error = None;
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(3), events.recv())
            .await
            .unwrap()
            .unwrap()
            .kind
        {
            EventKind::PermissionRequested { request } => {
                std::fs::write(&path, "A human changed this document").unwrap();
                agent
                    .answer_permission(chat, request.id, true)
                    .await
                    .unwrap();
            }
            EventKind::ToolCompleted {
                call_id, result, ..
            } if call_id == "edit" => {
                assert!(result.is_error, "changed content was overwritten");
                file_change_error = Some(result.content);
            }
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    let file_change_error = file_change_error.expect("the changed file was not reported");
    assert!(
        file_change_error.contains("file changed"),
        "{file_change_error}"
    );
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        "A human changed this document"
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn a_large_search_result_is_saved_to_a_file_with_a_preview_for_the_model() {
    let directory = tempfile::tempdir().unwrap();
    let content = (0..2000)
        .map(|index| format!("delivery item {index}\n"))
        .collect::<String>();
    std::fs::write(directory.path().join("notes.txt"), content).unwrap();
    let agent = Agent::new_embedded(
        config(),
        Arc::new(ToolsAndAnswer(vec![Content::ToolUse {
            id: "search".into(),
            name: "Grep".into(),
            input: json!({"path":"notes.txt","pattern":"delivery","output_mode":"content","head_limit":0}),
        }])),
    );
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Find all delivery items".into())
        .await
        .unwrap();
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(3), events.recv())
            .await
            .unwrap()
            .unwrap()
            .kind
        {
            EventKind::ToolCompleted { result, .. } => {
                assert!(!result.is_error, "{}", result.content);
                assert!(
                    result
                        .content
                        .starts_with("<persisted-output>\nOutput too large ("),
                    "{}",
                    result.content
                );
                let path = result.output_ref.expect("complete output reference");
                assert!(
                    path.ends_with("tool-results/search.txt"),
                    "{}",
                    path.display()
                );
                assert!(result.content.contains(&path.display().to_string()));
                let complete = std::fs::read_to_string(path).unwrap();
                assert_eq!(complete.lines().count(), 2000);
                assert!(complete.starts_with("1:delivery item 0\n"));
                assert!(complete.ends_with("\n2000:delivery item 1999"));
            }
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    agent.shutdown().await;
}

#[tokio::test]
async fn directory_search_does_not_open_every_file_at_once() {
    if std::env::var_os("TICKLOOP_LOW_FD_TEST_CHILD").is_none() {
        use std::os::unix::process::CommandExt;
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                tickloop_agent_test_support::test_path!(
                    directory_search_does_not_open_every_file_at_once
                )
                .as_str(),
                "--nocapture",
            ])
            .env("TICKLOOP_LOW_FD_TEST_CHILD", "1");
        unsafe {
            command.pre_exec(|| {
                let limit = libc::rlimit {
                    rlim_cur: 64,
                    rlim_max: 64,
                };
                if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    for index in 0..300 {
        std::fs::write(
            directory.path().join(format!("item-{index}.txt")),
            "delivery",
        )
        .unwrap();
    }
    let agent = Agent::new_embedded(
        config(),
        Arc::new(ToolsAndAnswer(vec![Content::ToolUse {
            id: "search".into(),
            name: "Grep".into(),
            input: json!({"pattern":"delivery","output_mode":"count","head_limit":0}),
        }])),
    );
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Find all delivery records".into())
        .await
        .unwrap();
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(3), events.recv())
            .await
            .unwrap()
            .unwrap()
            .kind
        {
            EventKind::ToolCompleted { result, .. } => {
                assert!(!result.is_error, "{}", result.content);
                let complete = if let Some(path) = result.output_ref {
                    std::fs::read_to_string(path).unwrap()
                } else {
                    result.content
                };
                assert!(
                    complete.ends_with("\n\nFound 300 total occurrences across 300 files."),
                    "files were skipped because traversal exhausted descriptors: {complete}"
                );
            }
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    agent.shutdown().await;
}

const NOT_READ: &str = "File has not been read yet. Read it first before writing to it.";

/// Waits for the turn to finish, approving every permission request, and returns the tool results
/// by call id together with how many approvals were requested.
async fn finish_turn(
    agent: &Agent,
    chat: Uuid,
    events: &mut tokio::sync::broadcast::Receiver<Event>,
) -> (Vec<(String, ToolResult)>, usize) {
    let mut results = Vec::new();
    let mut approvals = 0;
    loop {
        match tickloop_agent_test_support::next_event(events).await.kind {
            EventKind::PermissionRequested { request } => {
                approvals += 1;
                agent
                    .answer_permission(chat, request.id, true)
                    .await
                    .unwrap();
            }
            EventKind::ToolCompleted {
                call_id, result, ..
            } => results.push((call_id, result)),
            EventKind::Completed { .. } => return (results, approvals),
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
}

fn result<'a>(results: &'a [(String, ToolResult)], call_id: &str) -> &'a ToolResult {
    &results
        .iter()
        .find(|(id, _)| id == call_id)
        .unwrap_or_else(|| panic!("no result for {call_id}"))
        .1
}

#[tokio::test]
async fn an_unread_file_is_rejected_before_any_approval_is_requested() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("report.txt");
    std::fs::write(&path, "Delivery Monday\n").unwrap();
    let model = Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![
        tickloop_agent_test_support::call(
            "overwrite",
            "Write",
            json!({"file_path":"report.txt","content":"replaced\n"}),
        ),
        tickloop_agent_test_support::text("done"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    require_file_change_approval(&agent, chat, "Write").await;
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Replace the report".into())
        .await
        .unwrap();

    let (results, approvals) = finish_turn(&agent, chat, &mut events).await;
    assert_eq!(approvals, 0);
    let overwrite = result(&results, "overwrite");
    assert!(overwrite.is_error);
    assert_eq!(overwrite.content, NOT_READ);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "Delivery Monday\n");
    agent.shutdown().await;
}

#[tokio::test]
async fn a_file_attached_with_an_at_mention_counts_as_read_when_attached_whole() {
    let directory = tempfile::tempdir().unwrap();
    let notes = directory.path().join("notes.txt");
    let large = directory.path().join("large.txt");
    std::fs::write(&notes, "Delivery Monday\n").unwrap();
    std::fs::write(&large, format!("Monday\n{}", "x".repeat(70 * 1024))).unwrap();
    let model = Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![
        vec![
            Content::ToolUse {
                id: "edit-notes".into(),
                name: "Edit".into(),
                input: json!({"file_path":"notes.txt","old_string":"Monday","new_string":"Friday"}),
            },
            Content::ToolUse {
                id: "edit-large".into(),
                name: "Edit".into(),
                input: json!({"file_path":"large.txt","old_string":"Monday","new_string":"Friday"}),
            },
        ],
        tickloop_agent_test_support::text("done"),
    ]));
    let agent = Agent::new_embedded(config(), model);
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit_with_file_references(
            chat,
            "Fix the day in @notes.txt and @large.txt".into(),
            vec![notes.clone(), large.clone()],
        )
        .await
        .unwrap();

    let (results, _) = finish_turn(&agent, chat, &mut events).await;
    let edited = result(&results, "edit-notes");
    assert!(!edited.is_error, "{}", edited.content);
    assert_eq!(
        std::fs::read_to_string(&notes).unwrap(),
        "Delivery Friday\n"
    );
    let truncated = result(&results, "edit-large");
    assert!(truncated.is_error);
    assert_eq!(
        truncated.content, NOT_READ,
        "a truncated attachment is not the whole file"
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn compaction_forgets_which_files_the_chat_read() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("notes.txt");
    std::fs::write(&path, "Delivery Monday\n").unwrap();
    let edit = || {
        tickloop_agent_test_support::call(
            "edit",
            "Edit",
            json!({"file_path":"notes.txt","old_string":"Monday","new_string":"Friday"}),
        )
    };
    let read =
        || tickloop_agent_test_support::call("read", "Read", json!({"file_path":"notes.txt"}));
    let model = Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![
        read(),
        tickloop_agent_test_support::text("read it"),
        tickloop_agent_test_support::text("Summary: the notes were read."),
        edit(),
        tickloop_agent_test_support::text("could not edit"),
        read(),
        tickloop_agent_test_support::text("read it again"),
        tickloop_agent_test_support::text("Summary: the notes were read again."),
        edit(),
        tickloop_agent_test_support::text("could not edit again"),
    ]));
    let agent = Agent::new_embedded(config(), model);
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let mut turn = async |prompt: &str| {
        agent.submit(chat, prompt.into()).await.unwrap();
        finish_turn(&agent, chat, &mut events).await.0
    };

    turn("Read the notes").await;
    agent.compact(chat).await.unwrap();
    let results = turn("Change the day").await;
    assert_eq!(result(&results, "edit").content, NOT_READ);

    turn("Read the notes again").await;
    let latest_prompt = agent
        .snapshot(chat)
        .await
        .unwrap()
        .rewind_points()
        .last()
        .unwrap()
        .message_id;
    agent
        .summarize_checkpoint(
            chat,
            DirectedSummaryRequest {
                message_id: latest_prompt,
                direction: SummaryDirection::FromHere,
                guidance: None,
            },
        )
        .await
        .unwrap();
    let results = turn("Change the day").await;
    assert_eq!(result(&results, "edit").content, NOT_READ);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "Delivery Monday\n");
    agent.shutdown().await;
}

#[tokio::test]
async fn write_and_edit_report_success_in_claude_code_words() {
    let directory = tempfile::tempdir().unwrap();
    let workdir = directory.path().canonicalize().unwrap();
    let report = workdir.join("report.txt").display().to_string();
    let draft = workdir.join("draft.txt").display().to_string();
    let model = Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![
        tickloop_agent_test_support::call(
            "create",
            "Write",
            json!({"file_path":"report.txt","content":"Delivery Monday\nPickup Monday\n"}),
        ),
        tickloop_agent_test_support::call(
            "edit",
            "Edit",
            json!({"file_path":"report.txt","old_string":"Delivery Monday","new_string":"Delivery Friday"}),
        ),
        tickloop_agent_test_support::call(
            "replace-all",
            "Edit",
            json!({"file_path":"report.txt","old_string":"Monday","new_string":"Tuesday","replace_all":true}),
        ),
        tickloop_agent_test_support::call(
            "overwrite",
            "Write",
            json!({"file_path":"report.txt","content":"Done\n"}),
        ),
        tickloop_agent_test_support::call(
            "create-by-edit",
            "Edit",
            json!({"file_path":"draft.txt","old_string":"","new_string":"Draft\n"}),
        ),
        tickloop_agent_test_support::text("done"),
    ]));
    let agent = Agent::new_embedded(config(), model);
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Write and revise the report".into())
        .await
        .unwrap();

    let (results, _) = finish_turn(&agent, chat, &mut events).await;
    for (call_id, expected) in [
        ("create", format!("File created successfully at: {report}")),
        (
            "edit",
            format!("The file {report} has been updated successfully."),
        ),
        (
            "replace-all",
            format!(
                "The file {report} has been updated. All occurrences were successfully replaced."
            ),
        ),
        (
            "overwrite",
            format!("The file {report} has been updated successfully."),
        ),
        (
            "create-by-edit",
            format!("The file {draft} has been updated successfully."),
        ),
    ] {
        let result = result(&results, call_id);
        assert!(!result.is_error, "{call_id}: {}", result.content);
        assert_eq!(result.content, expected, "{call_id}");
    }
    assert_eq!(
        std::fs::read_to_string(workdir.join("report.txt")).unwrap(),
        "Done\n"
    );
    assert_eq!(
        std::fs::read_to_string(workdir.join("draft.txt")).unwrap(),
        "Draft\n"
    );
    agent.shutdown().await;
}
