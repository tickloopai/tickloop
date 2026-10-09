use tickloop_agent_test_support::model_http_support;

use std::{path::Path, sync::Arc};

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use image::{DynamicImage, ImageFormat, Rgb, RgbImage};
use model_http_support::serve_response;
use serde_json::{Value, json};
use tickloop_agent_model::{HttpModelClient, ModelClient, ModelRequest};
use tickloop_agent_protocol::{
    ChatId, Content, EventKind, ImageMediaType, ImageSource, InputContext, InputModality, Message,
    ModelConfig, Protocol, Role, ToolResultBlock, ToolResultContent,
};
use tickloop_agent_runtime::Agent;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const UNSUPPORTED_IMAGE_TEXT: &str =
    "ERROR: Cannot read image (this model does not support image input). Inform the user.";
const IMAGE_DATA: &str = "iVBORw0KGgo=";

fn configuration(
    base_url: String,
    protocol: Protocol,
    modalities: &[InputModality],
) -> ModelConfig {
    ModelConfig {
        provider: tickloop_agent_protocol::ModelProvider::Api,
        auth_source: tickloop_agent_protocol::ModelAuthSource::ApiKeyEnv,
        name: "DeepSeek".into(),
        protocol,
        base_url: match protocol {
            Protocol::Messages => base_url,
            _ => base_url.trim_end_matches("/anthropic").into(),
        },
        model: "deepseek-flash".into(),
        api_key_env: String::new(),
        max_output_tokens: 1024,
        provider_id: String::new(),
        provider_name: String::new(),
        model_id: String::new(),
        connection: None,
        card: None,
        input_modalities: modalities.to_vec(),
    }
}

fn event(event_type: &str, mut data: Value) -> String {
    data["type"] = json!(event_type);
    format!("event: {event_type}\ndata: {data}\n\n")
}

