use serde_json::{Value, json};
use std::path::Path;
use tickloop_harness_model::{
    LocalModelClients, ModelClients, ModelOutput, ModelRequest, cards::ModelCatalog,
    credentials::save_api_key,
};
use tickloop_harness_protocol::{
    Content, Message, ModelConfig, NativeToolSearch, PrivateHistorySource, Protocol, Role,
    ToolDefinition,
};
use tickloop_harness_test_support::model_http_support::serve_responses;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const WEATHER: &str = "mcp__weather__get_weather";
const INVOICES: &str = "mcp__billing__list_invoices";

#[test]
fn only_verified_official_models_offer_native_tool_search() {
    let catalog = ModelCatalog::defaults();
    catalog.validate().unwrap();
    for provider in &catalog.providers {
        for model in &provider.models {
            let expected = match provider.id.as_str() {
                "anthropic-api" => Some(NativeToolSearch::Anthropic),
                "openai-api" => Some(NativeToolSearch::OpenAi),
                "openai-codex" if model.id == "gpt-6.1-sol" => Some(NativeToolSearch::OpenAi),
                _ => None,
            };
            assert_eq!(
                model.native_tool_search.flatten(),
                expected,
                "{}/{}",
                provider.id,
                model.id
            );
        }
    }
}

fn tool(name: &str, description: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: description.into(),
        input_schema: json!({"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}),
    }
}

fn catalog_model(root: &Path, provider: &str, model: &str, endpoint: String) -> ModelConfig {
    let mut config = ModelCatalog::defaults().get(provider, model).unwrap();
    if !config.api_key_env.is_empty() {
        save_api_key(root, &config.api_key_env, "test-key").unwrap();
    }
    config.base_url = endpoint;
    config
}

fn user(text: &str) -> Message {
    Message {
        role: Role::User,
        content: vec![Content::Text { text: text.into() }],
    }
}

fn request(
    messages: Vec<(Message, Option<PrivateHistorySource>)>,
    deferred: &[&str],
) -> ModelRequest {
    let (messages, history_sources) = messages.into_iter().unzip();
    ModelRequest {
        history_sources,
        messages,
        tools: vec![tool("Read", "Read a file")],
        deferred_tools: deferred
            .iter()
            .map(|name| tool(name, "Deferred fixture"))
            .collect(),
        structured_output: None,
        temperature: None,
        reasoning_effort: None,
    }
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

fn messages_event(event_type: &str, mut data: Value) -> String {
    data["type"] = json!(event_type);
    format!("event: {event_type}\ndata: {data}\n\n")
}

fn messages_text() -> String {
    [
        messages_event(
            "message_start",
            json!({"message":{"role":"assistant","content":[]}}),
        ),
        messages_event(
            "content_block_start",
            json!({"index":0,"content_block":{"type":"text","text":"Sunny"}}),
        ),
        messages_event("content_block_stop", json!({"index":0})),
        messages_event("message_delta", json!({"delta":{"stop_reason":"end_turn"}})),
        messages_event("message_stop", json!({})),
    ]
    .concat()
}

/// The stream `claude-sonnet-5` sent on 2026-10-27 for a BM25 search followed by a direct call.
fn messages_search_then_call() -> String {
    [
        messages_event("message_start", json!({"message":{"role":"assistant","content":[]}})),
        messages_event("content_block_start", json!({"index":0,"content_block":{"type":"server_tool_use","id":"srvtoolu_1","name":"tool_search_tool_bm25","input":{}}})),
        messages_event("content_block_delta", json!({"index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\": \"wea"}})),
        messages_event("content_block_delta", json!({"index":0,"delta":{"type":"input_json_delta","partial_json":"ther forecast\"}"}})),
        messages_event("content_block_stop", json!({"index":0})),
        messages_event("content_block_start", json!({"index":1,"content_block":{"type":"tool_search_tool_result","tool_use_id":"srvtoolu_1","content":{"type":"tool_search_tool_search_result","tool_references":[{"type":"tool_reference","tool_name":WEATHER}]}}})),
        messages_event("content_block_stop", json!({"index":1})),
        messages_event("content_block_start", json!({"index":2,"content_block":{"type":"tool_use","id":"toolu_1","name":WEATHER,"input":{},"caller":{"type":"direct"}}})),
        messages_event("content_block_delta", json!({"index":2,"delta":{"type":"input_json_delta","partial_json":"{\"city\":\"Paris\"}"}})),
        messages_event("content_block_stop", json!({"index":2})),
        messages_event("message_delta", json!({"delta":{"stop_reason":"tool_use"}})),
        messages_event("message_stop", json!({})),
    ]
    .concat()
}

fn weather_result() -> Message {
    Message {
        role: Role::User,
        content: vec![Content::ToolResult {
            tool_use_id: "toolu_1".into(),
            content: "Sunny, 21C".into(),
            is_error: false,
        }],
    }
}

#[tokio::test]
async fn anthropic_defers_tools_behind_bm25_search_and_replays_the_search_for_the_same_model() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (endpoint, mut captured) = serve_responses(vec![
        (200, messages_search_then_call()),
        (200, messages_text()),
        (200, messages_text()),
        (200, messages_text()),
    ])
    .await;
    let config = catalog_model(&root, "anthropic-api", "claude-sonnet-5", endpoint);
    let first = complete(
        &config,
        &root,
        request(vec![(user("Weather?"), None)], &[WEATHER, INVOICES]),
    )
    .await;
    let wire = captured.recv().await.unwrap().1;
    assert_eq!(
        wire["tools"],
        json!([
            {"name":"Read","description":"Read a file","input_schema":tool("Read", "").input_schema},
            {"type":"tool_search_tool_bm25_20251119","name":"tool_search_tool_bm25"},
            {"name":WEATHER,"description":"Deferred fixture","input_schema":tool("Read", "").input_schema,"defer_loading":true},
            {"name":INVOICES,"description":"Deferred fixture","input_schema":tool("Read", "").input_schema,"defer_loading":true},
        ])
    );
    let search = json!({"type":"server_tool_use","id":"srvtoolu_1","name":"tool_search_tool_bm25","input":{"query":"weather forecast"}});
    let found = json!({"type":"tool_search_tool_result","tool_use_id":"srvtoolu_1","content":{"type":"tool_search_tool_search_result","tool_references":[{"type":"tool_reference","tool_name":WEATHER}]}});
    assert_eq!(first.stop_reason, "tool_use");
    assert_eq!(
        first.content,
        vec![
            Content::ProtocolData {
                protocol: Protocol::Messages,
                value: search.clone()
            },
            Content::ProtocolData {
                protocol: Protocol::Messages,
                value: found.clone()
            },
            Content::ToolUse {
                id: "toolu_1".into(),
                name: WEATHER.into(),
                input: json!({"city":"Paris"})
            },
        ]
    );
    let history = |deferred: &[&str]| {
        request(
            vec![
                (user("Weather?"), None),
                (
                    Message {
                        role: Role::Assistant,
                        content: first.content.clone(),
                    },
                    first.private_source.clone(),
                ),
                (weather_result(), None),
            ],
            deferred,
        )
    };

    complete(&config, &root, history(&[WEATHER, INVOICES])).await;
    let replay = captured.recv().await.unwrap().1;
    assert_eq!(replay["messages"][1]["content"][0], search);
    assert_eq!(replay["messages"][1]["content"][1], found);

    complete(&config, &root, history(&[INVOICES])).await;
    let withdrawn = captured.recv().await.unwrap().1;
    assert_eq!(withdrawn["messages"][1]["content"][0], search);
    assert_eq!(
        withdrawn["messages"][1]["content"][1]["content"]["tool_references"],
        json!([])
    );

    complete(&config, &root, history(&[])).await;
    let without_search = captured.recv().await.unwrap().1;
    assert_eq!(
        without_search["tools"],
        json!([{"name":"Read","description":"Read a file","input_schema":tool("Read", "").input_schema}])
    );
    let replayed = &without_search["messages"][1]["content"];
    assert_eq!(replayed[0]["type"], "text");
    assert!(
        replayed[0]["text"]
            .as_str()
            .unwrap()
            .contains("weather forecast")
    );
    assert_eq!(replayed[1]["type"], "text");
    assert!(replayed[1]["text"].as_str().unwrap().contains(WEATHER));
    assert_eq!(replayed[2]["type"], "tool_use");
}

