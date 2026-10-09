use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{
    host_tools::{HostToolPermission, HostToolProvider},
    model::{HttpModelClient, cards::ModelCatalog, codex_test_support::*},
    protocol::{ChatId, ToolDefinition},
    runtime::{Agent, EventKind},
    tools::ToolResult,
};

const TOOL_NAME: &str = "read_test_space";
const TOOL_OUTPUT: &str = "locality-marker:research";

#[derive(Default)]
struct TestSpaceTool {
    calls: Mutex<Vec<(ChatId, Value)>>,
    permission_checks: AtomicUsize,
}

#[async_trait]
impl HostToolProvider for TestSpaceTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: TOOL_NAME.into(),
            description: "Read a controlled test Space.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"space": {"type": "string", "const": "research"}},
                "required": ["space"],
                "additionalProperties": false
            }),
        }]
    }

    fn permission(&self, _: ChatId, _: &str, _: &Value) -> HostToolPermission {
        self.permission_checks.fetch_add(1, Ordering::SeqCst);
        HostToolPermission::Confirmation
    }

    async fn execute(
        &self,
        chat_id: ChatId,
        name: &str,
        input: Value,
        _: CancellationToken,
    ) -> ToolResult {
        assert_eq!(name, TOOL_NAME);
        self.calls.lock().unwrap().push((chat_id, input));
        ToolResult {
            content: TOOL_OUTPUT.into(),
            is_error: false,
            output_ref: None,
        }
    }
}

async fn run_agent_round_trip(agent: Agent, workdir: &std::path::Path) {
    let provider = Arc::new(TestSpaceTool::default());
    agent.install_host_tools(provider.clone()).unwrap();
    let chat = agent.create_chat(workdir.to_path_buf()).await.unwrap();
    let mut events = agent.subscribe();
    let run = agent
        .submit(chat, None, "Read the research test Space".into())
        .await
        .unwrap();
    let mut requested_permission = false;
    let mut completed_tool = false;
    let mut answer = String::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.chat_id != chat || event.run_id != run.run_id {
                continue;
            }
            match event.kind {
                EventKind::PermissionRequested { request } => {
                    assert!(!requested_permission);
                    assert!(provider.calls.lock().unwrap().is_empty());
                    requested_permission = true;
                    agent
                        .answer_permission(chat, request.id, true)
                        .await
                        .unwrap();
                }
                EventKind::ToolCompleted { result, .. } => {
                    assert!(!result.is_error);
                    assert_eq!(result.content, TOOL_OUTPUT);
                    completed_tool = true;
                }
                EventKind::Text(text) => answer.push_str(&text),
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("Agent tool round failed: {error}"),
                _ => {}
            }
        }
    })
    .await
    .expect("Agent tool round timed out");
    assert!(requested_permission);
    assert!(completed_tool);
    assert!(answer.contains("Research space read"));
    assert!(provider.permission_checks.load(Ordering::SeqCst) >= 1);
    assert_eq!(
        provider.calls.lock().unwrap().as_slice(),
        &[(chat, json!({"space":"research"}))]
    );
    agent.shutdown().await;
}

fn assert_wire_round_trip(requests: &[Value], codex: bool) {
    assert_eq!(requests.len(), 2);
    let definition = requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == TOOL_NAME)
        .expect("the new tool must be advertised by the real Adapter");
    assert_eq!(
        definition["parameters"]["properties"]["space"]["const"],
        "research"
    );
    let input = requests[1]["input"].as_array().unwrap();
    assert!(input.iter().any(|entry| {
        entry["type"] == "function_call_output"
            && entry["call_id"] == "call_host-call"
            && entry["output"] == TOOL_OUTPUT
    }));
    assert_eq!(
        requests[0]["model"],
        if codex { "gpt-5.5" } else { "deepseek-flash" }
    );
}

#[tokio::test]
async fn one_new_host_tool_round_trips_through_the_real_api_adapter() {
    let directory = tempfile::tempdir().unwrap();
    let (endpoint, requests) = serve_scripted_responses(vec![
        (
            200,
            function_call_stream("host-call", TOOL_NAME, json!({"space":"research"}), 12)
                .replace("response.done", "response.completed"),
        ),
        (
            200,
            reply_stream("host-finish", "Research space read", 15)
                .replace("response.done", "response.completed"),
        ),
    ])
    .await;
    let mut config = ModelCatalog::defaults()
        .get("deepseek-api", "deepseek-flash-responses")
        .unwrap();
    config.base_url = endpoint.as_str().trim_end_matches("/responses").into();
    config.api_key_env.clear();
    let client = HttpModelClient::new(config.clone()).unwrap();
    let agent = Agent::new_embedded(config, crate::model::fixture_client(Arc::new(client)));
    run_agent_round_trip(agent, directory.path()).await;
    assert_wire_round_trip(&requests.await.unwrap(), false);
}

#[tokio::test]
async fn the_same_new_host_tool_round_trips_through_the_real_codex_adapter() {
    let (_directory, state_dir) = state_with_credentials();
    let (endpoint, requests) = serve_scripted_responses(vec![
        (
            200,
            function_call_stream("host-call", TOOL_NAME, json!({"space":"research"}), 12),
        ),
        (200, reply_stream("host-finish", "Research space read", 15)),
    ])
    .await;
    let config = ModelCatalog::defaults()
        .get("openai-codex", "gpt-5.5-responses")
        .unwrap();
    let client = LoopbackCodexClient::new(config.clone(), &state_dir, endpoint.as_str());
    let agent = Agent::new_embedded(config, client.into_fixture_client());
    run_agent_round_trip(agent, &state_dir).await;
    assert_wire_round_trip(&requests.await.unwrap(), true);
}
