//! Opt-in real-account probe for GitHub Copilot. Sign-in uses your own device authorization of
//! GitHub's Copilot app (the client VS Code uses, as pi does); requests call only an inert Echo
//! fixture and a structured fixture. Tokens stay in STATE_DIRECTORY/auth and are never printed.
//!
//! usage: copilot_probe login|models|events|replay|echo|logout STATE_DIRECTORY [MODEL_CARD_ID|BODY_JSON] [EFFORT]

use std::{error::Error, os::unix::fs::PermissionsExt, path::PathBuf};

use serde_json::{Value, json};
use tickloop_agent::{
    auth::copilot::CopilotAuth,
    model::{
        Content, Message, ModelRequest, Role, StructuredOutput, ToolDefinition,
        access::AccessCatalog,
        cards::ModelCatalog,
        copilot::{CLIENT_HEADERS, COPILOT_API_URL, GITHUB_API_VERSION},
        model_client,
    },
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const PROVIDER: &str = "github-copilot";

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if !(2..=4).contains(&arguments.len()) {
        return Err(
            "usage: copilot_probe login|models|events|replay|echo|logout STATE_DIRECTORY [MODEL_CARD_ID|BODY_JSON] [EFFORT]"
                .into(),
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
                "Open {} on your own device and enter {}. {}",
                login.verification_uri,
                login.user_code,
                tickloop_agent::auth::copilot::CONSENT_NOTICE
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
        "replay" => {
            let body =
                std::fs::read_to_string(arguments.get(2).ok_or("replay needs a JSON body file")?)?;
            replay_messages(&auth, serde_json::from_str(&body)?, &cancel).await?;
        }
        "events" => {
            let model = arguments.get(2).map_or("gpt-6-luna", String::as_str);
            summarize_response_events(&auth, model, &cancel).await?;
        }
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
            let effort = arguments
                .get(3)
                .map(|effort| serde_json::from_value(json!(effort)))
                .transpose()?;
            echo(&root, &model, effort, &cancel).await?;
        }
        _ => return Err("command must be login, models, echo or logout".into()),
    }
    Ok(())
}

/// Sends a recorded Messages body once and prints only the status and the service's error
/// description; no token.
async fn replay_messages(
    auth: &CopilotAuth,
    body: Value,
    cancel: &CancellationToken,
) -> Result<(), Box<dyn Error>> {
    let access = auth.resolve_access(cancel).await?;
    let base = access.api_base.as_deref().unwrap_or(COPILOT_API_URL);
    let mut request = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?
        .post(format!("{base}/v1/messages"))
        .bearer_auth(&access.token)
        .header("anthropic-version", "2023-06-01")
        .header("openai-intent", "conversation-edits")
        .header("x-initiator", "user")
        .json(&body);
    for (name, value) in CLIENT_HEADERS {
        request = request.header(name, value);
    }
    let response = request.send().await?;
    let status = response.status().as_u16();
    let text = response.text().await?;
    println!("POST {base}/v1/messages → HTTP {status}");
    if status >= 400 {
        let error: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        println!("error: {}", error["error"]);
    }
    Ok(())
}

