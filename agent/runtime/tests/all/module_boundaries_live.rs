use std::{path::Path, time::Duration};

use tickloop_agent_model::{cards::ModelCatalog, model_client};
use tickloop_agent_protocol::{
    ChatId, Decision, EventKind, ExecutionMode, ModelConfig, ModelRequestKind, PermissionRule,
    RequestOutcome,
};
use tickloop_agent_runtime::Agent;

const LIVE_TIMEOUT: Duration = Duration::from_secs(240);
const REVIEWED_COMMAND: &str = "cp probe.txt reviewed.txt";

#[derive(Default)]
struct TurnEvidence {
    answer: String,
    tool_results: Vec<String>,
    approved_by_reviewer: bool,
}

async fn complete_turn(
    agent: &Agent,
    chat: ChatId,
    prompt: String,
    expected_tool: Option<&str>,
) -> TurnEvidence {
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, None, prompt).await.unwrap();
    tokio::time::timeout(LIVE_TIMEOUT, async {
        let mut evidence = TurnEvidence::default();
        loop {
            let event = events.recv().await.unwrap();
            if event.chat_id != chat || event.run_id != receipt.run_id {
                continue;
            }
            match event.kind {
                EventKind::Text(text) => evidence.answer.push_str(&text),
                EventKind::ToolStarted { name, input, .. } => {
                    assert_eq!(Some(name.as_str()), expected_tool);
                    if name == "Bash" {
                        assert_eq!(input["command"], REVIEWED_COMMAND);
                    }
                }
                EventKind::ToolCompleted { result, .. } => {
                    assert!(!result.is_error, "temporary validation tool failed");
                    evidence.tool_results.push(result.content);
                }
                EventKind::PermissionChecked { decision, .. } => {
                    assert_eq!(decision, Decision::Allow, "Auto review did not approve");
                    evidence.approved_by_reviewer = true;
                }
                EventKind::PermissionRequested { .. } => {
                    panic!("live validation unexpectedly requires manual permission");
                }
                EventKind::Completed { usage } => {
                    eprintln!("live turn completed: usage={usage:?}");
                    return evidence;
                }
                EventKind::Failed(error) => panic!("live request failed: {error}"),
                EventKind::ModelRetry { attempt, error, .. } => {
                    eprintln!("live request retry {attempt}: {error}");
                }
                _ => {}
            }
        }
    })
    .await
    .expect("live turn timed out")
}

fn open_agent(archive: &Path, auth_root: &Path, config: &ModelConfig) -> Agent {
    Agent::open_embedded(
        archive.to_owned(),
        config.clone(),
        model_client(config.clone(), Some(auth_root)).unwrap(),
    )
    .unwrap()
}

