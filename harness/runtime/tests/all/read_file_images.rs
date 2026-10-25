use std::{path::Path, sync::Arc};
use tickloop_code_settings::local_agent::LocalAgent;

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use image::{DynamicImage, ImageFormat, Rgb, RgbImage};
use serde_json::json;
use tickloop_harness_model::ModelRequest;
use tickloop_harness_protocol::{
    ChatId, Content, EventKind, ImageMediaType, ImageSource, InputModality, ModelConfig,
    ToolResult, ToolResultBlock, ToolResultContent,
};
use tickloop_harness_runtime::Agent;

use tickloop_harness_test_support::{ScriptedModel, call, config, next_event, text};

fn write_png(path: &Path, width: u32, height: u32) -> Vec<u8> {
    let mut seed = width ^ (height << 16) ^ 0x9e37_79b9;
    let pixels = RgbImage::from_fn(width, height, |_, _| {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        let [red, green, blue, _] = seed.to_le_bytes();
        Rgb([red, green, blue])
    });
    let mut bytes = Vec::new();
    DynamicImage::ImageRgb8(pixels)
        .write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Png)
        .unwrap();
    std::fs::write(path, &bytes).unwrap();
    bytes
}

fn text_only_config() -> ModelConfig {
    ModelConfig {
        name: "text only".into(),
        input_modalities: vec![InputModality::Text],
        ..config()
    }
}

/// Submits a prompt and returns the tool results reported while the run completes.
async fn run_to_completion(agent: &Agent, chat: ChatId, prompt: &str) -> Vec<ToolResult> {
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, prompt.into()).await.unwrap();
    let mut results = Vec::new();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != receipt.run_id {
            continue;
        }
        match event.kind {
            EventKind::ToolCompleted { result, .. } => results.push(result),
            EventKind::Completed { .. } => return results,
            EventKind::Failed(error) => panic!("run failed: {error}"),
            _ => {}
        }
    }
}

/// The content of the tool result answering `call_id` in a model request.
fn tool_result<'a>(request: &'a ModelRequest, call_id: &str) -> &'a ToolResultContent {
    request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|content| match content {
            Content::ToolResult {
                tool_use_id,
                content,
                is_error: false,
            } if tool_use_id == call_id => Some(content),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no successful result for {call_id}"))
}

fn assert_summary_and_image(content: &ToolResultContent, expected_data: &str) {
    let ToolResultContent::Blocks(blocks) = content else {
        panic!("expected a text summary and an image block: {content:?}");
    };
    let [
        ToolResultBlock::Text { text: summary },
        ToolResultBlock::Image {
            source: ImageSource::Base64 { media_type, data },
            width,
            height,
        },
    ] = &blocks[..]
    else {
        panic!("expected a text summary and an image block: {blocks:?}");
    };
    assert!(summary.contains("chart.png"), "{summary}");
    assert!(summary.contains("image/png"), "{summary}");
    assert!(!summary.contains(expected_data));
    assert_eq!(*media_type, ImageMediaType::Png);
    assert_eq!(data, expected_data);
    assert_eq!((*width, *height), (64, 48));
}

