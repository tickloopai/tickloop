//! A Harness without an Execution Environment: its Chats have no Folder, offer WebFetch alone,
//! and touch no local file.

use std::{path::Path, sync::Arc};

use serde_json::json;
use tickloop_agent_base::network::Reach;
use tickloop_agent_protocol::{
    AgentHostOptions, Content, Decision, EventKind, ImageContext, ImageOrigin, InputContext,
    PermissionRule, PromptPolicy, Role,
};
use tickloop_agent_runtime::{
    Agent, AgentError, HarnessOptions, HarnessSeams, ToolPolicy, environment::LocalEnvironment,
    transcript::JsonlTranscriptStore,
};
use tickloop_agent_test_support::harness::ChosenModels;
use tickloop_agent_test_support::{ScriptedModel, call, next_event, text};
use uuid::Uuid;

const CHILD_CASE: &str = "TICKLOOP_FOLDERLESS_CHILD";
const SANDBOX_DIRECTORIES: [&str; 4] = ["home", "tmp", "config", "cwd"];

fn allow_web_fetch() -> PermissionRule {
    PermissionRule {
        id: Uuid::new_v4(),
        decision: Decision::Allow,
        tool: tickloop_agent_tools::WEB_FETCH_TOOL_NAME.into(),
        specifier: None,
        input: None,
        loop_id: None,
        persistent: false,
    }
}

/// The stage-one cloud assembly: no Folder, WebFetch alone reaching `reach`, an explicit
/// WebFetch allow, and no prompt that waits for a person.
fn folderless_agent(model: Arc<ScriptedModel>, reach: Reach) -> Agent {
    Agent::with_seams(
        HarnessSeams {
            transcripts: None,
            environment: None,
            models: ChosenModels::new(model),
        },
        HarnessOptions {
            host: AgentHostOptions {
                prompt_policy: PromptPolicy::DenyOnPrompt,
                ..AgentHostOptions::product_defaults()
            },
            tools: ToolPolicy {
                folderless_reach: reach,
                ..ToolPolicy::web_fetch_only()
            },
            host_rules: vec![allow_web_fetch()],
            ..HarnessOptions::local(tickloop_agent_test_support::config())
        },
    )
    .unwrap()
}

fn tool_names(request: &tickloop_agent_model::ModelRequest) -> Vec<&str> {
    request
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect()
}

fn system_prompt(request: &tickloop_agent_model::ModelRequest) -> &str {
    let message = request
        .messages
        .iter()
        .find(|message| message.role == Role::System)
        .unwrap();
    let Content::Text { text } = &message.content[0] else {
        panic!("the system message is text");
    };
    text
}

fn tool_result<'a>(
    request: &'a tickloop_agent_model::ModelRequest,
    call_id: &str,
) -> &'a tickloop_agent_protocol::ToolResultContent {
    request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|content| match content {
            Content::ToolResult {
                tool_use_id,
                content,
                ..
            } if tool_use_id == call_id => Some(content),
            _ => None,
        })
        .unwrap()
}

/// Runs one turn of `prompt` and returns every event of its Run.
async fn run_turn(agent: &Agent, chat: Uuid, prompt: &str) -> Vec<EventKind> {
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, prompt.into()).await.unwrap();
    let mut kinds = Vec::new();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != receipt.run_id {
            continue;
        }
        let ends = matches!(
            event.kind,
            EventKind::Completed { .. } | EventKind::Failed(_) | EventKind::Interrupted
        );
        kinds.push(event.kind);
        if ends {
            return kinds;
        }
    }
}

#[tokio::test]
async fn a_local_agent_keeps_every_tool_of_a_chat_with_a_folder() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![text("done")]));
    let agent = Agent::new_embedded(tickloop_agent_test_support::config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let events = run_turn(&agent, chat, "hello").await;
    assert!(matches!(events.last(), Some(EventKind::Completed { .. })));
    let requests = model.requests.lock().unwrap().clone();
    assert_eq!(
        tool_names(&requests[0]),
        [
            "Read",
            "Glob",
            "Grep",
            "Write",
            "Edit",
            "Bash",
            "TaskStop",
            "WebFetch",
            "WebSearch",
            "EnterPlanMode",
            "ExitPlanMode",
            "AskUserQuestion",
            "Subagent",
            "ListSubagents",
            "SendMessage",
            "StopSubagent",
            "GetGoal",
            "CreateGoal",
            "UpdateGoal",
            "CronCreate",
            "CronList",
            "CronDelete",
            "ScheduleWakeup",
            "Skill",
        ]
    );
    let prompt = system_prompt(&requests[0]);
    for section in [
        "# Ticks, Loops, and workspace displays",
        "# Subagent communication",
        "# Agent daemon lifecycle",
    ] {
        assert!(prompt.contains(section), "{section}");
    }
    agent.shutdown().await;
}

