use std::{sync::Arc, time::Duration};
use tickloop_agent::{
    model::{Content, HttpModelClient, InputModality, ModelConfig, Protocol},
    runtime::{Agent, EventKind},
};

async fn verify_deepseek_file_round_trip(protocol: Protocol) {
    assert!(
        std::env::var_os("DEEPSEEK_API_KEY").is_some(),
        "DEEPSEEK_API_KEY is required for live validation"
    );
    let directory = tempfile::tempdir().unwrap();
    let marker = format!("tickloop-{}", uuid::Uuid::new_v4());
    std::fs::write(
        directory.path().join("probe.txt"),
        format!("validation_marker={marker}\n"),
    )
    .unwrap();
    let config = ModelConfig {
        provider: tickloop_agent::model::ModelProvider::Api,
        auth_source: tickloop_agent::model::ModelAuthSource::ApiKeyEnv,
        name: format!("DeepSeek live {protocol:?}"),
        protocol,
        base_url: if protocol == Protocol::Messages {
            "https://api.deepseek.com/anthropic"
        } else {
            "https://api.deepseek.com"
        }
        .into(),
        model: "deepseek-flash".into(),
        api_key_env: "DEEPSEEK_API_KEY".into(),
        max_output_tokens: 4096,
        provider_id: String::new(),
        provider_name: String::new(),
        model_id: String::new(),
        card: None,
        input_modalities: vec![InputModality::Text],
    };
    let client = HttpModelClient::new(config.clone()).unwrap();
    let agent = Agent::new_embedded(config, Arc::new(client));
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt=agent.submit(chat,None,"Use Read to read probe.txt in the current working directory. Reply with exactly the validation_marker value found inside. Use no other tool.".into()).await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(300), async {
        let mut observed_read = false;
        loop {
            match events.recv().await.map_err(|error| error.to_string())?.kind {
                EventKind::ToolCompleted { result, .. } => {
                    if result.is_error {
                        return Err(format!("live tool failed: {}", result.content));
                    }
                    observed_read |= result.content.contains(&marker);
                }
                EventKind::Completed { usage } => {
                    eprintln!("{protocol:?}: file round trip completed, usage={usage:?}");
                    break;
                }
                EventKind::Failed(error) => return Err(error),
                EventKind::PermissionRequested { .. } => {
                    return Err(
                        "unexpected permission request for ordinary local file reading".into(),
                    );
                }
                _ => {}
            }
        }
        if !observed_read {
            return Err("model did not obtain the real local file through the tool".into());
        }
        let snapshot = agent
            .snapshot(chat)
            .await
            .map_err(|error| error.to_string())?;
        if !snapshot
            .messages
            .iter()
            .rev()
            .find(|message| message.role == tickloop_agent::model::Role::Assistant)
            .is_some_and(|message| {
                message
                    .content
                    .iter()
                    .any(|content| matches!(content,Content::Text{text} if text.contains(&marker)))
            })
        {
            return Err("answer did not use the marker returned by the tool".into());
        }
        agent
            .submit(
                chat,
                Some(receipt.loop_id),
                "Repeat the exact validation_marker from our previous turn. Do not call any tools."
                    .into(),
            )
            .await
            .map_err(|error| error.to_string())?;
        let mut answer = String::new();
        loop {
            match events.recv().await.map_err(|error| error.to_string())?.kind {
                EventKind::Text(text) => answer.push_str(&text),
                EventKind::Completed { usage } => {
                    eprintln!("{protocol:?}: follow-up completed, usage={usage:?}");
                    break;
                }
                EventKind::Failed(error) => return Err(error),
                _ => {}
            }
        }
        if !answer.contains(&marker) {
            return Err("follow-up did not preserve conversation history".into());
        }
        Ok::<(), String>(())
    })
    .await;
    agent.shutdown().await;
    result
        .expect("live verification timed out")
        .expect("live verification failed");
}

#[tokio::test]
#[ignore = "requires DEEPSEEK_API_KEY and calls the real billed API"]
async fn live_deepseek_messages_file_and_follow_up() {
    verify_deepseek_file_round_trip(Protocol::Messages).await;
}

#[tokio::test]
#[ignore = "requires DEEPSEEK_API_KEY and calls the real billed API"]
async fn live_deepseek_responses_file_and_follow_up() {
    verify_deepseek_file_round_trip(Protocol::Responses).await;
}

#[tokio::test]
#[ignore = "requires DEEPSEEK_API_KEY and calls the real billed API"]
async fn live_deepseek_chat_completions_file_and_follow_up() {
    verify_deepseek_file_round_trip(Protocol::ChatCompletions).await;
}

