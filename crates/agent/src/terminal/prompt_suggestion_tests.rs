use super::prompt_suggestion::{SuggestionRequest, generate};
use super::test_support;
use serde_json::{Value, json};
use std::sync::Arc;
use tickloop_agent::model::HttpModelClient;
use tickloop_agent::model::{Content, Message, Role};
use tickloop_agent::runtime::{Agent, ChatSnapshot, EventKind};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::model::model_http_support as http;

fn request() -> SuggestionRequest {
    SuggestionRequest {
        chat: Uuid::from_u128(8100),
        id: Uuid::new_v4(),
        cancel: CancellationToken::new(),
    }
}

fn conversation() -> Vec<Message> {
    vec![
        Message {
            role: Role::User,
            content: test_support::text("修好了检查一下"),
        },
        Message {
            role: Role::Assistant,
            content: test_support::text("All fixture tests passed."),
        },
    ]
}

async fn snapshot(
    model: tickloop_agent::model::ModelConfig,
    messages: Vec<Message>,
) -> ChatSnapshot {
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(model, Arc::new(test_support::ScriptedModel::new(vec![])));
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut snapshot = agent.snapshot(chat).await.unwrap();
    snapshot.messages = messages;
    agent.shutdown().await;
    snapshot
}

fn response(content: Value, stop: &str) -> String {
    let event = |kind: &str, mut value: Value| {
        value["type"] = kind.into();
        format!("event: {kind}\ndata: {value}\n\n")
    };
    [
        event(
            "message_start",
            json!({"message":{"role":"assistant","content":[]}}),
        ),
        event(
            "content_block_start",
            json!({"index":0,"content_block":content}),
        ),
        event("content_block_stop", json!({"index":0})),
        event("message_delta", json!({"delta":{"stop_reason":stop}})),
        event("message_stop", json!({})),
    ]
    .concat()
}

fn responses_reasoning_reply(summary: &str, streamed_prefix: Option<&str>) -> String {
    let reasoning = json!({"type":"reasoning","id":"rs-budget","summary":[{"type":"summary_text","text":summary}],"encrypted_content":"opaque-fixture"});
    let message = json!({"type":"message","id":"msg-budget","role":"assistant","status":"completed","content":[{"type":"output_text","text":"run the tests","annotations":[]}]});
    let mut sequence = 0;
    let mut frames = Vec::new();
    let mut event = |kind: &str, mut body: Value| {
        body["type"] = kind.into();
        body["sequence_number"] = json!(sequence);
        sequence += 1;
        frames.push(format!("event: {kind}\ndata: {body}\n\n"));
    };
    event(
        "response.created",
        json!({"response":{"id":"completion-budget","status":"in_progress"}}),
    );
    let mut initial = reasoning.clone();
    if streamed_prefix.is_some() {
        initial["summary"] = json!([]);
    }
    event(
        "response.output_item.added",
        json!({"output_index":0,"item":initial}),
    );
    if let Some(prefix) = streamed_prefix {
        event(
            "response.reasoning_summary_text.delta",
            json!({"output_index":0,"item_id":"rs-budget","summary_index":0,"delta":prefix}),
        );
    }
    event(
        "response.output_item.done",
        json!({"output_index":0,"item":reasoning}),
    );
    let mut initial_message = message.clone();
    initial_message["content"] = json!([]);
    event(
        "response.output_item.added",
        json!({"output_index":1,"item":initial_message}),
    );
    event(
        "response.output_item.done",
        json!({"output_index":1,"item":message}),
    );
    event(
        "response.completed",
        json!({"response":{"id":"completion-budget","status":"completed","output":[reasoning,message],"usage":{"input_tokens":20,"output_tokens":512}}}),
    );
    frames.concat()
}

#[tokio::test]
async fn prompt_suggestion_responses_completion_reasoning_is_bounded_without_counting_prefix_twice()
{
    for (bytes, prefix_bytes, accepted) in [
        (2_048, None, false),
        (700, Some(700), true),
        (700, Some(400), true),
    ] {
        let summary = "x".repeat(bytes);
        let prefix = prefix_bytes.map(|count| "x".repeat(count));
        let (url, received) =
            http::serve_response(200, responses_reasoning_reply(&summary, prefix.as_deref())).await;
        let mut model = test_support::config();
        model.base_url = url;
        model.protocol = tickloop_agent::model::Protocol::Responses;
        let context = snapshot(model, conversation()).await;
        let request = request();
        let output = generate(&request, &context, None).await;
        assert_eq!(
            output.text.is_some(),
            accepted,
            "bytes={bytes}, prefix={prefix_bytes:?}"
        );
        assert_eq!(request.cancel.is_cancelled(), !accepted);
        let (_, body) = received.await.unwrap();
        assert_eq!(body["max_output_tokens"], 1_024);
    }
}

