use tickloop_agent_test_support::model_http_support;

use std::{path::Path, sync::Arc};

use image::{DynamicImage, ImageFormat, Rgb, RgbImage};
use serde_json::{Value, json};
use tickloop_agent_model::{HttpModelClient, ModelClient, ModelError, ModelRequest};
use tickloop_agent_protocol::{
    ChatId, Content, DirectedSummaryRequest, EventKind, ImageMediaType, ImageSource, Message,
    ModelConfig, Role, SummaryDirection, ToolResultBlock, ToolResultContent,
};
use tickloop_agent_runtime::Agent;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use model_http_support::serve_responses;
use tickloop_agent_test_support::{ScriptedModel, call, config, next_event, text};

const IMAGE_SIZE_REJECTION: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"messages.0.content.1.image.source.base64: image exceeds 5 MB maximum: 5316852 bytes > 5242880 bytes"}}"#;
const MANY_IMAGE_DIMENSION_REJECTION: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"messages.3.content.1.image.source.base64.data: At least one of the image dimensions exceed max allowed size for many-image requests: 2000 pixels"}}"#;
const IMAGE_COUNT_REJECTION: &str = r#"{"error":{"message":"Too many images in request. Max is 100.","type":"invalid_request_error"}}"#;

fn write_png(path: &Path) {
    let mut bytes = Vec::new();
    DynamicImage::ImageRgb8(RgbImage::from_pixel(8, 8, Rgb([200, 40, 40])))
        .write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Png)
        .unwrap();
    std::fs::write(path, bytes).unwrap();
}

async fn submit_and_wait(
    agent: &Agent,
    chat: ChatId,
    prompt: &str,
    paths: Vec<std::path::PathBuf>,
) {
    let mut events = agent.subscribe();
    let receipt = agent
        .submit_with_file_references(chat, None, prompt.into(), paths)
        .await
        .unwrap();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id == receipt.run_id && matches!(event.kind, EventKind::Completed { .. }) {
            return;
        }
    }
}

fn image_count(request: &ModelRequest) -> usize {
    request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .map(|content| match content {
            Content::Image { .. } => 1,
            Content::ToolResult {
                content: ToolResultContent::Blocks(blocks),
                ..
            } => blocks
                .iter()
                .filter(|block| matches!(block, ToolResultBlock::Image { .. }))
                .count(),
            _ => 0,
        })
        .sum()
}

fn tool_result_blocks(request: &ModelRequest) -> Vec<ToolResultBlock> {
    request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::ToolResult {
                content: ToolResultContent::Blocks(blocks),
                ..
            } => Some(blocks.clone()),
            _ => None,
        })
        .flatten()
        .collect()
}

fn contains_text(request: &ModelRequest, expected: &str) -> bool {
    request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .any(|content| matches!(content, Content::Text { text } if text == expected))
}

/// A Chat whose first prompt attached `chart.png` and whose second prompt is plain text.
async fn chat_with_an_image(model: Arc<ScriptedModel>) -> (tempfile::TempDir, Agent, ChatId) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("chart.png");
    write_png(&path);
    let agent =
        Agent::open_embedded(directory.path().join("state"), config(), model.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    submit_and_wait(&agent, chat, "Describe @chart.png", vec![path]).await;
    submit_and_wait(&agent, chat, "Compare it with last week", vec![]).await;
    assert_eq!(image_count(&model.requests.lock().unwrap()[1]), 1);
    (directory, agent, chat)
}

#[tokio::test]
async fn full_compaction_summarizes_images_as_placeholders() {
    let model = Arc::new(ScriptedModel::new(vec![
        text("A red chart."),
        text("Similar to last week."),
        text("Summary of the chart discussion."),
    ]));
    let (_directory, agent, chat) = chat_with_an_image(model.clone()).await;

    agent.compact(chat).await.unwrap();

    let summary_request = model.requests.lock().unwrap()[2].clone();
    assert_eq!(image_count(&summary_request), 0);
    assert!(contains_text(&summary_request, "[image]"));
    assert!(contains_text(&summary_request, "chart.png"));
    agent.shutdown().await;
}

