//! Opt-in real-account probe for Zhipu GLM / Z.AI. It calls only an inert Echo fixture and a
//! structured fixture; the API key comes from STATE_DIRECTORY/.env or the environment.

use std::{error::Error, os::unix::fs::PermissionsExt, path::PathBuf};

use serde_json::json;
use tickloop_agent_model::{ModelRequest, StructuredOutput, cards::ModelCatalog, model_client};
use tickloop_agent_protocol::{Content, Message, ReasoningEffort, Role, ToolDefinition};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if !(2..=4).contains(&arguments.len()) {
        return Err(
            "usage: glm_probe cn|global|cn-coding|global-coding STATE_DIRECTORY [MODEL_ID] [image]"
                .into(),
        );
    }
    let provider = match arguments[0].as_str() {
        "cn" => "zhipu-cn-api",
        "global" => "zai-global-api",
        "cn-coding" => "zhipu-cn-coding",
        "global-coding" => "zai-global-coding",
        _ => return Err("connection must be cn, global, cn-coding or global-coding".into()),
    };
    let root = PathBuf::from(&arguments[1]);
    if !root.exists() {
        std::fs::create_dir_all(&root)?;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
    }
    let root = root.canonicalize()?;
    let cancel = CancellationToken::new();
    let interrupted = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            interrupted.cancel();
        }
    });
    let model_id = arguments.get(2).map_or("glm-5.3", String::as_str);
    let mut config = ModelCatalog::defaults()
        .get(provider, model_id)
        .ok_or("GLM model missing")?;
    config.max_output_tokens = 4096;
    config
        .card
        .as_mut()
        .ok_or("model card missing")?
        .max_output_tokens = 4096;
    let client = model_client(config, Some(&root))?;
    let (events, _receiver) = mpsc::unbounded_channel();
    let mut request = ModelRequest {
        history_sources: vec![None],
        messages: vec![Message {
            role: Role::User,
            content: vec![Content::Text {
                text: r#"This is the first turn of an inert protocol test. Call the actual Echo tool exactly once. Its input must be exactly {"text":"hello"}. Do not describe, simulate or claim a tool call in text. After receiving the real tool result, report it briefly."#.into(),
            }],
        }],
        tools: vec![ToolDefinition {
            name: "Echo".into(),
            description: "Returns the text unchanged; no files, shell or external actions.".into(),
            input_schema: json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false}),
        }],
        structured_output: None,
        temperature: None,
        reasoning_effort: Some(ReasoningEffort::High),
    };
    let expected = if arguments.get(3).is_some_and(|value| value == "image") {
        use base64::Engine;
        use image::ImageEncoder;
        let image = image::RgbImage::from_pixel(32, 32, image::Rgb([255, 0, 0]));
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png).write_image(
            image.as_raw(),
            32,
            32,
            image::ColorType::Rgb8.into(),
        )?;
        request.messages[0].content = vec![Content::Text { text: "Inspect the image. Call Echo exactly once with text equal to the lowercase English name of its solid color; then report the tool result. Use the actual Echo tool, not a textual representation.".into() }, Content::Image {
            source: tickloop_agent_protocol::ImageSource::Base64 { media_type: tickloop_agent_protocol::ImageMediaType::Png, data: base64::engine::general_purpose::STANDARD.encode(png) }, width: 32, height: 32,
        }];
        "red"
    } else {
        "hello"
    };
    let first = client
        .complete(request.clone(), events.clone(), cancel.clone())
        .await?;
    println!("Tool turn usage: {:?}", first.usage);
    let mut results = Vec::new();
    for content in &first.content {
        if let Content::ToolUse { id, name, input } = content {
            let text = input["text"].as_str();
            if name != "Echo" || text != Some(expected) || !results.is_empty() {
                println!("Unexpected fixture invocation: {name} {input}");
                return Err("unexpected probe tool invocation; nothing executed".into());
            }
            results.push(Content::ToolResult {
                tool_use_id: id.clone(),
                content: expected.into(),
                is_error: false,
            });
        }
    }
    if results.len() != 1 {
        println!("Probe received no tool call (stop={})", first.stop_reason);
        return Err("model did not call the single probe tool".into());
    }
    request.messages.push(Message {
        role: Role::Assistant,
        content: first.content,
    });
    request.history_sources.push(first.private_source);
    request.messages.push(Message {
        role: Role::User,
        content: results,
    });
    request.history_sources.push(None);
    let second = client
        .complete(request, events.clone(), cancel.clone())
        .await?;
    if second
        .content
        .iter()
        .any(|block| matches!(block, Content::ToolUse { .. }))
    {
        return Err("model requested an extra probe tool; nothing executed".into());
    }
    println!("Tool continuation usage: {:?}", second.usage);
    let verdict = client.complete(ModelRequest {
        history_sources: vec![None],
        messages: vec![Message { role: Role::User, content: vec![Content::Text {
            text: "Return the structured verdict with allow=false and reason=probe. This is a protocol test, not authorization.".into(),
        }] }],
        tools: Vec::new(),
        structured_output: Some(StructuredOutput {
            name: "probe_verdict".into(),
            schema: json!({"type":"object","properties":{"allow":{"type":"boolean"},"reason":{"type":"string"}},"required":["allow","reason"],"additionalProperties":false}),
        }),
        temperature: Some(0.0),
        reasoning_effort: None,
    }, events, cancel).await?;
    let text = verdict
        .content
        .iter()
        .filter_map(|block| match block {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    let value: serde_json::Value = serde_json::from_str(text.trim())?;
    if value != json!({"allow":false,"reason":"probe"}) {
        return Err("structured verdict disagrees with fixture; no authorization granted".into());
    }
    println!(
        "GLM stream, tool continuation and structured-output probe passed. Verdict usage: {:?}",
        verdict.usage
    );
    Ok(())
}
