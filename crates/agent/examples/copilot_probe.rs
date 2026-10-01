//! Opt-in real-account probe for GitHub Copilot. Sign-in uses your own device authorization of
//! the TickLoop Agent GitHub App; requests call only an inert Echo fixture and a structured
//! fixture. Tokens stay in STATE_DIRECTORY/auth and are never printed.
//!
//! usage: copilot_probe login|models|echo|logout STATE_DIRECTORY [MODEL_CARD_ID]

use std::{error::Error, os::unix::fs::PermissionsExt, path::PathBuf};

use serde_json::{Value, json};
use tickloop_agent::{
    auth::copilot::CopilotAuth,
    model::{
        Content, Message, ModelRequest, Role, StructuredOutput, ToolDefinition,
        access::AccessCatalog,
        cards::ModelCatalog,
        copilot::{COPILOT_API_URL, GITHUB_API_VERSION, USER_AGENT},
        model_client,
    },
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const PROVIDER: &str = "github-copilot";

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if !(2..=3).contains(&arguments.len()) {
        return Err(
            "usage: copilot_probe login|models|echo|logout STATE_DIRECTORY [MODEL_CARD_ID]".into(),
        );
    }
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
    let auth = CopilotAuth::new(&root);
    match arguments[0].as_str() {
        "login" => {
            let login = auth.start_device_login(&cancel).await?;
            println!(
                "Open {} on your own device and enter {}. GitHub shows the TickLoop Agent app by TickLoopAI.",
                login.verification_uri, login.user_code
            );
            auth.finish_device_login(login, &cancel).await?;
            println!(
                "GitHub Copilot sign-in saved in {}",
                root.join("auth").display()
            );
        }
        "logout" => {
            auth.logout(&cancel).await?;
            println!(
                "Local GitHub Copilot sign-in removed; revoke the app in GitHub settings if needed."
            );
        }
        "models" => summarize_models(&auth, &cancel).await?,
        "echo" => {
            let model = arguments.get(2).map_or_else(
                || {
                    AccessCatalog::defaults()
                        .recommended_model(PROVIDER)
                        .expect("Copilot has a recommended model")
                        .to_owned()
                },
                Clone::clone,
            );
            echo(&root, &model, &cancel).await?;
        }
        _ => return Err("command must be login, models, echo or logout".into()),
    }
    Ok(())
}

/// Prints only model metadata needed to check cards and bindings; no token or account data.
async fn summarize_models(
    auth: &CopilotAuth,
    cancel: &CancellationToken,
) -> Result<(), Box<dyn Error>> {
    let credentials = auth.resolve_credentials(cancel).await?;
    let response = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?
        .get(format!("{COPILOT_API_URL}/models"))
        .bearer_auth(&credentials.access_token)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .header("x-github-api-version", GITHUB_API_VERSION)
        .send()
        .await?;
    println!("GET /models → HTTP {}", response.status().as_u16());
    let listing: Value = response.json().await?;
    let bound = AccessCatalog::defaults()
        .provider(PROVIDER)
        .ok_or("Copilot access catalog missing")?
        .series
        .iter()
        .filter_map(|series| {
            ModelCatalog::defaults()
                .get(PROVIDER, &series.model)
                .map(|card| (series.label(), card.model, card.protocol))
        })
        .collect::<Vec<_>>();
    let entries = listing["data"].as_array().cloned().unwrap_or_default();
    for (label, service_id, protocol) in &bound {
        match entries
            .iter()
            .find(|entry| entry["id"] == service_id.as_str())
        {
            Some(entry) => println!(
                "{label}: {service_id} listed · policy={} picker={} endpoints={} card={protocol:?} limits={} supports={}",
                entry["policy"]["state"],
                entry["model_picker_enabled"],
                entry["supported_endpoints"],
                entry["capabilities"]["limits"],
                entry["capabilities"]["supports"],
            ),
            None => println!("{label}: {service_id} not listed"),
        }
    }
    let unbound = entries
        .iter()
        .filter_map(|entry| entry["id"].as_str())
        .filter(|id| bound.iter().all(|(_, service_id, _)| service_id != id))
        .collect::<Vec<_>>();
    println!("Other listed ids (not offered by Tick Loop): {unbound:?}");
    Ok(())
}

async fn echo(
    root: &std::path::Path,
    model: &str,
    cancel: &CancellationToken,
) -> Result<(), Box<dyn Error>> {
    let config = ModelCatalog::defaults()
        .get(PROVIDER, model)
        .ok_or("unknown Copilot model card")?;
    println!(
        "Probing {} ({}) via {:?}",
        config.name, config.model, config.protocol
    );
    let client = model_client(config, Some(root))?;
    let (events, _receiver) = mpsc::unbounded_channel();
    let mut request = ModelRequest {
        history_sources: vec![None],
        messages: vec![Message {
            role: Role::User,
            content: vec![Content::Text {
                text: r#"This is an inert protocol test. Call the actual Echo tool exactly once with input exactly {"text":"hello"}. Do not describe or simulate the call. After the real tool result, report it briefly."#.into(),
            }],
        }],
        tools: vec![ToolDefinition {
            name: "Echo".into(),
            description: "Returns the text unchanged; no files, shell or external actions.".into(),
            input_schema: json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false}),
        }],
        structured_output: None,
        temperature: None,
        reasoning_effort: None,
    };
    let first = client
        .complete(request.clone(), events.clone(), cancel.clone())
        .await?;
    println!("Tool turn usage: {:?}", first.usage);
    let mut results = Vec::new();
    for content in &first.content {
        if let Content::ToolUse { id, name, input } = content {
            if name != "Echo" || input["text"] != "hello" || !results.is_empty() {
                return Err("unexpected probe tool invocation; nothing executed".into());
            }
            results.push(Content::ToolResult {
                tool_use_id: id.clone(),
                content: "hello".into(),
                is_error: false,
            });
        }
    }
    if results.len() != 1 {
        return Err(format!(
            "model did not call the probe tool (stop={})",
            first.stop_reason
        )
        .into());
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
    let verdict = client
        .complete(
            ModelRequest {
                history_sources: vec![None],
                messages: vec![Message {
                    role: Role::User,
                    content: vec![Content::Text {
                        text: "Return the structured verdict with allow=false and reason=probe. This is a protocol test, not authorization.".into(),
                    }],
                }],
                tools: Vec::new(),
                structured_output: Some(StructuredOutput {
                    name: "probe_verdict".into(),
                    schema: json!({"type":"object","properties":{"allow":{"type":"boolean"},"reason":{"type":"string"}},"required":["allow","reason"],"additionalProperties":false}),
                }),
                temperature: None,
                reasoning_effort: None,
            },
            events,
            cancel.clone(),
        )
        .await?;
    let text = verdict
        .content
        .iter()
        .filter_map(|block| match block {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    let value: Value = serde_json::from_str(text.trim())?;
    if value != json!({"allow":false,"reason":"probe"}) {
        return Err("structured verdict disagrees with fixture; no authorization granted".into());
    }
    println!(
        "Copilot stream, tool continuation and structured-output probe passed. Verdict usage: {:?}",
        verdict.usage
    );
    Ok(())
}