/// Streams one minimal Responses request and prints each event's type, output index and whether
/// its item id matches the id announced by `response.output_item.added`; no text or token.
async fn summarize_response_events(
    auth: &CopilotAuth,
    model: &str,
    cancel: &CancellationToken,
) -> Result<(), Box<dyn Error>> {
    let access = auth.resolve_access(cancel).await?;
    let base = access.api_base.as_deref().unwrap_or(COPILOT_API_URL);
    let mut request = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?
        .post(format!("{base}/responses"))
        .bearer_auth(&access.token)
        .header("openai-intent", "conversation-edits")
        .header("x-initiator", "user")
        .json(&json!({"model":model,"input":"Call the Echo tool once with text hello.","stream":true,"store":false,"max_output_tokens":2048,"tools":[{"type":"function","name":"Echo","description":"Returns the text unchanged.","parameters":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}}]}));
    for (name, value) in CLIENT_HEADERS {
        request = request.header(name, value);
    }
    let mut response = request.send().await?;
    println!(
        "POST {base}/responses → HTTP {}",
        response.status().as_u16()
    );
    let mut announced = std::collections::BTreeMap::new();
    let mut done_items = std::collections::BTreeMap::new();
    let mut buffer = String::new();
    while let Some(chunk) = response.chunk().await? {
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(end) = buffer.find("\n\n") {
            let frame = buffer[..end].to_owned();
            buffer.drain(..end + 2);
            let Some(data) = frame.lines().find_map(|line| line.strip_prefix("data: ")) else {
                continue;
            };
            let Ok(event) = serde_json::from_str::<Value>(data) else {
                continue;
            };
            let index = event["output_index"].as_u64();
            let id = event["item_id"]
                .as_str()
                .or_else(|| event["item"]["id"].as_str());
            if event["type"] == "response.output_item.added" {
                announced.insert(index, id.map(str::to_owned));
            }
            if event["type"] == "response.output_item.done" {
                done_items.insert(index, event["item"].clone());
            }
            let same_id = id.map(|id| announced.get(&index).and_then(Option::as_deref) == Some(id));
            println!(
                "{} index={index:?} item_type={} id_len={:?} same_as_added={same_id:?}",
                event["type"].as_str().unwrap_or("?"),
                event["item"]["type"].as_str().unwrap_or("-"),
                id.map(str::len),
            );
            if event["type"] == "response.completed" {
                for (position, item) in event["response"]["output"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    let id = item["id"].as_str();
                    println!(
                        "  completed output[{position}] type={} same_as_added={:?} differing_from_done={:?}",
                        item["type"].as_str().unwrap_or("?"),
                        id.map(|id| announced
                            .get(&Some(position as u64))
                            .and_then(Option::as_deref)
                            == Some(id)),
                        done_items
                            .get(&Some(position as u64))
                            .map(|done| differing_fields(done, item, ""))
                    );
                }
            }
        }
    }
    Ok(())
}

/// Paths of fields whose values differ between two JSON values; values themselves stay hidden.
fn differing_fields(left: &Value, right: &Value, path: &str) -> Vec<String> {
    match (left, right) {
        (Value::Object(left), Value::Object(right)) => {
            let mut keys = left.keys().chain(right.keys()).collect::<Vec<_>>();
            keys.sort();
            keys.dedup();
            keys.into_iter()
                .flat_map(|key| {
                    differing_fields(
                        left.get(key).unwrap_or(&Value::Null),
                        right.get(key).unwrap_or(&Value::Null),
                        &format!("{path}.{key}"),
                    )
                })
                .collect()
        }
        (Value::Array(left), Value::Array(right)) if left.len() == right.len() => left
            .iter()
            .zip(right)
            .enumerate()
            .flat_map(|(index, (left, right))| {
                differing_fields(left, right, &format!("{path}[{index}]"))
            })
            .collect(),
        _ if left == right => Vec::new(),
        _ => vec![path.to_owned()],
    }
}

/// Prints only model metadata needed to check cards and bindings; no token or account data.
async fn summarize_models(
    auth: &CopilotAuth,
    cancel: &CancellationToken,
) -> Result<(), Box<dyn Error>> {
    let access = auth.resolve_access(cancel).await?;
    let base = access.api_base.as_deref().unwrap_or(COPILOT_API_URL);
    let mut request = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?
        .get(format!("{base}/models"))
        .bearer_auth(&access.token)
        .header("x-github-api-version", GITHUB_API_VERSION);
    for (name, value) in CLIENT_HEADERS {
        request = request.header(name, value);
    }
    let response = request.send().await?;
    println!("GET {base}/models → HTTP {}", response.status().as_u16());
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
    effort: Option<tickloop_agent::model::ReasoningEffort>,
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
        reasoning_effort: effort,
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