#[tokio::test]
async fn read_returns_a_short_summary_and_the_image_to_the_model() {
    let directory = tempfile::tempdir().unwrap();
    let bytes = write_png(&directory.path().join("chart.png"), 64, 48);
    let model = Arc::new(ScriptedModel::new(vec![
        call("read-chart", "Read", json!({"file_path":"chart.png"})),
        text("A noisy chart."),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();

    let results = run_to_completion(&agent, chat, "Describe chart.png").await;

    let requests = model.requests.lock().unwrap().clone();
    let description = &requests[0]
        .tools
        .iter()
        .find(|tool| tool.name == "Read")
        .unwrap()
        .description;
    assert!(description.contains("image"), "{description}");
    assert_summary_and_image(
        tool_result(&requests[1], "read-chart"),
        &BASE64.encode(&bytes),
    );
    let [result] = &results[..] else {
        panic!("expected one tool result: {results:?}");
    };
    assert!(!result.is_error);
    assert!(result.content.contains("chart.png"));
    assert!(!result.content.contains(&BASE64.encode(&bytes)[..64]));
    agent.shutdown().await;
}

#[tokio::test]
async fn read_tells_a_text_only_model_it_cannot_read_images_and_the_run_continues() {
    let directory = tempfile::tempdir().unwrap();
    // A PNG signature over data no decoder accepts: only the capability check can succeed here.
    std::fs::write(
        directory.path().join("chart.png"),
        b"\x89PNG\r\n\x1a\nnot really an image",
    )
    .unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call("read-chart", "Read", json!({"file_path":"chart.png"})),
        text("I cannot see images with this model."),
    ]));
    let agent = Agent::new_embedded(text_only_config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();

    run_to_completion(&agent, chat, "Describe chart.png").await;

    let requests = model.requests.lock().unwrap().clone();
    let ToolResultContent::Text(error) = tool_result(&requests[1], "read-chart") else {
        panic!("expected a text error");
    };
    assert!(
        error.contains("does not support image input"),
        "unexpected result: {error}"
    );
    assert!(!error.contains("decode"), "the file was read: {error}");
    agent.shutdown().await;
}

#[tokio::test]
async fn text_files_are_still_read_as_text() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("notes.txt"), "PNG is a format.").unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call("read-notes", "Read", json!({"file_path":"notes.txt"})),
        text("Noted."),
    ]));
    let agent = Agent::new_embedded(text_only_config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();

    run_to_completion(&agent, chat, "Read notes.txt").await;

    let requests = model.requests.lock().unwrap().clone();
    let ToolResultContent::Text(result) = tool_result(&requests[1], "read-notes") else {
        panic!("expected a text result");
    };
    assert!(result.contains("PNG is a format."), "{result}");
    agent.shutdown().await;
}

#[tokio::test]
async fn images_read_by_tools_survive_reopening_the_transcript() {
    let directory = tempfile::tempdir().unwrap();
    let state = directory.path().join("state");
    let path = directory.path().join("chart.png");
    let bytes = write_png(&path, 64, 48);
    let agent = Agent::open_embedded(
        state.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![
            call("read-chart", "Read", json!({"file_path":"chart.png"})),
            text("A noisy chart."),
        ])),
    )
    .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    run_to_completion(&agent, chat, "Describe chart.png").await;
    agent.shutdown().await;
    drop(agent);
    std::fs::remove_file(&path).unwrap();

    let model = Arc::new(ScriptedModel::new(vec![text("Still the same chart.")]));
    let reopened = Agent::open_embedded(state, config(), model.clone()).unwrap();
    reopened.resume_chat(chat).await.unwrap();
    run_to_completion(&reopened, chat, "Look again").await;

    assert_summary_and_image(
        tool_result(&model.requests.lock().unwrap()[0], "read-chart"),
        &BASE64.encode(&bytes),
    );
    reopened.shutdown().await;
}

#[tokio::test]
async fn images_read_by_tools_count_by_pixels_so_they_do_not_force_compaction() {
    let directory = tempfile::tempdir().unwrap();
    let bytes = write_png(&directory.path().join("screenshot.png"), 400, 400);
    assert!(BASE64.encode(&bytes).len() > 400_000);
    let model = Arc::new(ScriptedModel::new(vec![
        call("read-shot", "Read", json!({"file_path":"screenshot.png"})),
        text("Looked at it."),
        text("Still fine."),
    ]));
    let agent = Agent::open_embedded(
        directory.path().join("state"),
        tickloop_harness_test_support::config_with_window(100_000),
        model.clone(),
    )
    .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();

    run_to_completion(&agent, chat, "Check screenshot.png").await;
    run_to_completion(&agent, chat, "Anything else?").await;

    let requests = model.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 3);
    assert!(tool_result(&requests[2], "read-shot").has_images());
    agent.shutdown().await;
}