#[tokio::test]
async fn directed_summaries_in_both_directions_summarize_images_as_placeholders() {
    for (direction, checkpoint) in [
        (SummaryDirection::UpToHere, 1),
        (SummaryDirection::FromHere, 0),
    ] {
        let model = Arc::new(ScriptedModel::new(vec![
            text("A red chart."),
            text("Similar to last week."),
            text("Summary of the chart discussion."),
        ]));
        let (_directory, agent, chat) = chat_with_an_image(model.clone()).await;
        let point = agent.snapshot(chat).await.unwrap().rewind_points()[checkpoint].clone();

        agent
            .summarize_checkpoint(
                chat,
                DirectedSummaryRequest {
                    message_id: point.message_id,
                    direction,
                    guidance: None,
                },
            )
            .await
            .unwrap();

        let summary_request = model.requests.lock().unwrap()[2].clone();
        assert_eq!(image_count(&summary_request), 0, "{direction:?}");
        assert!(contains_text(&summary_request, "[image]"), "{direction:?}");
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn compaction_summarizes_images_read_by_tools_as_placeholders() {
    let directory = tempfile::tempdir().unwrap();
    write_png(&directory.path().join("chart.png"));
    let model = Arc::new(ScriptedModel::new(vec![
        call("read-chart", "Read", json!({"file_path":"chart.png"})),
        text("A red chart."),
        text("Summary of the chart discussion."),
    ]));
    let agent =
        Agent::open_embedded(directory.path().join("state"), config(), model.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    submit_and_wait(&agent, chat, "Look at chart.png", vec![]).await;
    assert_eq!(image_count(&model.requests.lock().unwrap()[1]), 1);

    agent.compact(chat).await.unwrap();

    let summary_request = model.requests.lock().unwrap()[2].clone();
    assert_eq!(image_count(&summary_request), 0);
    assert!(
        tool_result_blocks(&summary_request).contains(&ToolResultBlock::Text {
            text: "[image]".into()
        })
    );
    agent.shutdown().await;
}

fn http_config(base_url: String) -> ModelConfig {
    ModelConfig {
        base_url,
        ..config()
    }
}

fn image(index: usize) -> Content {
    Content::Image {
        source: ImageSource::Base64 {
            media_type: ImageMediaType::Png,
            data: format!("image-{index}"),
        },
        width: 16,
        height: 16,
    }
}

fn request_with_images(count: usize) -> ModelRequest {
    ModelRequest {
        history_sources: Vec::new(),
        messages: (0..count)
            .map(|index| Message {
                role: Role::User,
                content: vec![
                    Content::Text {
                        text: format!("label-{index}"),
                    },
                    image(index),
                ],
            })
            .collect(),
        tools: vec![],
        structured_output: None,
        temperature: None,
        reasoning_effort: None,
    }
}

fn event(event_type: &str, mut data: Value) -> String {
    data["type"] = json!(event_type);
    format!("event: {event_type}\ndata: {data}\n\n")
}

fn text_response(answer: &str) -> String {
    [
        event(
            "message_start",
            json!({"message":{"role":"assistant","content":[]}}),
        ),
        event(
            "content_block_start",
            json!({"index":0,"content_block":{"type":"text","text":""}}),
        ),
        event(
            "content_block_delta",
            json!({"index":0,"delta":{"type":"text_delta","text":answer}}),
        ),
        event("content_block_stop", json!({"index":0})),
        event("message_delta", json!({"delta":{"stop_reason":"end_turn"}})),
        event("message_stop", json!({})),
    ]
    .concat()
}

async fn complete(
    base_url: String,
    request: ModelRequest,
) -> Result<tickloop_agent_model::ModelOutput, ModelError> {
    let client = HttpModelClient::new(http_config(base_url)).unwrap();
    let (events, _) = mpsc::unbounded_channel();
    client
        .complete(request, events, CancellationToken::new())
        .await
}

async fn next_request(requests: &mut mpsc::UnboundedReceiver<(String, Value)>) -> Value {
    tokio::time::timeout(std::time::Duration::from_secs(5), requests.recv())
        .await
        .expect("request timed out")
        .expect("server stopped")
        .1
}

/// The wire blocks of every message, flattened in request order.
fn sent_blocks(request: &Value) -> Vec<Value> {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| message["content"].as_array().unwrap().clone())
        .collect()
}

fn sent_image_data(request: &Value) -> Vec<String> {
    sent_blocks(request)
        .into_iter()
        .filter(|block| block["type"] == "image")
        .map(|block| block["source"]["data"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn requests_over_one_hundred_images_replace_the_oldest_with_placeholders() {
    let (base_url, mut requests) = serve_responses(vec![(200, text_response("ok"))]).await;

    complete(base_url, request_with_images(103)).await.unwrap();

    let request = next_request(&mut requests).await;
    assert_eq!(
        sent_image_data(&request),
        (3..103)
            .map(|index| format!("image-{index}"))
            .collect::<Vec<_>>()
    );
    let blocks = sent_blocks(&request);
    for index in 0..3 {
        assert_eq!(
            blocks[index * 2],
            json!({"type":"text","text":format!("label-{index}")})
        );
        assert_eq!(
            blocks[index * 2 + 1],
            json!({"type":"text","text":"[image]"})
        );
    }
}

#[tokio::test]
async fn tool_result_images_count_toward_the_limit_and_are_replaced_oldest_first() {
    let (base_url, mut requests) = serve_responses(vec![(200, text_response("ok"))]).await;
    let mut request = request_with_images(100);
    request.messages.splice(
        0..0,
        [
            Message {
                role: Role::Assistant,
                content: vec![Content::ToolUse {
                    id: "read-chart".into(),
                    name: "Read".into(),
                    input: json!({"file_path":"chart.png"}),
                }],
            },
            Message {
                role: Role::User,
                content: vec![Content::ToolResult {
                    tool_use_id: "read-chart".into(),
                    content: ToolResultContent::Blocks(vec![
                        ToolResultBlock::Text {
                            text: "chart.png summary".into(),
                        },
                        ToolResultBlock::Image {
                            source: ImageSource::Base64 {
                                media_type: ImageMediaType::Png,
                                data: "tool-image".into(),
                            },
                            width: 16,
                            height: 16,
                        },
                    ]),
                    is_error: false,
                }],
            },
        ],
    );

    complete(base_url, request).await.unwrap();

    let request = next_request(&mut requests).await;
    assert_eq!(
        request["messages"][1]["content"][0]["content"],
        json!([
            {"type":"text","text":"chart.png summary"},
            {"type":"text","text":"[image]"}
        ])
    );
    assert_eq!(sent_image_data(&request).len(), 100);
}

#[tokio::test]
async fn requests_with_exactly_one_hundred_images_are_sent_unchanged() {
    let (base_url, mut requests) = serve_responses(vec![(200, text_response("ok"))]).await;

    complete(base_url, request_with_images(100)).await.unwrap();

    let request = next_request(&mut requests).await;
    assert_eq!(sent_image_data(&request).len(), 100);
}

#[tokio::test]
async fn image_rejections_are_retried_once_without_images() {
    for (status, rejection) in [
        (400, IMAGE_SIZE_REJECTION),
        (400, MANY_IMAGE_DIMENSION_REJECTION),
        (400, IMAGE_COUNT_REJECTION),
        (413, IMAGE_SIZE_REJECTION),
    ] {
        let (base_url, mut requests) = serve_responses(vec![
            (status, rejection.into()),
            (200, text_response("Answered without the image")),
        ])
        .await;

        let output = complete(base_url, request_with_images(2)).await.unwrap();

        assert_eq!(
            output.content,
            vec![Content::Text {
                text: "Answered without the image".into()
            }],
            "{rejection}"
        );
        let first = next_request(&mut requests).await;
        let retry = next_request(&mut requests).await;
        assert_eq!(sent_image_data(&first).len(), 2);
        assert!(sent_image_data(&retry).is_empty());
        assert_eq!(
            sent_blocks(&retry),
            vec![
                json!({"type":"text","text":"label-0"}),
                json!({"type":"text","text":"[image]"}),
                json!({"type":"text","text":"label-1"}),
                json!({"type":"text","text":"[image]"}),
            ]
        );
    }
}

#[tokio::test]
async fn a_failed_retry_reports_the_normal_error_without_retrying_again() {
    let (base_url, mut requests) = serve_responses(vec![
        (400, IMAGE_SIZE_REJECTION.into()),
        (400, IMAGE_SIZE_REJECTION.into()),
        (200, text_response("never requested")),
    ])
    .await;

    let error = complete(base_url, request_with_images(1))
        .await
        .unwrap_err();

    assert_eq!(error.status(), Some(400), "{error}");
    assert!(
        error
            .provider_failure()
            .unwrap()
            .message
            .contains("image exceeds")
    );
    next_request(&mut requests).await;
    next_request(&mut requests).await;
    assert!(requests.try_recv().is_err());
}

#[tokio::test]
async fn other_rejections_and_requests_without_images_are_not_retried() {
    let text_only = ModelRequest {
        history_sources: Vec::new(),
        messages: vec![Message {
            role: Role::User,
            content: vec![Content::Text {
                text: "Hello".into(),
            }],
        }],
        tools: vec![],
        structured_output: None,
        temperature: None,
        reasoning_effort: None,
    };
    for (status, body, request) in [
        (
            400,
            r#"{"error":{"message":"max_tokens is too large"}}"#,
            request_with_images(1),
        ),
        (500, IMAGE_SIZE_REJECTION, request_with_images(1)),
        (400, IMAGE_SIZE_REJECTION, text_only),
    ] {
        let (base_url, mut requests) = serve_responses(vec![
            (status, body.into()),
            (200, text_response("never requested")),
        ])
        .await;

        let error = complete(base_url, request).await.unwrap_err();

        assert_eq!(error.status(), Some(status), "{body}: {error}");
        next_request(&mut requests).await;
        assert!(requests.try_recv().is_err(), "{body} was retried");
    }
}