#[tokio::test]
async fn prompt_suggestion_completion_backstop_counts_protocol_readable_reasoning() {
    let client = test_support::ScriptedModel::new(vec![vec![
        Content::ProtocolData {
            protocol: tickloop_agent::model::Protocol::Responses,
            value: json!({"type":"reasoning","summary":[{"type":"summary_text","text":"x".repeat(2_048)}],"encrypted_content":"opaque-fixture"}),
        },
        Content::Text {
            text: "run the tests".into(),
        },
    ]]);
    let cancel = CancellationToken::new();
    let result =
        tickloop_agent::model::GenerationBudget::new(1_024, std::time::Duration::from_secs(10))
            .complete(
                &client,
                Uuid::new_v4(),
                tickloop_agent::model::ModelRequest {
                    history_sources: vec![],
                    messages: conversation(),
                    tools: vec![],
                    structured_output: None,
                    temperature: None,
                    reasoning_effort: None,
                },
                cancel.clone(),
            )
            .await;
    assert!(matches!(
        result,
        Err(tickloop_agent::model::ModelError::ResponseRejected(
            tickloop_agent::model::ModelResponseRejection::OutputLimit
        ))
    ));
    assert!(cancel.is_cancelled());
}

#[tokio::test]
async fn prompt_suggestion_compaction_then_provider_switch_never_resends_sensitive_original() {
    const ORIGINAL: &str = "sensitive original fixture credential excluded from public summary";
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(
        test_support::config(),
        Arc::new(test_support::ScriptedModel::new(vec![
            test_support::text(ORIGINAL),
            test_support::text("Public compacted summary."),
        ])),
    );
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let initial = agent
        .submit(chat, None, "Read the sensitive fixture.".into())
        .await
        .unwrap();
    loop {
        let event = test_support::next_event(&mut events).await;
        if event.run_id == initial.run_id && matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
    }
    agent.compact(chat).await.unwrap();
    let (url, mut captured) = http::serve_responses(vec![
        (
            200,
            response(
                json!({"type":"text","text":"Short completed turn."}),
                "end_turn",
            ),
        ),
        (
            200,
            response(json!({"type":"text","text":"run the tests"}), "end_turn"),
        ),
    ])
    .await;
    let mut model = test_support::config();
    model.base_url = url;
    model.provider_id = "fixture-next-provider".into();
    model.name = "Next Provider fixture".into();
    agent
        .set_model(
            chat,
            model.clone(),
            Arc::new(HttpModelClient::new(model).unwrap()),
        )
        .await
        .unwrap();
    let next = agent
        .submit(chat, None, "Check the public summary.".into())
        .await
        .unwrap();
    loop {
        let event = test_support::next_event(&mut events).await;
        if event.run_id == next.run_id && matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
        if matches!(event.kind, EventKind::Failed(_)) {
            panic!("{event:?}");
        }
    }
    let (_, normal) = captured.recv().await.unwrap();
    assert!(!normal.to_string().contains(ORIGINAL));
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(
        snapshot
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .any(|content| matches!(content, Content::Text { text } if text.contains(ORIGINAL)))
    );
    assert!(generate(&request(), &snapshot, None).await.text.is_some());
    let (_, suggestion) = captured.recv().await.unwrap();
    assert!(
        !suggestion.to_string().contains(ORIGINAL),
        "suggestion crossed the compaction boundary"
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn prompt_suggestion_openai_stream_hits_the_output_budget_before_the_deadline() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let (started, received) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let (_, body) = http::read_request(&mut socket).await;
        assert_eq!(body["max_tokens"], 1_024);
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        let chunk = json!({"id":"fixture","choices":[{"index":0,"delta":{"content":"x".repeat(1_024)},"finish_reason":null}]});
        socket
            .write_all(format!("data: {chunk}\n\n").as_bytes())
            .await
            .unwrap();
        started.send(()).unwrap();
        let mut byte = [0];
        let closed =
            tokio::time::timeout(std::time::Duration::from_secs(2), socket.read(&mut byte)).await;
        assert!(
            matches!(closed, Ok(Ok(0)) | Ok(Err(_))),
            "bounded request should close the transport"
        );
    });
    let mut model = test_support::config();
    model.base_url = url;
    model.protocol = tickloop_agent::model::Protocol::ChatCompletions;
    let context = snapshot(model, conversation()).await;
    let request = request();
    let running = request.clone();
    let generation = tokio::spawn(async move { generate(&running, &context, None).await });
    received.await.unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_millis(500), generation).await;
    assert!(
        result.is_ok(),
        "stream budget must cancel before the ten-second deadline"
    );
    let result = result.unwrap().unwrap();
    assert!(result.text.is_none());
    assert_eq!(result.diagnostic, "failed: output budget reached");
    assert!(request.cancel.is_cancelled());
    server.await.unwrap();
}