/// Whether DeepSeek's Messages endpoint reads an image nested in a tool_result: the model can
/// only name the swatch's colour if it saw the image that `Read` returned.
#[tokio::test]
#[ignore = "requires DEEPSEEK_API_KEY and calls the real billed API"]
async fn live_deepseek_messages_reads_images_in_tool_results() {
    assert!(
        std::env::var_os("DEEPSEEK_API_KEY").is_some(),
        "DEEPSEEK_API_KEY is required for live validation"
    );
    let directory = tempfile::tempdir().unwrap();
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
        256,
        256,
        image::Rgb([0, 64, 255]),
    ))
    .save(directory.path().join("swatch.png"))
    .unwrap();
    let config = ModelConfig {
        name: "DeepSeek live images".into(),
        protocol: Protocol::Messages,
        base_url: "https://api.deepseek.com/anthropic".into(),
        model: "deepseek-flash".into(),
        api_key_env: "DEEPSEEK_API_KEY".into(),
        max_output_tokens: 4096,
        provider: tickloop_agent::model::ModelProvider::Api,
        auth_source: tickloop_agent::model::ModelAuthSource::ApiKeyEnv,
        provider_id: String::new(),
        provider_name: String::new(),
        model_id: String::new(),
        card: None,
        input_modalities: vec![InputModality::Text, InputModality::Image],
    };
    let agent = Agent::new_embedded(
        config.clone(),
        Arc::new(HttpModelClient::new(config).unwrap()),
    );
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, None, "Use the Read tool to read swatch.png in the current working directory, then reply with the single English name of its colour. Use no other tool.".into()).await.unwrap();
    let answer = tokio::time::timeout(Duration::from_secs(300), async {
        let mut answer = String::new();
        let mut read_image = false;
        loop {
            match events.recv().await.unwrap().kind {
                EventKind::ToolCompleted { result, .. } => {
                    assert!(!result.is_error, "{}", result.content);
                    read_image |= result.content.contains("image/png");
                }
                EventKind::Text(text) => answer.push_str(&text),
                EventKind::Completed { usage } => {
                    eprintln!("Messages tool_result image answered {answer:?}, usage={usage:?}");
                    assert!(read_image, "the model did not read the image");
                    return answer;
                }
                EventKind::Failed(error) => panic!("tool_result image was rejected: {error}"),
                _ => {}
            }
        }
    })
    .await
    .expect("live image verification timed out");
    agent.shutdown().await;
    assert!(
        answer.to_lowercase().contains("blue"),
        "the model did not see the image: {answer}"
    );
}