async fn verify_live_lifecycle(provider: &str, model: &str) {
    let auth_root = std::env::var_os("TICKLOOP_LIVE_AUTH_ROOT")
        .map(std::path::PathBuf::from)
        .expect("set TICKLOOP_LIVE_AUTH_ROOT to the existing Tickloop state directory")
        .canonicalize()
        .unwrap();
    let config = ModelCatalog::defaults().get(provider, model).unwrap();
    eprintln!("live lifecycle: provider={provider}, model={model}");
    let directory = tempfile::tempdir().unwrap();
    let workdir = directory.path().join("work");
    let archive = directory.path().join("archive");
    std::fs::create_dir(&workdir).unwrap();
    let marker = format!("file-marker-{}", uuid::Uuid::new_v4());
    std::fs::write(workdir.join("probe.txt"), &marker).unwrap();
    let agent = open_agent(&archive, &auth_root, &config);
    let chat = agent.create_chat(workdir.clone()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Auto)
        .await
        .unwrap();

    let file = complete_turn(
        &agent,
        chat,
        "Use Read to read probe.txt in the current working directory. Reply with its exact contents. Use no other tool.".into(),
        Some("Read"),
    ).await;
    assert!(file.answer.contains(&marker));
    assert!(
        file.tool_results
            .iter()
            .any(|result| result.contains(&marker))
    );

    agent
        .set_permission_rules(
            chat,
            vec![PermissionRule {
                id: uuid::Uuid::new_v4(),
                decision: Decision::Ask,
                tool: "Bash".into(),
                specifier: Some("cp *".into()),
                input: None,
                loop_id: None,
                persistent: false,
            }],
        )
        .await
        .unwrap();
    let shell = complete_turn(
        &agent,
        chat,
        format!("This validation authorizes exactly the shell command `{REVIEWED_COMMAND}` in the current working directory to copy our temporary file to reviewed.txt. Execute it using the Bash tool, then reply with the exact original file contents from our earlier turn. Use no other tool or command."),
        Some("Bash"),
    ).await;
    assert!(shell.approved_by_reviewer);
    assert_eq!(
        std::fs::read_to_string(workdir.join("reviewed.txt")).unwrap(),
        marker
    );
    assert!(shell.answer.contains(&marker));
    assert!(!shell.tool_results.is_empty());
    assert!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .request_usage
            .entries()
            .iter()
            .any(|entry| {
                entry.kind == ModelRequestKind::PermissionCheck
                    && entry.outcome == RequestOutcome::Accepted
            })
    );
    eprintln!("live file and Auto-reviewed shell round trips passed");

    tokio::time::timeout(LIVE_TIMEOUT, agent.compact(chat))
        .await
        .unwrap()
        .unwrap();
    let recall = "Reply with only the exact contents of probe.txt from our earlier tool results. Do not use any tools.";
    let compacted = complete_turn(&agent, chat, recall.into(), None).await;
    assert!(compacted.answer.contains(&marker));
    assert!(compacted.tool_results.is_empty());
    eprintln!("live compaction and continuation passed");

    let other_chat = agent.create_chat(workdir.clone()).await.unwrap();
    let other_marker = format!("other-chat-{}", uuid::Uuid::new_v4());
    let other = complete_turn(&agent, other_chat, format!("Remember this marker for this conversation: {other_marker}. Reply with only this marker. Do not use tools."), None).await;
    assert!(other.answer.contains(&other_marker));
    assert!(!other.answer.contains(&marker));
    let original = complete_turn(&agent, chat, recall.into(), None).await;
    assert!(original.answer.contains(&marker));
    assert!(!original.answer.contains(&other_marker));
    agent.shutdown().await;

    let reopened = open_agent(&archive, &auth_root, &config);
    reopened.resume_chat(chat).await.unwrap();
    let restored = complete_turn(&reopened, chat, recall.into(), None).await;
    assert!(restored.answer.contains(&marker));
    assert!(restored.tool_results.is_empty());
    eprintln!("live Chat isolation and restart recovery passed");

    let cancel_chat = reopened.create_chat(workdir).await.unwrap();
    let mut events = reopened.subscribe();
    let receipt = reopened.submit(cancel_chat, None, "Count from 1 to 100000, one number per line, without skipping numbers or using any tools. Start the list immediately.".into()).await.unwrap();
    tokio::time::timeout(LIVE_TIMEOUT, async {
        let mut interrupted = false;
        loop {
            let event = events.recv().await.unwrap();
            if event.chat_id != cancel_chat || event.run_id != receipt.run_id {
                continue;
            }
            match event.kind {
                EventKind::Text(_) | EventKind::Thinking(_) if !interrupted => {
                    reopened.interrupt(cancel_chat).await.unwrap();
                    interrupted = true;
                }
                EventKind::Interrupted => {
                    assert!(interrupted);
                    break;
                }
                EventKind::Completed { .. } => {
                    panic!("request completed before cancellation was observed")
                }
                EventKind::Failed(error) => panic!("cancellation request failed: {error}"),
                EventKind::ToolStarted { .. } => {
                    panic!("cancellation probe unexpectedly used a tool")
                }
                _ => {}
            }
        }
    })
    .await
    .expect("live cancellation timed out");
    reopened.shutdown().await;
    eprintln!("live stream cancellation passed");
}

#[tokio::test]
#[ignore = "calls the billed DeepSeek API and reviews an authorized temporary file command"]
async fn deepseek_model_module_lifecycle() {
    verify_live_lifecycle("deepseek-api", "deepseek-flash").await;
}

#[tokio::test]
#[ignore = "uses existing Tickloop Codex credentials and reviews an authorized temporary file command"]
async fn codex_model_module_lifecycle() {
    verify_live_lifecycle("openai-codex", "gpt-5.6-sol").await;
}

#[tokio::test]
#[ignore = "calls both existing real providers; use proxy environment variables for this smoke check"]
async fn environment_proxy_text_requests() {
    let auth_root = std::path::PathBuf::from(std::env::var_os("TICKLOOP_LIVE_AUTH_ROOT").unwrap())
        .canonicalize()
        .unwrap();
    for (provider, model) in [
        ("deepseek-api", "deepseek-flash"),
        ("openai-codex", "gpt-5.6-sol"),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let config = ModelCatalog::defaults().get(provider, model).unwrap();
        let agent = Agent::new_embedded(
            config.clone(),
            model_client(config, Some(&auth_root)).unwrap(),
        );
        let chat = agent
            .create_chat(directory.path().to_owned())
            .await
            .unwrap();
        let marker = format!("connection-{}", uuid::Uuid::new_v4());
        let turn = complete_turn(
            &agent,
            chat,
            format!("Reply with exactly {marker}. Do not use tools."),
            None,
        )
        .await;
        assert!(turn.answer.contains(&marker));
        agent.shutdown().await;
        eprintln!("environment proxy text passed: {provider}/{model}");
    }
}
