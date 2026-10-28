//! Anthropic prompt caching on the wire: where a request's `cache_control` breakpoints go, which
//! Providers receive them, and the cache usage the service reports back.

use std::path::Path;

use serde_json::{Value, json};
use tickloop_harness_model::{
    LocalModelClients, ModelClients, ModelOutput, ModelRequest,
    cards::{self, ModelCatalog},
    credentials::save_api_key,
    prompt_cache::PromptCache,
};
use tickloop_harness_protocol::{Content, Message, ModelConfig, Role, ToolDefinition, Usage};
use tickloop_harness_test_support::model_http_support::serve_responses;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const EPHEMERAL: &str = r#"{"type":"ephemeral"}"#;

fn catalog_model(root: &Path, provider: &str, model: &str, endpoint: String) -> ModelConfig {
    let mut config = ModelCatalog::defaults().get(provider, model).unwrap();
    if !config.api_key_env.is_empty() {
        save_api_key(root, &config.api_key_env, "test-key").unwrap();
    }
    config.base_url = endpoint;
    config
}

fn message(role: Role, content: Vec<Content>) -> Message {
    Message { role, content }
}

fn text(text: &str) -> Content {
    Content::Text { text: text.into() }
}

fn read_tool() -> ToolDefinition {
    ToolDefinition {
        name: "Read".into(),
        description: "Read a file".into(),
        input_schema: json!({"type":"object","properties":{"file_path":{"type":"string"}}}),
    }
}

/// A conversation request as the runtime sends it: the system prompt, the user context message
/// and then the Chat's history.
fn conversation(history: Vec<Message>) -> ModelRequest {
    let mut messages = vec![
        message(Role::System, vec![text("You are Tickloop.")]),
        message(
            Role::User,
            vec![text("<system-reminder>\nInstructions</system-reminder>")],
        ),
    ];
    messages.extend(history);
    ModelRequest {
        history_sources: vec![None; messages.len()],
        messages,
        tools: vec![read_tool()],
        deferred_tools: Vec::new(),
        structured_output: None,
        temperature: None,
        reasoning_effort: None,
    }
}

fn event(event_type: &str, mut data: Value) -> String {
    data["type"] = json!(event_type);
    format!("event: {event_type}\ndata: {data}\n\n")
}

fn reply(blocks: &[Value], usage_start: Value, usage_end: Value, stop: &str) -> String {
    let mut events = vec![event(
        "message_start",
        json!({"message":{"id":"msg","role":"assistant","content":[],"usage":usage_start}}),
    )];
    for (index, block) in blocks.iter().enumerate() {
        events.push(event(
            "content_block_start",
            json!({"index":index,"content_block":block}),
        ));
        events.push(event("content_block_stop", json!({"index":index})));
    }
    events.push(event(
        "message_delta",
        json!({"delta":{"stop_reason":stop},"usage":usage_end}),
    ));
    events.push(event("message_stop", json!({})));
    events.concat()
}

fn text_reply() -> String {
    reply(
        &[json!({"type":"text","text":"Done."})],
        json!({"input_tokens":1,"output_tokens":1}),
        json!({"output_tokens":2}),
        "end_turn",
    )
}

async fn complete(config: &ModelConfig, root: &Path, request: ModelRequest) -> ModelOutput {
    let client = LocalModelClients::new(Some(root.to_path_buf()))
        .client(config)
        .unwrap();
    let (events, _) = mpsc::unbounded_channel();
    client
        .complete(request, events, CancellationToken::new())
        .await
        .unwrap()
}

/// Every `cache_control` value in `wire`, with the path to its block.
fn breakpoints(wire: &Value) -> Vec<(String, String)> {
    let mut found = Vec::new();
    for (index, block) in wire["system"].as_array().into_iter().flatten().enumerate() {
        if let Some(control) = block.get("cache_control") {
            found.push((format!("system/{index}"), control.to_string()));
        }
    }
    for (message, entry) in wire["messages"].as_array().unwrap().iter().enumerate() {
        for (index, block) in entry["content"].as_array().unwrap().iter().enumerate() {
            if let Some(control) = block.get("cache_control") {
                found.push((format!("messages/{message}/{index}"), control.to_string()));
            }
        }
    }
    assert_eq!(
        found.len(),
        wire.to_string().matches("cache_control").count(),
        "breakpoints only on top-level system and message blocks: {wire}"
    );
    found
}