#[tokio::test]
async fn a_folderless_chat_offers_web_fetch_alone_and_hears_of_nothing_else() {
    let model = Arc::new(ScriptedModel::new(vec![
        call("first", "WebSearch", json!({"query": "news"})),
        text("done"),
    ]));
    let agent = folderless_agent(model.clone(), Reach::PublicAddresses);
    let chat = agent.create_chat_without_folder().await.unwrap();
    assert_eq!(agent.snapshot(chat).await.unwrap().folder, None);
    let events = run_turn(&agent, chat, "what is new").await;
    assert!(matches!(events.last(), Some(EventKind::Completed { .. })));
    let requests = model.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    // The second request follows a tool round, so its catalog was refreshed while the Chat's
    // search setting still turned WebSearch on.
    for request in requests.iter() {
        assert_eq!(tool_names(request), ["WebFetch"]);
        let prompt = system_prompt(request);
        for absent in [
            "AskUserQuestion",
            "Subagent",
            "TickManage",
            "Tickhost",
            "daemon",
            "/websearch",
            "workdir",
            "scratchpad",
            "workspace_host",
        ] {
            assert!(!prompt.contains(absent), "{absent}");
        }
    }
    agent.shutdown().await;
}

#[tokio::test]
async fn excepted_tool_names_return_disabled_results_without_any_wait() {
    let model = Arc::new(ScriptedModel::new(vec![
        [
            call("search", "WebSearch", json!({"query": "rust"})),
            call("task", "TaskCreate", json!({"title": "x"})),
        ]
        .concat(),
        text("done"),
    ]));
    let agent = folderless_agent(model.clone(), Reach::PublicAddresses);
    let chat = agent.create_chat_without_folder().await.unwrap();
    let events = run_turn(&agent, chat, "go").await;
    assert!(matches!(events.last(), Some(EventKind::Completed { .. })));
    assert!(!events.iter().any(|event| matches!(
        event,
        EventKind::PermissionRequested { .. } | EventKind::QuestionAsked { .. }
    )));
    let requests = model.requests.lock().unwrap().clone();
    let search = tool_result(&requests[1], "search").text();
    assert_eq!(
        search,
        "WebSearch is not available in this Chat. This call was not executed."
    );
    let task = tool_result(&requests[1], "task").text();
    assert!(
        task.starts_with("Task tools are not available in this Chat") && !task.contains("Settings"),
        "{task}"
    );
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(snapshot.permissions.is_empty() && snapshot.questions.is_empty());
    agent.shutdown().await;
}

#[tokio::test]
async fn web_fetch_is_allowed_by_the_host_rule_without_review_and_reaches_only_public_addresses() {
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "fetch",
            "WebFetch",
            json!({"url": "http://localhost:9/status", "prompt": "summarize"}),
        ),
        text("done"),
    ]));
    let reviewer = Arc::new(ScriptedModel::new(Vec::new()));
    let agent = folderless_agent(model.clone(), Reach::PublicAddresses);
    agent.set_auto_reviewer(reviewer.clone()).await;
    let chat = agent.create_chat_without_folder().await.unwrap();
    let events = run_turn(&agent, chat, "read it").await;
    assert!(matches!(events.last(), Some(EventKind::Completed { .. })));
    assert!(!events.iter().any(|event| matches!(
        event,
        EventKind::PermissionRequested { .. } | EventKind::ApprovalAssessed { .. }
    )));
    let completed = events
        .iter()
        .find_map(|event| match event {
            EventKind::ToolCompleted {
                call_id, result, ..
            } if call_id == "fetch" => Some(result),
            _ => None,
        })
        .expect("WebFetch ran");
    assert!(
        completed.is_error
            && completed.content.starts_with("localhost resolves to ")
            && completed.content.contains("which is not a public address"),
        "{completed:?}"
    );
    assert!(reviewer.requests.lock().unwrap().is_empty());
    assert_eq!(model.requests.lock().unwrap().len(), 2);
    agent.shutdown().await;
}