#[tokio::test]
#[ignore = "requires DEEPSEEK_API_KEY and calls the real billed API"]
async fn live_deepseek_switches_protocols_after_public_compaction() {
    let directory = tempfile::tempdir().unwrap();
    let marker = format!("marker-{}", uuid::Uuid::new_v4());
    let profile = |protocol| ModelConfig {
        provider: tickloop_agent::model::ModelProvider::Api,
        auth_source: tickloop_agent::model::ModelAuthSource::ApiKeyEnv,
        name: format!("DeepSeek {protocol:?}"),
        protocol,
        base_url: if protocol == Protocol::Messages {
            "https://api.deepseek.com/anthropic"
        } else {
            "https://api.deepseek.com"
        }
        .into(),
        model: "deepseek-flash".into(),
        api_key_env: "DEEPSEEK_API_KEY".into(),
        max_output_tokens: 4096,
        provider_id: String::new(),
        provider_name: String::new(),
        model_id: String::new(),
        card: None,
        input_modalities: vec![InputModality::Text],
    };
    let config = profile(Protocol::Messages);
    let agent = Agent::new_embedded(
        config.clone(),
        Arc::new(HttpModelClient::new(config).unwrap()),
    );
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let mut goal = None;
    for protocol in [
        Protocol::Messages,
        Protocol::Responses,
        Protocol::ChatCompletions,
    ] {
        if goal.is_some() {
            let raw = agent.snapshot(chat).await.unwrap().messages;
            if let Err(error) = agent.compact(chat).await {
                assert_eq!(agent.snapshot(chat).await.unwrap().messages, raw);
                assert!(
                    agent
                        .snapshot(chat)
                        .await
                        .unwrap()
                        .events
                        .iter()
                        .any(|event| matches!(
                            event.kind,
                            tickloop_agent::runtime::EventKind::ContextCompaction {
                                status: tickloop_agent::runtime::CompactionStatus::Failed,
                                ..
                            }
                        ))
                );
                eprintln!(
                    "Compaction failed without changing raw history: {error}. The validation driver explicitly retries once, representing a user's manual retry."
                );
                agent
                    .compact(chat)
                    .await
                    .expect("manual compaction retry failed");
            }
            assert_eq!(agent.snapshot(chat).await.unwrap().messages, raw);
            let config = profile(protocol);
            agent
                .set_model(
                    chat,
                    config.clone(),
                    Arc::new(HttpModelClient::new(config).unwrap()),
                )
                .await
                .unwrap();
        }
        let input = if goal.is_none() {
            format!(
                "Remember this exact validation marker: {marker}. Reply with just that marker; use no tools."
            )
        } else {
            "Repeat the exact validation marker from the previous conversation. Use no tools."
                .into()
        };
        goal = Some(agent.submit(chat, goal, input).await.unwrap().loop_id);
        let answer = tokio::time::timeout(Duration::from_secs(180), async {
            let mut answer = String::new();
            loop {
                match events.recv().await.unwrap().kind {
                    EventKind::Text(text) => answer.push_str(&text),
                    EventKind::Completed { usage } => {
                        eprintln!("Switched to {protocol:?}, actual usage={usage:?}");
                        return answer;
                    }
                    EventKind::Failed(error) => panic!("{error}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("live switch timed out");
        assert!(answer.contains(&marker));
    }
    assert_eq!(agent.snapshot(chat).await.unwrap().loops.len(), 1);
    agent.shutdown().await;
}

#[tokio::test]
#[ignore = "requires DeepSeek credentials and real public search and web services"]
async fn live_deepseek_searches_and_reads_official_documentation() {
    let directory = tempfile::tempdir().unwrap();
    let config = ModelConfig {
        provider: tickloop_agent::model::ModelProvider::Api,
        auth_source: tickloop_agent::model::ModelAuthSource::ApiKeyEnv,
        name: "DeepSeek live web".into(),
        protocol: Protocol::Messages,
        base_url: "https://api.deepseek.com/anthropic".into(),
        model: "deepseek-flash".into(),
        api_key_env: "DEEPSEEK_API_KEY".into(),
        max_output_tokens: 4096,
        provider_id: String::new(),
        provider_name: String::new(),
        model_id: String::new(),
        card: None,
        input_modalities: vec![InputModality::Text],
    };
    let agent = Agent::new_embedded(
        config.clone(),
        Arc::new(HttpModelClient::new(config).unwrap()),
    );
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_search(
            chat,
            Some(tickloop_agent::tools::SearchConfig {
                endpoint: "https://api.mwmbl.org/api/v2/search/".into(),
                api_key_env: None,
            }),
        )
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, None, "Search the web for 'rust documentation' using WebSearch, then use WebFetch to read https://doc.rust-lang.org/stable/. Summarize one official documentation resource and cite its source URL. Both real tools are required; do not use Shell or files.".into()).await.unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(180), async {
        let mut completed = std::collections::HashMap::new();
        let mut names = std::collections::HashMap::new();
        loop {
            match events.recv().await.unwrap().kind {
                EventKind::PermissionRequested {request} => {
                    assert!(matches!(request.name.as_str(), "WebSearch" | "WebFetch"));
                    agent.answer_permission(chat, request.id, true).await.unwrap();
                }
                EventKind::ToolStarted {call_id, name, ..} => { names.insert(call_id, name); }
                EventKind::ToolCompleted {call_id, result} => {
                    assert!(!result.is_error, "{}", result.content);
                    let name = names.remove(&call_id).unwrap();
                    if name == "WebSearch" {
                        assert!(result.content.contains("https://"));
                    }
                    eprintln!("Live {name}: {} bytes of actual tool result", result.content.len());
                    completed.insert(name, result.content);
                }
                EventKind::Completed {usage} => {
                    assert!(completed.contains_key("WebSearch"));
                    assert!(completed.contains_key("WebFetch"));
                    eprintln!("DeepSeek search + fetch completed, usage={usage:?}");
                    break;
                }
                EventKind::Failed(error) => panic!("live web round failed: {error}"),
                _ => {}
            }
        }
        let snapshot = agent.snapshot(chat).await.unwrap();
        let answer = snapshot.messages.last().unwrap();
        assert!(answer.content.iter().any(|content| matches!(content, Content::Text {text} if text.contains("https://doc.rust-lang.org"))));
    }).await;
    agent.shutdown().await;
    outcome.expect("live web verification timed out");
}