fn paths(wire: &Value) -> Vec<String> {
    let found = breakpoints(wire);
    assert!(found.len() <= 4, "at most four breakpoints: {found:?}");
    assert!(
        found.iter().all(|(_, control)| control == EPHEMERAL),
        "the default five-minute lifetime: {found:?}"
    );
    found.into_iter().map(|(path, _)| path).collect()
}

fn without_breakpoints(mut value: Value) -> Value {
    match &mut value {
        Value::Object(object) => {
            object.remove("cache_control");
            for item in object.values_mut() {
                *item = without_breakpoints(item.take());
            }
        }
        Value::Array(items) => {
            for item in items {
                *item = without_breakpoints(item.take());
            }
        }
        _ => {}
    }
    value
}

#[test]
fn only_the_anthropic_api_sends_cache_breakpoints() {
    let catalog = ModelCatalog::defaults();
    catalog.validate().unwrap();
    for profile in catalog.profiles() {
        let expected = (profile.provider_id == "anthropic-api").then_some(PromptCache::Anthropic);
        assert_eq!(
            cards::prompt_cache(&profile),
            expected,
            "{}/{}",
            profile.provider_id,
            profile.model_id
        );
    }
    let custom = ModelCatalog::defaults()
        .with_providers(
            serde_json::from_value(json!([{
                "id": "claude-gateway",
                "display_name": "Claude gateway",
                "auth": "api_key_env",
                "api_key_env": "GATEWAY_KEY",
                "endpoints": {"messages": "https://gateway.example"},
                "models": [{"id": "claude-opus-5-5", "display_name": "Opus", "adapter": "messages"}],
            }]))
            .unwrap(),
        )
        .unwrap()
        .get("claude-gateway", "claude-opus-5-5")
        .unwrap();
    assert_eq!(cards::prompt_cache(&custom), None);
}