#[tokio::test]
async fn a_host_that_lets_folderless_chats_reach_anywhere_fetches_a_loopback_page() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0; 1024];
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let length = socket.read(&mut buffer).await.unwrap();
            request.extend_from_slice(&buffer[..length]);
        }
        let body = "<p>Delivery is on Friday.</p>";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
    });
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "fetch",
            "WebFetch",
            json!({"url": format!("http://localhost:{port}/delivery"), "prompt": "When?"}),
        ),
        text("Friday."),
        text("done"),
    ]));
    let agent = folderless_agent(model.clone(), Reach::Anywhere);
    let chat = agent.create_chat_without_folder().await.unwrap();
    let events = run_turn(&agent, chat, "read it").await;
    assert!(matches!(events.last(), Some(EventKind::Completed { .. })));
    let completed = events
        .iter()
        .find_map(|event| match event {
            EventKind::ToolCompleted {
                call_id, result, ..
            } if call_id == "fetch" => Some(result),
            _ => None,
        })
        .expect("WebFetch ran");
    assert!(
        !completed.is_error && completed.content == "Friday.",
        "{completed:?}"
    );
    server.await.unwrap();
    agent.shutdown().await;
}

#[tokio::test]
async fn a_folderless_chat_refuses_local_shells_files_and_attachments() {
    let model = Arc::new(ScriptedModel::new(Vec::new()));
    let agent = folderless_agent(model.clone(), Reach::PublicAddresses);
    let chat = agent.create_chat_without_folder().await.unwrap();
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("notes.txt"), "secret").unwrap();
    let unsupported = |result: Result<_, AgentError>| matches!(result, Err(AgentError::Invalid(message)) if message.starts_with("Unsupported: this Chat has no Folder"));
    assert!(unsupported(
        agent
            .submit_user_shell(chat, "pwd".into(), false)
            .await
            .map(|_| ())
    ));
    assert!(unsupported(
        agent
            .submit_with_file_references(
                chat,
                "read @notes.txt".into(),
                vec![directory.path().join("notes.txt")],
            )
            .await
            .map(|_| ())
    ));
    let mentioned_image = InputContext::Image(ImageContext {
        origin: ImageOrigin::Mentioned {
            path: directory.path().join("chart.png"),
        },
        image: tickloop_agent_protocol::Image {
            media_type: tickloop_agent_protocol::ImageMediaType::Png,
            data: "iVBORw0KGgo=".into(),
            width: 1,
            height: 1,
        },
    });
    assert!(unsupported(
        agent
            .submit_with_input_contexts(chat, "look".into(), vec![mentioned_image], Vec::new())
            .await
            .map(|_| ())
    ));
    assert!(unsupported(
        agent
            .set_directories(chat, directory.path().into(), Vec::new())
            .await
    ));
    assert!(matches!(
        agent.create_chat(directory.path().into()).await,
        Err(AgentError::Invalid(message)) if message.starts_with("Unsupported:")
    ));
    assert!(model.requests.lock().unwrap().is_empty());
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(snapshot.user_shells.is_empty() && snapshot.messages.is_empty());
    agent.shutdown().await;
}

#[tokio::test]
async fn a_folderless_harness_keeps_no_local_transcript_and_a_local_one_needs_a_folder() {
    let models = || ChosenModels::new(Arc::new(ScriptedModel::new(vec![text("answer")])));
    let options = || HarnessOptions {
        ..HarnessOptions::local(tickloop_agent_test_support::config())
    };
    let root = tempfile::tempdir().unwrap();
    let store = JsonlTranscriptStore::open(root.path().join("state")).unwrap();
    let folderless = Agent::with_seams(
        HarnessSeams {
            transcripts: Some(Arc::new(store)),
            environment: None,
            models: models(),
        },
        HarnessOptions {
            tools: ToolPolicy::web_fetch_only(),
            ..options()
        },
    )
    .unwrap();
    let chat = folderless.create_chat_without_folder().await.unwrap();
    run_turn(&folderless, chat, "prompt").await;
    folderless.sync().await.unwrap();
    folderless.shutdown().await;
    drop(folderless);
    assert_empty(&root.path().join("state"));
    let local = Agent::with_seams(
        HarnessSeams {
            transcripts: None,
            environment: Some(Arc::new(LocalEnvironment::bare())),
            models: models(),
        },
        options(),
    )
    .unwrap();
    assert!(local.create_chat_without_folder().await.is_err());
    local.shutdown().await;
    let settings_root = tempfile::tempdir().unwrap();
    let with_settings = Agent::with_seams(
        HarnessSeams {
            transcripts: None,
            environment: None,
            models: models(),
        },
        HarnessOptions {
            settings: Some(tickloop_agent_config::UserSettings::in_root(
                settings_root.path(),
            )),
            ..options()
        },
    );
    assert!(matches!(with_settings, Err(AgentError::Invalid(_))));
    assert_empty(settings_root.path());
}