fn text_stream(protocol: Protocol) -> String {
    match protocol {
        Protocol::Messages => [
            event(
                "message_start",
                json!({"message":{"role":"assistant","content":[]}}),
            ),
            event(
                "content_block_start",
                json!({"index":0,"content_block":{"type":"text","text":"I cannot see images."}}),
            ),
            event("content_block_stop", json!({"index":0})),
            event("message_delta", json!({"delta":{"stop_reason":"end_turn"}})),
            event("message_stop", json!({})),
        ]
        .concat(),
        Protocol::Responses => {
            let item = json!({"type":"message","id":"msg-1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"I cannot see images.","annotations":[]}]});
            [
                event(
                    "response.created",
                    json!({"response":{"id":"resp-1","status":"in_progress"}}),
                ),
                event(
                    "response.output_item.added",
                    json!({"output_index":0,"item":{"type":"message","id":"msg-1","role":"assistant","status":"in_progress","content":[]}}),
                ),
                event(
                    "response.output_text.delta",
                    json!({"output_index":0,"item_id":"msg-1","content_index":0,"delta":"I cannot see images."}),
                ),
                event(
                    "response.output_item.done",
                    json!({"output_index":0,"item":item}),
                ),
                event(
                    "response.completed",
                    json!({"response":{"id":"resp-1","status":"completed","output":[item]}}),
                ),
            ]
            .concat()
        }
        Protocol::ChatCompletions => {
            let chunk = |delta: Value, finish_reason: Value| {
                format!(
                    "data: {}\n\n",
                    json!({"id":"chat-1","choices":[{"index":0,"delta":delta,"finish_reason":finish_reason}]})
                )
            };
            [
                chunk(
                    json!({"role":"assistant","content":"I cannot see images."}),
                    Value::Null,
                ),
                chunk(json!({}), json!("stop")),
                "data: [DONE]\n\n".into(),
            ]
            .concat()
        }
        Protocol::Google => format!(
            "data: {}\n\n",
            json!({"responseId":"google-1","candidates":[{"index":0,"content":{"role":"model","parts":[{"text":"I cannot see images."}]},"finishReason":"STOP"}]})
        ),
        Protocol::SystemOne => unreachable!("System One never receives image prompts"),
    }
}

fn image_request() -> ModelRequest {
    ModelRequest {
        history_sources: Vec::new(),
        messages: vec![Message {
            role: Role::User,
            content: vec![
                Content::Text {
                    text: "chart.png".into(),
                },
                Content::Image {
                    source: ImageSource::Base64 {
                        media_type: ImageMediaType::Png,
                        data: IMAGE_DATA.into(),
                    },
                    width: 640,
                    height: 480,
                },
            ],
        }],
        tools: vec![],
        structured_output: None,
        temperature: None,
        reasoning_effort: None,
    }
}

#[tokio::test]
async fn every_protocol_replaces_images_with_an_explanation_for_a_text_only_model() {
    for protocol in [
        Protocol::Messages,
        Protocol::Responses,
        Protocol::ChatCompletions,
    ] {
        let (base_url, request_receiver) = serve_response(200, text_stream(protocol)).await;
        let client =
            HttpModelClient::new(configuration(base_url, protocol, &[InputModality::Text]))
                .unwrap();
        let (events, _) = mpsc::unbounded_channel();

        client
            .complete(image_request(), events, CancellationToken::new())
            .await
            .unwrap_or_else(|error| panic!("{protocol:?}: {error}"));

        let (_, body) = request_receiver.await.unwrap();
        let body = body.to_string();
        assert!(
            body.contains(UNSUPPORTED_IMAGE_TEXT),
            "{protocol:?}: {body}"
        );
        assert!(body.contains("chart.png"), "{protocol:?}: {body}");
        assert!(!body.contains(IMAGE_DATA), "{protocol:?}: {body}");
    }
}

#[tokio::test]
async fn messages_replaces_each_image_block_in_place() {
    let (base_url, request_receiver) = serve_response(200, text_stream(Protocol::Messages)).await;
    let client = HttpModelClient::new(configuration(
        base_url,
        Protocol::Messages,
        &[InputModality::Text],
    ))
    .unwrap();
    let (events, _) = mpsc::unbounded_channel();

    client
        .complete(image_request(), events, CancellationToken::new())
        .await
        .unwrap();

    let (_, body) = request_receiver.await.unwrap();
    assert_eq!(
        body["messages"],
        json!([{"role":"user","content":[
            {"type":"text","text":"chart.png"},
            {"type":"text","text":UNSUPPORTED_IMAGE_TEXT}
        ]}])
    );
}

fn tool_result_image_request() -> ModelRequest {
    ModelRequest {
        history_sources: Vec::new(),
        messages: vec![
            Message {
                role: Role::Assistant,
                content: vec![Content::ToolUse {
                    id: "call-1".into(),
                    name: "Read".into(),
                    input: json!({"file_path":"chart.png"}),
                }],
            },
            Message {
                role: Role::User,
                content: vec![Content::ToolResult {
                    tool_use_id: "call-1".into(),
                    content: ToolResultContent::Blocks(vec![
                        ToolResultBlock::Text {
                            text: "chart.png summary".into(),
                        },
                        ToolResultBlock::Image {
                            source: ImageSource::Base64 {
                                media_type: ImageMediaType::Png,
                                data: IMAGE_DATA.into(),
                            },
                            width: 640,
                            height: 480,
                        },
                    ]),
                    is_error: false,
                }],
            },
        ],
        tools: vec![],
        structured_output: None,
        temperature: None,
        reasoning_effort: None,
    }
}

#[tokio::test]
async fn every_protocol_replaces_tool_result_images_for_a_text_only_model() {
    for protocol in [
        Protocol::Messages,
        Protocol::Responses,
        Protocol::ChatCompletions,
    ] {
        let (base_url, request_receiver) = serve_response(200, text_stream(protocol)).await;
        let client =
            HttpModelClient::new(configuration(base_url, protocol, &[InputModality::Text]))
                .unwrap();
        let (events, _) = mpsc::unbounded_channel();

        client
            .complete(
                tool_result_image_request(),
                events,
                CancellationToken::new(),
            )
            .await
            .unwrap_or_else(|error| panic!("{protocol:?}: {error}"));

        let (_, body) = request_receiver.await.unwrap();
        if protocol == Protocol::Messages {
            assert_eq!(
                body["messages"][1]["content"][0]["content"],
                json!([
                    {"type":"text","text":"chart.png summary"},
                    {"type":"text","text":UNSUPPORTED_IMAGE_TEXT}
                ])
            );
        }
        let body = body.to_string();
        assert!(
            body.contains(UNSUPPORTED_IMAGE_TEXT),
            "{protocol:?}: {body}"
        );
        assert!(!body.contains(IMAGE_DATA), "{protocol:?}: {body}");
    }
}

fn write_png(path: &Path) -> Vec<u8> {
    let pixels = RgbImage::from_fn(12, 10, |x, y| Rgb([x as u8 * 20, y as u8 * 20, 90]));
    let mut bytes = Vec::new();
    DynamicImage::ImageRgb8(pixels)
        .write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Png)
        .unwrap();
    std::fs::write(path, &bytes).unwrap();
    bytes
}

async fn complete_turn(agent: &Agent, chat: ChatId, prompt: &str, paths: Vec<std::path::PathBuf>) {
    let mut events = agent.subscribe();
    let receipt = agent
        .submit_with_file_references(chat, None, prompt.into(), paths)
        .await
        .unwrap();
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        if event.run_id != receipt.run_id {
            continue;
        }
        match event.kind {
            EventKind::Completed { .. } => return,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
}

async fn switch_model(agent: &Agent, chat: ChatId, config: ModelConfig) {
    let client = Arc::new(HttpModelClient::new(config.clone()).unwrap());
    agent.set_model(chat, config, client).await.unwrap();
}

fn user_content(body: &Value) -> Vec<Value> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "user")
        .flat_map(|message| message["content"].as_array().unwrap().clone())
        .collect()
}