#[tokio::test]
async fn the_second_request_repeats_the_first_up_to_its_moving_tail_breakpoint() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let read_call =
        json!({"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"notes.txt"}});
    let (endpoint, mut captured) = serve_responses(vec![
        (
            200,
            reply(
                &[json!({"type":"text","text":"Reading."}), read_call],
                json!({"input_tokens":3,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":1_200}),
                json!({"output_tokens":30}),
                "tool_use",
            ),
        ),
        (200, text_reply()),
    ])
    .await;
    let config = catalog_model(&root, "anthropic-api", "claude-opus-5-5", endpoint);
    let prompt = message(Role::User, vec![text("Read the notes")]);
    let first_output = complete(&config, &root, conversation(vec![prompt.clone()])).await;
    let first = captured.recv().await.unwrap().1;
    assert_eq!(
        paths(&first),
        ["system/0", "messages/0/0", "messages/1/0"],
        "tools and system, the stable prefix through the user context, and the tail"
    );
    assert!(
        first["tools"]
            .as_array()
            .unwrap()
            .iter()
            .all(|tool| tool.get("cache_control").is_none())
    );

    let result = message(
        Role::User,
        vec![Content::ToolResult {
            tool_use_id: "toolu_1".into(),
            content: "Delivery is Friday.".into(),
            is_error: false,
        }],
    );
    complete(
        &config,
        &root,
        conversation(vec![
            prompt,
            message(Role::Assistant, first_output.content),
            result,
        ]),
    )
    .await;
    let second = captured.recv().await.unwrap().1;
    assert_eq!(
        paths(&second),
        ["system/0", "messages/0/0", "messages/1/0", "messages/3/0"],
        "the tail moves to the tool result; the previous tail stays marked to read it"
    );
    for part in ["model", "system", "tools", "thinking", "output_config"] {
        assert_eq!(
            without_breakpoints(first[part].clone()),
            without_breakpoints(second[part].clone()),
            "{part}"
        );
    }
    let first_messages = without_breakpoints(first["messages"].clone());
    let second_messages = without_breakpoints(second["messages"].clone());
    let first_messages = first_messages.as_array().unwrap();
    assert_eq!(
        first_messages[..],
        second_messages.as_array().unwrap()[..first_messages.len()],
        "the first request's messages lead the second's byte for byte"
    );
}

#[tokio::test]
async fn thinking_and_one_shot_requests_keep_breakpoints_off_ineligible_blocks() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let paused = reply(
        &[
            json!({"type":"text","text":"Partial answer"}),
            json!({"type":"thinking","thinking":"","signature":"signed"}),
        ],
        json!({"input_tokens":1,"output_tokens":1}),
        json!({"output_tokens":2}),
        "end_turn",
    );
    let (endpoint, mut captured) = serve_responses(vec![
        (200, paused),
        (200, text_reply()),
        (200, text_reply()),
    ])
    .await;
    let config = catalog_model(&root, "anthropic-api", "claude-opus-5-5", endpoint);
    let prompt = message(Role::User, vec![text("Continue")]);
    let output = complete(&config, &root, conversation(vec![prompt.clone()])).await;
    captured.recv().await.unwrap();
    let mut continuation = conversation(vec![prompt, message(Role::Assistant, output.content)]);
    continuation.history_sources[3] = output.private_source;
    complete(&config, &root, continuation).await;
    let continued = captured.recv().await.unwrap().1;
    assert_eq!(continued["messages"][2]["content"][1]["type"], "thinking");
    assert_eq!(
        paths(&continued),
        ["system/0", "messages/0/0", "messages/1/0", "messages/2/0"],
        "the thinking block passes its breakpoint to the text before it"
    );

    let mut one_shot = conversation(vec![message(Role::User, vec![text("Title this")])]);
    one_shot.tools.clear();
    complete(&config, &root, one_shot).await;
    let title = captured.recv().await.unwrap().1;
    assert_eq!(paths(&title), ["system/0"]);
}

#[tokio::test]
async fn anthropic_compatible_endpoints_receive_no_breakpoints() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (endpoint, mut captured) = serve_responses(vec![(200, text_reply())]).await;
    let config = catalog_model(&root, "minimax-global-api", "MiniMax-M3", endpoint);
    complete(
        &config,
        &root,
        conversation(vec![message(Role::User, vec![text("Hello")])]),
    )
    .await;
    let wire = captured.recv().await.unwrap().1;
    assert!(breakpoints(&wire).is_empty(), "{wire}");
}

/// Anthropic reports cached input apart from `input_tokens` in `message_start`, and repeats the
/// cumulative counters in `message_delta`; a delta with the output count alone keeps the rest.
#[tokio::test]
async fn cache_reads_and_writes_reach_the_usage_from_start_and_delta() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let cache_creation = json!({"ephemeral_5m_input_tokens":60,"ephemeral_1h_input_tokens":0});
    let (endpoint, _captured) = serve_responses(vec![
        (
            200,
            reply(
                &[json!({"type":"text","text":"Done."})],
                json!({"input_tokens":4,"output_tokens":1,"cache_read_input_tokens":1_200,"cache_creation_input_tokens":60,"cache_creation":cache_creation}),
                json!({"input_tokens":4,"output_tokens":25,"cache_read_input_tokens":1_200,"cache_creation_input_tokens":60}),
                "end_turn",
            ),
        ),
        (
            200,
            reply(
                &[json!({"type":"text","text":"Done."})],
                json!({"input_tokens":4,"output_tokens":1,"cache_read_input_tokens":1_260,"cache_creation_input_tokens":30}),
                json!({"output_tokens":9}),
                "end_turn",
            ),
        ),
    ])
    .await;
    let config = catalog_model(&root, "anthropic-api", "claude-opus-5-5", endpoint);
    let request = || conversation(vec![message(Role::User, vec![text("Hello")])]);
    assert_eq!(
        complete(&config, &root, request()).await.usage,
        Usage {
            input_tokens: Some(4),
            output_tokens: Some(25),
            cache_read_input_tokens: Some(1_200),
            cache_write_input_tokens: Some(60),
        }
    );
    assert_eq!(
        complete(&config, &root, request()).await.usage,
        Usage {
            input_tokens: Some(4),
            output_tokens: Some(9),
            cache_read_input_tokens: Some(1_260),
            cache_write_input_tokens: Some(30),
        }
    );
}