fn chat_text() -> String {
    [
        format!(
            "data: {}\n\n",
            json!({"id":"chat-1","choices":[{"index":0,"delta":{"role":"assistant","content":"Sunny"},"finish_reason":null}]})
        ),
        format!(
            "data: {}\n\n",
            json!({"id":"chat-1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})
        ),
        "data: [DONE]\n\n".into(),
    ]
    .concat()
}

#[tokio::test]
async fn switching_models_turns_foreign_search_records_into_text_and_restores_them_on_return() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (anthropic_endpoint, mut anthropic_wire) = serve_responses(vec![
        (200, messages_search_then_call()),
        (200, messages_text()),
    ])
    .await;
    let (openai_endpoint, mut openai_wire) =
        serve_responses(vec![(200, responses_search_then_call())]).await;
    let (chat_endpoint, mut chat_wire) = serve_responses(vec![(200, chat_text())]).await;
    let anthropic = catalog_model(
        &root,
        "anthropic-api",
        "claude-sonnet-5",
        anthropic_endpoint,
    );
    let openai = catalog_model(
        &root,
        "openai-api",
        "gpt-5.6-terra",
        openai_endpoint.trim_end_matches("/anthropic").to_owned() + "/v1",
    );
    let fold = catalog_model(&root, "deepseek-api", "deepseek-flash", chat_endpoint);
    let mut conversation = vec![(user("Weather?"), None)];
    let first = complete(&anthropic, &root, request(conversation.clone(), &[WEATHER])).await;
    anthropic_wire.recv().await.unwrap();
    conversation.push((
        Message {
            role: Role::Assistant,
            content: first.content.clone(),
        },
        first.private_source.clone(),
    ));
    conversation.push((weather_result(), None));
    conversation.push((user("And tomorrow?"), None));

    let second = complete(&openai, &root, request(conversation.clone(), &[WEATHER])).await;
    let at_openai = openai_wire.recv().await.unwrap().1;
    let input = at_openai["input"].as_array().unwrap();
    assert_eq!(input[1]["role"], "assistant");
    assert!(
        input[1]["content"]
            .as_str()
            .unwrap()
            .contains("weather forecast")
    );
    assert!(input[2]["content"].as_str().unwrap().contains(WEATHER));
    assert_eq!(
        input[3],
        json!({"type":"function_call","call_id":"toolu_1","name":WEATHER,"arguments":"{\"city\":\"Paris\"}"})
    );
    assert!(!at_openai.to_string().contains("server_tool_use"));
    conversation.push((
        Message {
            role: Role::Assistant,
            content: second.content.clone(),
        },
        second.private_source.clone(),
    ));
    conversation.push((function_result(), None));

    complete(&fold, &root, request(conversation.clone(), &[])).await;
    let at_fold = chat_wire.recv().await.unwrap().1.to_string();
    for record in [
        "server_tool_use",
        "tool_search_call",
        "tool_search_output",
        "namespace",
    ] {
        assert!(!at_fold.contains(record), "{record} reached a Fold model");
    }
    assert!(at_fold.contains("Searched tools for"));

    complete(&anthropic, &root, request(conversation, &[WEATHER])).await;
    let back = anthropic_wire.recv().await.unwrap().1;
    let messages = back["messages"].as_array().unwrap();
    assert_eq!(messages[1]["content"][0]["type"], "server_tool_use");
    assert_eq!(messages[1]["content"][1]["type"], "tool_search_tool_result");
    let foreign = &messages[4]["content"];
    assert_eq!(foreign[0]["type"], "text");
    assert!(foreign[0]["text"].as_str().unwrap().contains("paths"));
    assert_eq!(foreign[2]["type"], "tool_use");
    assert!(!back.to_string().contains("tool_search_output"));
}

#[tokio::test]
async fn models_without_native_search_refuse_deferred_tools_before_sending() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (endpoint, mut wire) = serve_responses(vec![(200, chat_text())]).await;
    for (provider, model) in [
        ("deepseek-api", "deepseek-flash"),
        ("github-copilot", "claude-sonnet-5.5"),
    ] {
        let config = catalog_model(&root, provider, model, endpoint.clone());
        let client = LocalModelClients::new(Some(root.clone()))
            .client(&config)
            .unwrap();
        let (events, _) = mpsc::unbounded_channel();
        let error = client
            .complete(
                request(vec![(user("Weather?"), None)], &[WEATHER]),
                events,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, tickloop_harness_model::ModelError::Configuration(_)),
            "{provider}: {error}"
        );
    }
    assert!(wire.try_recv().is_err());
}

