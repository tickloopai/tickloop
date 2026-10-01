use std::{path::PathBuf, sync::Arc, time::Duration};

use serde_json::{Value, json};
use tickloop_agent::{
    artifacts::ArtifactClient,
    host_tools::{HostCallResult, HostToolProvider},
    mcp_tools::McpHostToolProvider,
    model::{ModelAuthSource, model_client},
    permissions::ExecutionMode,
    runtime::{Agent, ChatId, EventKind, LoopState},
    settings::UserSettings,
};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

const LIVE_TIMEOUT: Duration = Duration::from_secs(240);

async fn finish_turn(
    agent: &Agent,
    chat: ChatId,
    events: &mut broadcast::Receiver<tickloop_agent::runtime::Event>,
    approved_tools: &[&str],
) {
    tokio::time::timeout(LIVE_TIMEOUT, async {
        loop {
            let event = events.recv().await.expect("live event stream ended");
            if event.chat_id != chat {
                continue;
            }
            match event.kind {
                EventKind::PermissionRequested { request } => {
                    let allowed = approved_tools.contains(&request.name.as_str());
                    agent
                        .answer_permission(chat, request.id, allowed)
                        .await
                        .unwrap();
                    assert!(allowed, "unexpected live mutation: {}", request.name);
                }
                EventKind::ToolCompleted { result, .. } => {
                    if result.is_error {
                        eprintln!("live artifact tool returned an error: {}", result.content);
                    }
                }
                EventKind::Completed { usage } => {
                    eprintln!("live artifact turn completed: {usage:?}");
                    break;
                }
                EventKind::Failed(_) => panic!("live model request failed"),
                EventKind::QuestionAsked { .. } => panic!("live model requested additional input"),
                _ => {}
            }
        }
    })
    .await
    .expect("live artifact turn timed out");
}

async fn fixture_tool(
    provider: &McpHostToolProvider,
    chat: ChatId,
    name: &str,
    input: Value,
) -> Value {
    let cancel = CancellationToken::new();
    let prepared = provider
        .prepare_call(chat, name, &input, cancel.clone())
        .await
        .unwrap();
    provider
        .revalidate_call(chat, &prepared, cancel.clone())
        .await
        .unwrap();
    let result = match provider.execute_prepared(chat, prepared, cancel).await {
        HostCallResult::Completed(result) | HostCallResult::CompletedWithEffect(result, _) => {
            result
        }
        _ => panic!("fixture operation did not complete"),
    };
    assert!(!result.is_error, "fixture tool failed: {name}");
    let result: Value = serde_json::from_str(&result.content).unwrap();
    result["structuredContent"].clone()
}