#[tokio::test]
async fn prompt_suggestion_http_uses_current_model_without_tools_or_private_history() {
    let (url, received) = http::serve_response(
        200,
        response(json!({"type":"text","text":"进度怎么样了"}), "end_turn"),
    )
    .await;
    let mut model = test_support::config();
    model.base_url = url;
    model.max_output_tokens = 16_000;
    let mut messages = conversation();
    messages.insert(
        0,
        Message {
            role: Role::System,
            content: test_support::text("private system fixture"),
        },
    );
    messages[2].content.push(Content::Thinking {
        thinking: "private reasoning fixture".into(),
        signature: None,
    });
    messages[2].content.push(Content::ToolUse {
        id: "hidden-call".into(),
        name: "Read".into(),
        input: json!({"file_path":"private tool fixture"}),
    });
    assert_eq!(
        generate(&request(), &snapshot(model, messages).await, None)
            .await
            .text
            .as_deref(),
        Some("进度怎么样了")
    );
    let (_, body) = received.await.unwrap();
    assert_eq!(body["model"], "fixture");
    assert_eq!(body["max_tokens"], 1_024);
    assert!(
        body.get("tools")
            .is_none_or(|tools| tools.as_array().is_some_and(Vec::is_empty))
    );
    let serialized = body.to_string();
    assert!(serialized.contains("修好了检查一下"));
    assert!(!serialized.contains("private system fixture"));
    assert!(!serialized.contains("private reasoning fixture"));
    assert!(!serialized.contains("private tool fixture"));
}

#[tokio::test]
async fn prompt_suggestion_http_filters_meta_errors_formatting_and_tool_proposals() {
    for text in [
        "done",
        "Looks good",
        "[no suggestion]",
        "API Error: bad fixture",
        "I'll run the tests",
        "run\nthe tests",
        "one two three four five six seven eight nine ten eleven twelve thirteen",
    ] {
        let (url, _) = http::serve_response(
            200,
            response(json!({"type":"text","text":text}), "end_turn"),
        )
        .await;
        let mut model = test_support::config();
        model.base_url = url;
        assert!(
            generate(&request(), &snapshot(model, conversation()).await, None)
                .await
                .text
                .is_none(),
            "{text}"
        );
    }
    let (url, _) = http::serve_response(200, response(json!({"type":"tool_use","id":"proposal","name":"Shell","input":{"command":"touch forbidden"}}), "tool_use")).await;
    let mut model = test_support::config();
    model.base_url = url;
    assert!(
        generate(&request(), &snapshot(model, conversation()).await, None)
            .await
            .text
            .is_none()
    );
}

#[tokio::test]
async fn prompt_suggestion_http_cancellation_and_deadline_drop_a_delayed_result() {
    for timeout in [false, true] {
        let (url, mut received, release) = http::serve_responses_with_gated_response(
            vec![(
                200,
                response(json!({"type":"text","text":"run the tests"}), "end_turn"),
            )],
            0,
        )
        .await;
        let mut model = test_support::config();
        model.base_url = url;
        let request = request();
        let running = request.clone();
        let generation = tokio::spawn(async move {
            generate(&running, &snapshot(model, conversation()).await, None).await
        });
        received.recv().await.unwrap();
        if timeout {
            tokio::time::pause();
            tokio::time::advance(std::time::Duration::from_secs(11)).await;
        } else {
            request.cancel.cancel();
        }
        let result = generation.await.unwrap();
        assert!(result.text.is_none());
        assert_eq!(
            result.diagnostic,
            if timeout {
                "failed: generation deadline"
            } else {
                "cancelled"
            }
        );
        assert!(request.cancel.is_cancelled());
        drop(release);
        if timeout {
            tokio::time::resume();
        }
    }
}

#[tokio::test]
async fn prompt_suggestion_diagnostics_distinguish_empty_filtered_and_incomplete_results() {
    for (text, stop, diagnostic, expected) in [
        ("", "end_turn", "no obvious continuation", None),
        ("无建议", "end_turn", "rejected: suggestion text", None),
        (
            "查看说明文件",
            "max_tokens",
            "failed: output budget reached",
            None,
        ),
        (
            "查看说明文件",
            "end_turn",
            "generated",
            Some("查看说明文件"),
        ),
    ] {
        let (url, received) =
            http::serve_response(200, response(json!({"type":"text","text":text}), stop)).await;
        let mut model = test_support::config();
        model.base_url = url;
        let result = generate(&request(), &snapshot(model, conversation()).await, None).await;
        assert_eq!(result.diagnostic, diagnostic);
        assert_eq!(result.text.as_deref(), expected);
        received.await.unwrap();
    }
}

#[tokio::test]
async fn prompt_suggestion_http_errors_are_silent_and_pre_cancelled_requests_never_connect() {
    let (url, received) = http::serve_response(500, "fixture private error payload".into()).await;
    let mut model = test_support::config();
    model.base_url = url;
    let result = generate(&request(), &snapshot(model, conversation()).await, None).await;
    assert!(result.text.is_none());
    assert_eq!(result.diagnostic, "failed: Provider request");
    received.await.unwrap();
    let (url, mut received) = http::serve_response(
        200,
        response(json!({"type":"text","text":"run the tests"}), "end_turn"),
    )
    .await;
    let mut model = test_support::config();
    model.base_url = url;
    let request = request();
    request.cancel.cancel();
    assert!(
        generate(&request, &snapshot(model, conversation()).await, None)
            .await
            .text
            .is_none()
    );
    assert!(matches!(
        received.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
}