fn responses_stream(output: &[(Value, Value)]) -> String {
    let mut frames = Vec::new();
    let mut sequence = 0;
    let mut event = |kind: &str, mut body: Value| {
        body["type"] = json!(kind);
        body["sequence_number"] = json!(sequence);
        sequence += 1;
        frames.push(format!("event: {kind}\ndata: {body}\n\n"));
    };
    event(
        "response.created",
        json!({"response":{"id":"resp-1","status":"in_progress"}}),
    );
    for (index, (added, done)) in output.iter().enumerate() {
        event(
            "response.output_item.added",
            json!({"output_index":index,"item":added}),
        );
        event(
            "response.output_item.done",
            json!({"output_index":index,"item":done}),
        );
    }
    let completed: Vec<_> = output.iter().map(|(_, done)| done.clone()).collect();
    event(
        "response.completed",
        json!({"response":{"id":"resp-1","status":"completed","output":completed,"usage":{"input_tokens":10,"output_tokens":5}}}),
    );
    frames.concat()
}

fn responses_text() -> String {
    let message = json!({"type":"message","id":"msg-1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"Sunny"}]});
    responses_stream(&[(
        json!({"type":"message","id":"msg-1","role":"assistant","status":"in_progress","content":[]}),
        message,
    )])
}

fn search_call() -> Value {
    json!({"id":"tsc_1","type":"tool_search_call","status":"completed","arguments":{"paths":[WEATHER]},"call_id":null,"execution":"server"})
}

fn search_output() -> Value {
    json!({"id":"tso_1","type":"tool_search_output","status":"completed","call_id":null,"execution":"server","tools":[{"type":"function","defer_loading":true,"description":"Deferred fixture","name":WEATHER,"output_schema":null,"parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"],"additionalProperties":false},"strict":true}]})
}

