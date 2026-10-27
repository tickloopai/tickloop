use std::{error::Error, os::unix::fs::PermissionsExt, path::PathBuf};

use serde_json::json;
use tickloop_harness_model::{
    LocalModelClients, ModelClients, ModelRequest, StructuredOutput,
    auth::minimax::{MiniMaxAuth, MiniMaxRegion},
    cards::ModelCatalog,
};
use tickloop_harness_protocol::{Content, Message, ReasoningEffort, Role, ToolDefinition};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if !(3..=5).contains(&arguments.len())
        || !matches!(arguments[1].as_str(), "api" | "oauth" | "login")
    {
        return Err(
            "usage: minimax_probe cn|global api|oauth|login STATE_DIRECTORY [MODEL_ID] [image]"
                .into(),
        );
    }
    let (region, slug) = match arguments[0].as_str() {
        "cn" => (MiniMaxRegion::China, "cn"),
        "global" => (MiniMaxRegion::Global, "global"),
        _ => return Err("region must be cn or global".into()),
    };
    let root = PathBuf::from(&arguments[2]);
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
    if arguments[1] == "login" {
        let auth = MiniMaxAuth::new(&root, region);
        let login = auth.start_device_login(&cancel).await?;
        println!(
            "{} Portal authorization: {}",
            region.label(),
            login.verification_uri
        );
        println!("User code: {}", login.user_code);
        println!("Approve in your own browser. Ctrl+C cancels; tokens are never printed.");
        auth.finish_device_login(login, &cancel).await?;
        println!("Local OAuth credentials saved.");
    }
    let mode = if arguments[1] == "api" {
        "api"
    } else {
        "oauth"
    };
    let model_id = arguments.get(3).map_or("MiniMax-M3", String::as_str);
    let mut config = ModelCatalog::defaults()
        .get(&format!("minimax-{slug}-{mode}"), model_id)
        .ok_or("MiniMax model missing")?;
    config.max_output_tokens = 4096;
    config
        .card
        .as_mut()
        .ok_or("model card missing")?
        .max_output_tokens = 4096;
    let toggle = config
        .card
        .as_ref()
        .is_some_and(|card| card.thinking == tickloop_harness_protocol::ThinkingControl::Toggle);
    let effort = if toggle {
        ReasoningEffort::On
    } else {
        ReasoningEffort::Max
    };
    let client = LocalModelClients::new(Some(root.to_path_buf())).client(&config)?;
    let (events, _receiver) = mpsc::unbounded_channel();
    let mut request = ModelRequest {
        deferred_tools: Vec::new(),
        history_sources: vec![None],
        messages: vec![Message {
            role: Role::User,
            content: vec![Content::Text {
                text: r#"This is the first turn of an inert protocol test. Call the actual Echo tool exactly once. Its input must be exactly {"text":"hello"}. Do not describe, simulate or claim a tool call in text. After receiving the real tool_result, report it briefly."#.into(),
            }],
        }],
        tools: vec![ToolDefinition {
            name: "Echo".into(),
            description: "Returns the text unchanged; no files, shell or external actions.".into(),
            input_schema: json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false}),
        }],
        structured_output: None,
        temperature: Some(0.1),
        reasoning_effort: Some(effort),
    };
    let expected = if arguments.get(4).is_some_and(|value| value == "image") {
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
            source: tickloop_harness_protocol::ImageSource::Base64 { media_type: tickloop_harness_protocol::ImageMediaType::Png, data: base64::engine::general_purpose::STANDARD.encode(png) }, width: 32, height: 32,
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
            let matches_fixture = text.is_some_and(|value| {
                value == expected || expected == "red" && matches!(value, "darkred" | "dark red")
            });
            if name != "Echo" || !matches_fixture || !results.is_empty() {
                println!("Unexpected fixture invocation: {name} {input}");
                return Err("unexpected probe tool invocation; nothing executed".into());
            }
            results.push(Content::ToolResult {
                tool_use_id: id.clone(),
                content: text.ok_or("missing fixture text")?.into(),
                is_error: false,
            });
        }
    }
    if results.len() != 1 {
        let public_text = first
            .content
            .iter()
            .filter_map(|block| match block {
                Content::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        println!(
            "Probe received no tool call (stop={}): {}",
            first.stop_reason, public_text
        );
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
        deferred_tools: Vec::new(),
        history_sources: vec![None],
        messages: vec![Message { role: Role::User, content: vec![Content::Text {
            text: "Return the structured verdict with allow=false and reason=probe. This is a protocol test, not authorization.".into(),
        }] }],
        tools: Vec::new(),
        structured_output: Some(StructuredOutput {
            name: "probe_verdict".into(),
            schema: json!({"type":"object","properties":{"allow":{"type":"boolean"},"reason":{"type":"string"}},"required":["allow","reason"],"additionalProperties":false}),
        }),
        temperature: None,
        reasoning_effort: Some(if toggle { ReasoningEffort::Off } else { effort }),
    }, events, cancel).await?;
    let text = verdict
        .content
        .iter()
        .filter_map(|block| match block {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    let value: serde_json::Value = serde_json::from_str(&text)?;
    if value != json!({"allow":false,"reason":"probe"}) {
        return Err("structured verdict disagrees with fixture; no authorization granted".into());
    }
    println!(
        "M3 stream, tool continuation and structured-output probe passed. Verdict usage: {:?}",
        verdict.usage
    );
    Ok(())
}