#[tokio::test]
async fn history_images_are_replaced_for_a_text_only_model_and_sent_again_after_switching_back() {
    let directory = tempfile::tempdir().unwrap();
    let image = directory.path().join("chart.png");
    let data = BASE64.encode(write_png(&image));
    let with_images = [InputModality::Text, InputModality::Image];
    let image_block =
        json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":data}});
    let unsupported = json!({"type":"text","text":UNSUPPORTED_IMAGE_TEXT});

    let (vision_url, vision_request) = serve_response(200, text_stream(Protocol::Messages)).await;
    let vision = configuration(vision_url, Protocol::Messages, &with_images);
    let agent = Agent::new_embedded(
        vision.clone(),
        Arc::new(HttpModelClient::new(vision).unwrap()),
    );
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    complete_turn(&agent, chat, "Describe @chart.png", vec![image]).await;
    let (_, body) = vision_request.await.unwrap();
    assert!(user_content(&body).contains(&image_block));

    let (text_url, text_request) = serve_response(200, text_stream(Protocol::Messages)).await;
    switch_model(
        &agent,
        chat,
        ModelConfig {
            name: "text only".into(),
            ..configuration(text_url, Protocol::Messages, &[InputModality::Text])
        },
    )
    .await;
    complete_turn(&agent, chat, "What was in the chart?", Vec::new()).await;
    let (_, body) = text_request.await.unwrap();
    let content = user_content(&body);
    assert!(content.contains(&unsupported), "{body}");
    assert!(!body.to_string().contains(&data));
    assert!(matches!(
        &agent.snapshot(chat).await.unwrap().message_contexts[&0][..],
        [InputContext::Image(context)] if context.image.data == data
    ));

    let (back_url, back_request) = serve_response(200, text_stream(Protocol::Messages)).await;
    switch_model(
        &agent,
        chat,
        configuration(back_url, Protocol::Messages, &with_images),
    )
    .await;
    complete_turn(&agent, chat, "Look again", Vec::new()).await;
    let (_, body) = back_request.await.unwrap();
    let content = user_content(&body);
    assert!(content.contains(&image_block), "{body}");
    assert!(!content.contains(&unsupported));
}