/// The items `gpt-5.6-terra` sent on 2026-10-27 for a hosted search followed by a direct call.
fn responses_search_then_call() -> String {
    let in_progress = |mut item: Value| {
        item["status"] = json!("in_progress");
        item
    };
    let mut started_search = in_progress(search_call());
    started_search["arguments"] = json!({});
    let call = json!({"id":"fc_1","type":"function_call","status":"completed","arguments":"{\"city\":\"Paris\"}","call_id":"call_1","name":WEATHER,"namespace":WEATHER});
    let mut started_call = in_progress(call.clone());
    started_call["arguments"] = json!("");
    responses_stream(&[
        (started_search, search_call()),
        (in_progress(search_output()), search_output()),
        (started_call, call),
    ])
}

fn function_result() -> Message {
    Message {
        role: Role::User,
        content: vec![Content::ToolResult {
            tool_use_id: "call_1".into(),
            content: "Sunny, 21C".into(),
            is_error: false,
        }],
    }
}

#[tokio::test]
async fn openai_defers_functions_behind_hosted_search_and_round_trips_their_namespace() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (endpoint, mut captured) = serve_responses(vec![
        (200, responses_search_then_call()),
        (200, responses_text()),
        (200, responses_text()),
    ])
    .await;
    let config = catalog_model(
        &root,
        "openai-api",
        "gpt-5.6-terra",
        endpoint.trim_end_matches("/anthropic").to_owned() + "/v1",
    );
    let first = complete(
        &config,
        &root,
        request(vec![(user("Weather?"), None)], &[WEATHER, INVOICES]),
    )
    .await;
    let wire = captured.recv().await.unwrap().1;
    let parameters = tool("Read", "").input_schema;
    assert_eq!(
        wire["tools"],
        json!([
            {"type":"function","name":"Read","description":"Read a file","parameters":parameters},
            {"type":"tool_search"},
            {"type":"function","name":WEATHER,"description":"Deferred fixture","parameters":parameters,"defer_loading":true},
            {"type":"function","name":INVOICES,"description":"Deferred fixture","parameters":parameters,"defer_loading":true},
        ])
    );
    assert_eq!(first.stop_reason, "tool_use");
    assert_eq!(
        first.content.last(),
        Some(&Content::ToolUse {
            id: "call_1".into(),
            name: WEATHER.into(),
            input: json!({"city":"Paris"})
        })
    );
    let history = |deferred: &[&str]| {
        request(
            vec![
                (user("Weather?"), None),
                (
                    Message {
                        role: Role::Assistant,
                        content: first.content.clone(),
                    },
                    first.private_source.clone(),
                ),
                (function_result(), None),
            ],
            deferred,
        )
    };

    complete(&config, &root, history(&[WEATHER, INVOICES])).await;
    let replay = captured.recv().await.unwrap().1;
    assert_eq!(
        replay["input"],
        json!([
            {"type":"message","role":"user","content":"Weather?"},
            search_call(),
            search_output(),
            {"type":"function_call","call_id":"call_1","name":WEATHER,"arguments":"{\"city\":\"Paris\"}","namespace":WEATHER},
            {"type":"function_call_output","call_id":"call_1","output":"Sunny, 21C"},
        ])
    );

    complete(&config, &root, history(&[])).await;
    let without_search = captured.recv().await.unwrap().1;
    assert_eq!(
        without_search["tools"],
        json!([{"type":"function","name":"Read","description":"Read a file","parameters":parameters}])
    );
    let input = without_search["input"].as_array().unwrap();
    assert_eq!(input.len(), 5);
    assert_eq!(input[1]["role"], "assistant");
    assert!(input[1]["content"].as_str().unwrap().contains("paths"));
    assert!(input[2]["content"].as_str().unwrap().contains(WEATHER));
    assert_eq!(
        input[3],
        json!({"type":"function_call","call_id":"call_1","name":WEATHER,"arguments":"{\"city\":\"Paris\"}"})
    );
}