#[tokio::test]
#[ignore = "requires explicit live model configuration and the test-only local Artifact Worker bridge"]
async fn real_model_publishes_updates_and_handles_owner_feedback() {
    assert_eq!(std::env::var("TICKLOOP_LIVE_ARTIFACTS").as_deref(), Ok("1"));
    let configured_root = PathBuf::from(
        std::env::var_os("TICKLOOP_LIVE_MODEL_ROOT").expect("set TICKLOOP_LIVE_MODEL_ROOT"),
    );
    let config = UserSettings::in_root(&configured_root)
        .load()
        .configured_model()
        .unwrap()
        .expect("select a model first");
    assert_eq!(
        config.auth_source,
        ModelAuthSource::ApiKeyEnv,
        "this harness uses inherited API-key environment only"
    );
    eprintln!(
        "live artifact model: {}/{}",
        config.provider_id, config.model_id
    );
    let client = model_client(config.clone(), None).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let workspace = root.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let origin = std::env::var("TICKLOOP_MCP_BRIDGE_ORIGIN")
        .unwrap_or_else(|_| "http://127.0.0.1:8788".into());
    assert!(
        origin.starts_with("http://127.0.0.1:"),
        "use only the local fixture bridge"
    );
    let artifact = ArtifactClient::for_origin(root.join("artifact-account"), &origin).unwrap();
    let device = artifact.start_device_login().await.unwrap();
    let approved = reqwest::Client::new()
        .post(format!("{origin}/fixture/approve"))
        .json(&json!({"user_code": device.user_code}))
        .send()
        .await
        .unwrap();
    assert!(approved.status().is_success());
    artifact
        .finish_device_login(device, &CancellationToken::new())
        .await
        .unwrap();
    let provider = Arc::new(
        McpHostToolProvider::new(artifact, None).with_event_bridge("tickloop://event-bridge/v1"),
    );
    let agent = Agent::open_embedded(root.join("agent-state"), config, client).unwrap();
    agent.install_host_tools(provider.clone()).unwrap();
    let chat = agent.create_chat(workspace).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let first_operation = uuid::Uuid::new_v4().to_string();
    let first_source = "# Live artifact validation\n\nVersion one.";
    agent.submit(chat, None, format!(
        "This is a controlled integration test. Use only the available MCP artifact tools. Publish exactly one private Markdown artifact titled Live artifact validation, operation_id {first_operation}, with this exact source: {first_source:?}. Do not share it. Then briefly confirm completion."
    )).await.unwrap();
    finish_turn(
        &agent,
        chat,
        &mut events,
        &["mcp__artifacts__artifact_publish"],
    )
    .await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    let goal = snapshot.loops.last().unwrap();
    assert_eq!(goal.state, LoopState::WaitingTrigger);
    assert_eq!(goal.event_sources.len(), 1);
    let binding = goal.event_sources[0].clone();
    let loop_id = goal.id;
    assert_eq!(binding.association_epoch, Some(1));
    let first = fixture_tool(
        &provider,
        chat,
        "mcp__artifacts__artifact_read",
        json!({"artifact_id": binding.source_key}),
    )
    .await;
    assert_eq!(first["source"], first_source);

    let second_source = "# Live artifact validation\n\nVersion two.";
    let second_operation = uuid::Uuid::new_v4().to_string();
    agent.submit(chat, Some(loop_id), format!(
        "Update the same private artifact {} using operation_id {second_operation} and expected_version_id {}. Publish exactly this source: {second_source:?}. Use only artifact MCP tools, keep it private, then confirm. Do not create another artifact.", binding.source_key, binding.publication.as_ref().unwrap().version_id
    )).await.unwrap();
    finish_turn(
        &agent,
        chat,
        &mut events,
        &["mcp__artifacts__artifact_publish"],
    )
    .await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    let goal = snapshot
        .loops
        .iter()
        .find(|goal| goal.id == loop_id)
        .unwrap();
    assert_eq!(goal.event_sources.len(), 1);
    assert_eq!(goal.event_sources[0].binding_id, binding.binding_id);
    assert_eq!(goal.event_sources[0].association_epoch, Some(1));
    assert!(!goal.event_sources[0].association_unknown);
    let second = fixture_tool(
        &provider,
        chat,
        "mcp__artifacts__artifact_read",
        json!({"artifact_id": binding.source_key}),
    )
    .await;
    assert_eq!(second["source"], second_source);
    assert_ne!(second["version_id"], first["version_id"]);

    fixture_tool(&provider, chat, "mcp__artifacts__artifact_comment_add", json!({
        "artifact_id": binding.source_key,
        "operation_id": uuid::Uuid::new_v4().to_string(),
        "version_id": second["version_id"],
        "quote": "Version two.",
        "text": "Replace the exact quoted text with the literal marker FEEDBACK_APPLIED (without quotes). Publish the updated version, reply to this artifact comment, and then use artifact_feedback_complete with the confirmed reply/publication operation IDs and new version. Use only artifact MCP tools and keep the artifact private.",
        "send_to_agent": true,
    })).await;
    finish_turn(
        &agent,
        chat,
        &mut events,
        &[
            "mcp__artifacts__artifact_publish",
            "mcp__artifacts__artifact_feedback_reply",
            "mcp__artifacts__artifact_feedback_complete",
        ],
    )
    .await;
    let final_source = fixture_tool(
        &provider,
        chat,
        "mcp__artifacts__artifact_read",
        json!({"artifact_id": binding.source_key}),
    )
    .await;
    let final_text = final_source["source"].as_str().unwrap();
    assert!(
        final_text.contains("FEEDBACK_APPLIED"),
        "feedback marker missing from fixture source: {final_text}"
    );
    assert!(
        !final_text.contains("Version two."),
        "quoted text was not replaced: {final_text}"
    );
    let comments = fixture_tool(
        &provider,
        chat,
        "mcp__artifacts__artifact_comments",
        json!({"artifact_id": binding.source_key}),
    )
    .await;
    assert!(
        comments["comments"]
            .as_array()
            .unwrap()
            .iter()
            .any(|comment| comment["feedback_status"] == "replied")
    );
    let versions = fixture_tool(
        &provider,
        chat,
        "mcp__artifacts__artifact_versions",
        json!({"artifact_id": binding.source_key}),
    )
    .await;
    assert_eq!(versions["versions"].as_array().unwrap().len(), 3);
    agent.shutdown().await;
    fixture_tool(
        &provider,
        chat,
        "mcp__artifacts__artifact_delete",
        json!({
            "artifact_id": binding.source_key, "operation_id": uuid::Uuid::new_v4().to_string(),
        }),
    )
    .await;
    eprintln!("live artifact publication, same-Loop update and claimed feedback passed");
}
