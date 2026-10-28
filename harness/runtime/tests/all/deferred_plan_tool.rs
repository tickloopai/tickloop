//! EnterPlanMode is deferred behind tool search, as Claude Code's `shouldDefer`: its name is
//! known, its definition loads on demand, and entering Plan needs no confirmation.

use serde_json::{Value, json};
use tickloop_harness_model::ModelRequest;
use tickloop_harness_protocol::{ChatId, Content, EventKind, ExecutionMode, Role, ToolDefinition};
use tickloop_harness_runtime::Agent;
use tickloop_harness_test_support::{ScriptedModel, call, config, next_event, text};

const ENTER: &str = "EnterPlanMode";
const EXIT: &str = "ExitPlanMode";

fn tool_names(tools: &[ToolDefinition]) -> Vec<&str> {
    tools.iter().map(|tool| tool.name.as_str()).collect()
}

fn system_text(request: &ModelRequest) -> String {
    request
        .messages
        .iter()
        .filter(|message| message.role == Role::System)
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// Submits `prompt`, answering no permission request, and returns the names of the tools that
/// asked and every tool result in order.
async fn run_turn(agent: &Agent, chat: ChatId, prompt: &str) -> (Vec<String>, Vec<String>) {
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, prompt.into()).await.unwrap();
    let mut asked = Vec::new();
    let mut results = Vec::new();
    loop {
        let event = next_event(&mut events).await;
        match event.kind {
            EventKind::PermissionRequested { request } => {
                asked.push(request.name.clone());
                agent
                    .answer_permission(chat, request.id, false)
                    .await
                    .unwrap();
            }
            EventKind::ToolCompleted { result, .. } => results.push(result.content),
            EventKind::Completed { .. } if event.run_id == receipt.run_id => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    (asked, results)
}

fn tool_result_text(request: &ModelRequest, call_id: &str) -> String {
    request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|content| match content {
            Content::ToolResult {
                tool_use_id,
                content,
                ..
            } if tool_use_id == call_id => Some(content.text().into_owned()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no result for {call_id}"))
}

#[tokio::test]
async fn searchtools_reaches_enter_plan_mode_without_any_mcp_server() {
    let model = std::sync::Arc::new(ScriptedModel::new(vec![
        call("search", "SearchTools", json!({"query": "plan mode"})),
        call("describe", "DescribeTools", json!({"names": [ENTER]})),
        call(
            "execute",
            "ExecuteTools",
            json!({"calls": [{"name": ENTER, "arguments": {}}]}),
        ),
        text("Planning."),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();

    let (asked, _) = run_turn(&agent, chat, "Add caching").await;

    assert!(asked.is_empty(), "{asked:?}");
    assert_eq!(
        agent.snapshot(chat).await.unwrap().policy.mode,
        ExecutionMode::Plan
    );
    let requests = model.requests.lock().unwrap().clone();
    let listed = tool_names(&requests[0].tools);
    for fold in ["SearchTools", "DescribeTools", "ExecuteTools", EXIT] {
        assert!(listed.contains(&fold), "{fold} missing from {listed:?}");
    }
    assert!(!listed.contains(&ENTER), "{listed:?}");
    assert!(requests[0].deferred_tools.is_empty());
    assert!(
        system_text(&requests[0]).contains(ENTER),
        "the system prompt names the deferred tool"
    );
    let found: Value = serde_json::from_str(&tool_result_text(&requests[1], "search")).unwrap();
    assert_eq!(found["matches"][0]["name"], ENTER, "{found}");
    assert!(found["matches"][0].get("server").is_none(), "{found}");
    let described: Value =
        serde_json::from_str(&tool_result_text(&requests[2], "describe")).unwrap();
    assert!(
        described["tools"][0]["description"]
            .as_str()
            .unwrap()
            .starts_with("Use this tool proactively when you're about to start"),
        "{described}"
    );
    let executed = tool_result_text(&requests[3], "execute");
    assert!(executed.contains("Entered plan mode."), "{executed}");
    agent.shutdown().await;
}

#[tokio::test]
async fn native_search_defers_enter_plan_mode_without_any_mcp_server() {
    use tickloop_harness_model::cards::ModelCatalog;
    let model = std::sync::Arc::new(ScriptedModel::new(vec![
        call("enter", ENTER, json!({})),
        text("Planning."),
    ]));
    let mut native = ModelCatalog::defaults()
        .get("anthropic-api", "claude-sonnet-5")
        .unwrap();
    native.base_url = config().base_url;
    native.api_key_env.clear();
    let agent = Agent::new_embedded(native, model.clone());
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();

    let (asked, results) = run_turn(&agent, chat, "Add caching").await;

    assert!(asked.is_empty(), "{asked:?}");
    assert!(results[0].starts_with("Entered plan mode."), "{results:?}");
    let requests = model.requests.lock().unwrap().clone();
    assert_eq!(tool_names(&requests[0].deferred_tools), [ENTER]);
    let listed = tool_names(&requests[0].tools);
    assert!(!listed.contains(&ENTER), "{listed:?}");
    assert!(listed.contains(&EXIT), "{listed:?}");
    assert!(
        !listed.contains(&"SearchTools"),
        "native search needs no Fold tools: {listed:?}"
    );
    agent.shutdown().await;
}

/// Entering and leaving Plan, by the tool or by the user, never changes the tools a request
/// lists or defers, so the prompt cache prefix survives every mode switch.
#[tokio::test]
async fn the_offered_tools_are_the_same_bytes_in_every_execution_mode() {
    let model = std::sync::Arc::new(ScriptedModel::new(vec![
        call("enter", ENTER, json!({})),
        text("Planning."),
        text("Auto."),
        text("Full access."),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();

    run_turn(&agent, chat, "Plan it").await;
    for mode in [ExecutionMode::Auto, ExecutionMode::FullAccess] {
        agent.set_execution_mode(chat, mode).await.unwrap();
        run_turn(&agent, chat, "Go on").await;
    }

    let requests = model.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 4);
    let offered = |request: &ModelRequest| {
        serde_json::to_vec(&(&request.tools, &request.deferred_tools)).unwrap()
    };
    for request in &requests[1..] {
        assert!(
            offered(request) == offered(&requests[0]),
            "{:?}",
            tool_names(&request.tools)
        );
    }
    agent.shutdown().await;
}

/// ExecuteTools runs its calls concurrently, so a batch that would decide other calls while the
/// mode switches to Plan is refused whole; EnterPlanMode runs there only on its own.
#[tokio::test]
async fn execute_tools_refuses_to_enter_plan_mode_beside_other_calls() {
    let model = std::sync::Arc::new(ScriptedModel::new(vec![
        call(
            "execute",
            "ExecuteTools",
            json!({"calls": [
                {"name": ENTER, "arguments": {}},
                {"name": "Write", "arguments": {"file_path": "notes.md", "content": "x"}},
            ]}),
        ),
        text("Stopped."),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();

    run_turn(&agent, chat, "Plan and write").await;

    let requests = model.requests.lock().unwrap().clone();
    let refused = tool_result_text(&requests[1], "execute");
    assert!(
        refused.contains("EnterPlanMode must be the only call"),
        "{refused}"
    );
    assert_ne!(
        agent.snapshot(chat).await.unwrap().policy.mode,
        ExecutionMode::Plan
    );
    assert!(!directory.path().join("notes.md").exists());
    agent.shutdown().await;
}