#[tokio::test]
async fn codex_native_search_replays_namespace_and_projects_history_across_models() {
    use tickloop_harness_model::codex_test_support::{
        LoopbackCodexClient, serve_scripted_responses, state_with_credentials,
    };
    let (_directory, root) = state_with_credentials();
    let config = ModelCatalog::defaults()
        .get("openai-codex", "gpt-6.1-sol")
        .unwrap();
    let (endpoint, captured) = serve_scripted_responses(vec![
        (200, codex_search_then_call()),
        (200, responses_text()),
        (200, responses_text()),
        (200, responses_text()),
    ])
    .await;
    let client =
        LoopbackCodexClient::new(config.clone(), &root, endpoint.as_str()).into_fixture_client();
    let run = |request| {
        let (events, _) = mpsc::unbounded_channel();
        client.complete(request, events, CancellationToken::new())
    };
    let first = run(request(
        vec![(user("Weather in Paris?"), None)],
        &[WEATHER, INVOICES],
    ))
    .await
    .unwrap();
    let (call_id, name, arguments) = first
        .content
        .iter()
        .find_map(|content| match content {
            Content::ToolUse { id, name, input } => Some((id.clone(), name, input)),
            _ => None,
        })
        .unwrap();
    assert_eq!(name, WEATHER);
    assert_eq!(arguments, &json!({"city":"Paris"}));
    let conversation = vec![
        (user("Weather in Paris?"), None),
        (
            Message {
                role: Role::Assistant,
                content: first.content.clone(),
            },
            first.private_source.clone(),
        ),
        (
            Message {
                role: Role::User,
                content: vec![Content::ToolResult {
                    tool_use_id: call_id,
                    content: "Sunny, 21C".into(),
                    is_error: false,
                }],
            },
            None,
        ),
    ];
    let original = conversation.clone();
    run(request(conversation.clone(), &[WEATHER, INVOICES]))
        .await
        .unwrap();
    run(request(conversation.clone(), &[])).await.unwrap();
    let (fold_endpoint, mut fold_wire) = serve_responses(vec![(200, chat_text())]).await;
    let fold = catalog_model(&root, "deepseek-api", "deepseek-flash", fold_endpoint);
    complete(&fold, &root, request(conversation.clone(), &[])).await;
    let foreign = fold_wire.recv().await.unwrap().1.to_string();
    assert!(foreign.contains("Searched tools for"));
    assert!(!foreign.contains("tool_search_call") && !foreign.contains("namespace"));
    run(request(conversation.clone(), &[WEATHER, INVOICES]))
        .await
        .unwrap();
    assert_eq!(conversation, original);
    let wires = captured.await.unwrap();
    assert_eq!(wires[0]["tools"][1], json!({"type":"tool_search"}));
    assert_eq!(wires[0]["tools"][2]["defer_loading"], true);
    assert_eq!(wires[0]["tools"][3]["defer_loading"], true);
    assert_eq!(wires[0]["store"], false);
    let input = wires[1]["input"].as_array().unwrap();
    assert_eq!(input[1]["type"], "tool_search_call");
    assert_eq!(input[2]["type"], "tool_search_output");
    assert_eq!(input[3]["namespace"], WEATHER);
    assert_eq!(input[3]["call_id"], input[4]["call_id"]);
    assert_eq!(input[3]["id"], "fc_probe");
    assert_eq!(wires[1]["input"], wires[3]["input"]);
    assert_eq!(wires[0]["tools"], wires[1]["tools"]);
    let without_search = wires[2].to_string();
    assert!(without_search.contains("Searched tools for"));
    assert!(!without_search.contains("tool_search_call") && !without_search.contains("namespace"));
}