#[tokio::test]
async fn local_folder_apis_refuse_a_folderless_harness_before_reading_anything() {
    let project = tempfile::tempdir().unwrap();
    let skill = project.path().join(".tickloop/skills/project-workflow");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: project-workflow\ndescription: A project Skill\n---\nBody",
    )
    .unwrap();
    let agent = folderless_agent(
        Arc::new(ScriptedModel::new(Vec::new())),
        Reach::PublicAddresses,
    );
    let refused = |result: Result<(), AgentError>| matches!(result, Err(AgentError::Invalid(message)) if message.starts_with("Unsupported: this Agent has no Execution Environment"));
    assert!(refused(agent.check_user_skill(
        project.path(),
        "project-workflow",
        ""
    )));
    // A missing path proves the refusal comes before canonicalization.
    assert!(refused(
        agent
            .trust_working_directory(project.path().join("missing"))
            .await
    ));
    agent.install_own_tickhost();
    let chat = agent.create_chat_without_folder().await.unwrap();
    assert!(
        agent
            .submit_user_shell(chat, "pwd".into(), false)
            .await
            .is_err()
    );
    agent.shutdown().await;
}

/// Runs a folderless turn in a child of this test binary whose HOME, temporary directory, config
/// root and working directory are fresh, and checks that every one of them stays empty. Network
/// diagnostics are switched on in the child's environment, which a folderless WebFetch, failing
/// here, must not consult: nothing reaches the child's stderr.
#[test]
fn a_folderless_turn_leaves_home_temp_config_and_cwd_untouched() {
    if std::env::var_os(CHILD_CASE).is_some() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(folderless_turn_in_child());
        return;
    }
    let sandbox = tempfile::tempdir().unwrap();
    let directory = |name: &str| sandbox.path().join(name);
    for name in SANDBOX_DIRECTORIES {
        std::fs::create_dir(directory(name)).unwrap();
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "folderless_chats::a_folderless_turn_leaves_home_temp_config_and_cwd_untouched",
            "--nocapture",
        ])
        .current_dir(directory("cwd"))
        .env(CHILD_CASE, "1")
        .env("HOME", directory("home"))
        .env("TICKLOOP_TMPDIR", directory("tmp"))
        .env("TICKLOOP_CONFIG_DIR", directory("config"))
        .env("TICKLOOP_NETWORK_DIAGNOSTICS", "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "{stdout}{stderr}"
    );
    assert!(
        !stdout.contains("network phase=") && !stderr.contains("network phase="),
        "{stdout}{stderr}"
    );
    for name in SANDBOX_DIRECTORIES {
        assert_empty(&directory(name));
    }
}

fn assert_empty(directory: &Path) {
    let entries = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert!(entries.is_empty(), "{}: {entries:?}", directory.display());
}

async fn folderless_turn_in_child() {
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "fetch",
            "WebFetch",
            json!({"url": "https://localhost:9/", "prompt": "summarize"}),
        ),
        call("search", "WebSearch", json!({"query": "rust"})),
        text("done"),
    ]));
    let agent = folderless_agent(model.clone(), Reach::PublicAddresses);
    agent.install_own_tickhost();
    let chat = agent.create_chat_without_folder().await.unwrap();
    let events = run_turn(&agent, chat, "fetch and search").await;
    assert!(matches!(events.last(), Some(EventKind::Completed { .. })));
    assert_eq!(model.requests.lock().unwrap().len(), 3);
    agent.flush_transcripts().await;
    agent.shutdown().await;
}