// Terminal items observed from gpt-6.1-sol's Codex SSE response on 2026-10-27;
// identifiers are replaced, and framing is reconstructed from those items.
fn codex_search_then_call() -> String {
    let output: Vec<Value> =
        serde_json::from_str(include_str!("fixtures/codex_native_search.json")).unwrap();
    responses_stream(
        &output
            .into_iter()
            .map(|done| {
                let mut added = done.clone();
                added["status"] = json!("in_progress");
                if added["type"] == "function_call" {
                    added["arguments"] = json!("");
                }
                if added["type"] == "tool_search_call" {
                    added["arguments"] = json!({});
                }
                (added, done)
            })
            .collect::<Vec<_>>(),
    )
}

#[tokio::test]
async fn codex_native_search_websocket_continuation_keeps_loaded_tools() {
    use futures_util::StreamExt;
    use tickloop_harness_model::codex_test_support::{
        LoopbackCodexClient, send_ws_stream, state_with_credentials,
    };
    let (_directory, root) = state_with_credentials();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/codex/responses", listener.local_addr().unwrap());
    let (send, receive) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let mut frames = Vec::new();
        for reply in [codex_search_then_call(), responses_text()] {
            let frame = socket.next().await.unwrap().unwrap().into_text().unwrap();
            frames.push(serde_json::from_str::<Value>(&frame).unwrap());
            send_ws_stream(&mut socket, &reply).await;
        }
        send.send(frames).unwrap();
    });
    let config = ModelCatalog::defaults()
        .get("openai-codex", "gpt-6.1-sol")
        .unwrap();
    let client = LoopbackCodexClient::new(config, &root, &endpoint)
        .with_websocket()
        .into_fixture_client();
    let chat = uuid::Uuid::new_v4().into();
    let (events, _) = mpsc::unbounded_channel();
    let mut conversation = vec![(user("Weather in Paris?"), None)];
    let first = client
        .complete_for_chat(
            chat,
            request(conversation.clone(), &[WEATHER]),
            events.clone(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let call_id = first
        .content
        .iter()
        .find_map(|content| match content {
            Content::ToolUse { id, .. } => Some(id.clone()),
            _ => None,
        })
        .unwrap();
    conversation.push((
        Message {
            role: Role::Assistant,
            content: first.content,
        },
        first.private_source,
    ));
    conversation.push((
        Message {
            role: Role::User,
            content: vec![Content::ToolResult {
                tool_use_id: call_id,
                content: "Sunny".into(),
                is_error: false,
            }],
        },
        None,
    ));
    client
        .complete_for_chat(
            chat,
            request(conversation, &[WEATHER]),
            events,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    client.close_chat(chat).await;
    let frames = receive.await.unwrap();
    assert_eq!(frames[1]["previous_response_id"], "resp-1");
    assert_eq!(frames[1]["input"].as_array().unwrap().len(), 1);
    assert_eq!(frames[1]["input"][0]["type"], "function_call_output");
    assert_eq!(frames[1]["input"][0]["call_id"], "call_probe");
    assert_eq!(frames[0]["tools"], frames[1]["tools"]);
}

/// The tools a request lists before the native search tool: the part of the prompt cache prefix
/// that a change of the deferred tools must leave alone.
fn tools_before_search(wire: &Value) -> Vec<Value> {
    let tools = wire["tools"].as_array().unwrap();
    let search = tools
        .iter()
        .position(|tool| {
            matches!(
                tool["type"].as_str(),
                Some("tool_search_tool_bm25_20251119" | "tool_search")
            )
        })
        .expect("the request offers native search");
    tools[..search].to_vec()
}

fn listed_and_deferred(deferred: &[&str]) -> ModelRequest {
    let mut request = request(vec![(user("Weather?"), None)], deferred);
    request.tools.push(tool("mcp__alpha__lookup", "Look up"));
    request
}

#[tokio::test]
async fn a_change_of_deferred_tools_leaves_the_tools_before_native_search_byte_identical() {
    use tickloop_harness_model::codex_test_support::{
        LoopbackCodexClient, serve_scripted_responses, state_with_credentials,
    };
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let (messages_endpoint, mut messages_wire) =
        serve_responses(vec![(200, messages_text()), (200, messages_text())]).await;
    let anthropic = catalog_model(&root, "anthropic-api", "claude-sonnet-5", messages_endpoint);
    let (responses_endpoint, mut responses_wire) =
        serve_responses(vec![(200, responses_text()), (200, responses_text())]).await;
    let openai = catalog_model(
        &root,
        "openai-api",
        "gpt-5.6-terra",
        responses_endpoint.trim_end_matches("/anthropic").to_owned() + "/v1",
    );
    let mut wires = Vec::new();
    for (config, captured) in [
        (&anthropic, &mut messages_wire),
        (&openai, &mut responses_wire),
    ] {
        complete(config, &root, listed_and_deferred(&[WEATHER])).await;
        complete(config, &root, listed_and_deferred(&[INVOICES, WEATHER])).await;
        wires.push([
            captured.recv().await.unwrap().1,
            captured.recv().await.unwrap().1,
        ]);
    }
    let (_codex_directory, codex_root) = state_with_credentials();
    let codex = ModelCatalog::defaults()
        .get("openai-codex", "gpt-6.1-sol")
        .unwrap();
    let (endpoint, captured) =
        serve_scripted_responses(vec![(200, responses_text()), (200, responses_text())]).await;
    let client =
        LoopbackCodexClient::new(codex, &codex_root, endpoint.as_str()).into_fixture_client();
    for deferred in [&[WEATHER][..], &[INVOICES, WEATHER][..]] {
        let (events, _) = mpsc::unbounded_channel();
        client
            .complete(
                listed_and_deferred(deferred),
                events,
                CancellationToken::new(),
            )
            .await
            .unwrap();
    }
    let codex_wires = captured.await.unwrap();
    wires.push([codex_wires[0].clone(), codex_wires[1].clone()]);
    for [first, second] in &wires {
        let listed = tools_before_search(first);
        assert_eq!(listed.len(), 2, "{first}");
        assert_eq!(
            serde_json::to_vec(&listed).unwrap(),
            serde_json::to_vec(&tools_before_search(second)).unwrap()
        );
        assert_ne!(first["tools"], second["tools"]);
    }
}
