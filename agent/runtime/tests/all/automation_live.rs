use std::time::Duration;

use tickloop_agent_model::{LocalModelClients, ModelClients};
use tickloop_agent_protocol::{Content, EventKind, InputModality, ModelConfig, Protocol, Role};
use tickloop_agent_runtime::Agent;

const LIVE_TIMEOUT: Duration = Duration::from_secs(180);

fn live_agent() -> Agent {
    let config = ModelConfig {
        provider: tickloop_agent_protocol::ModelProvider::Api,
        auth_source: tickloop_agent_protocol::ModelAuthSource::ApiKeyEnv,
        name: "DeepSeek automation validation".into(),
        protocol: Protocol::Messages,
        base_url: "https://api.deepseek.com/anthropic".into(),
        model: "deepseek-flash".into(),
        api_key_env: "DEEPSEEK_API_KEY".into(),
        max_output_tokens: 4096,
        provider_id: String::new(),
        provider_name: String::new(),
        model_id: String::new(),
        connection: None,
        card: None,
        input_modalities: vec![InputModality::Text],
    };
    Agent::new_embedded(
        config.clone(),
        LocalModelClients::new(None)
            .client(&config)
            .expect("valid DeepSeek configuration and credentials"),
    )
}

#[tokio::test]
#[ignore = "requires DeepSeek credentials and executes an explicitly approved temporary Shell command"]
async fn live_deepseek_reads_a_temporary_file_through_an_approved_shell_command() {
    let directory = tempfile::tempdir().unwrap();
    let marker = format!("shell-marker-{}", uuid::Uuid::new_v4());
    std::fs::write(directory.path().join("source.txt"), &marker).unwrap();
    let agent = live_agent();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let result = tokio::time::timeout(LIVE_TIMEOUT, async {
        agent.submit(chat, None, "Use the Bash tool with exactly the command `cat source.txt` in the current working directory. This temporary file belongs to this validation. Reply with its exact contents. Do not use any other tool or command.".into()).await.map_err(|error|error.to_string())?;
        let mut approved = false;
        let mut call = None;
        let mut observed_output = false;
        loop {
            match events.recv().await.map_err(|error| error.to_string())?.kind {
                EventKind::PermissionRequested { request } => {
                    if request.name != "Bash" || request.input["command"] != "cat source.txt" {
                        return Err("model proposed an operation outside this exact Shell validation".into());
                    }
                    agent.answer_permission(chat, request.id, true).await.map_err(|error|error.to_string())?;
                    approved = true;
                }
                EventKind::ToolStarted {call_id, name, ..} => {
                    if name != "Bash" {return Err("model used an unexpected tool".into());}
                    call = Some(call_id);
                }
                EventKind::ToolCompleted { call_id, result, .. } if call.as_ref()==Some(&call_id) => {
                    if result.is_error {return Err(format!("actual Shell failed: {}",result.content));}
                    if result.content != marker {return Err("Bash did not return the exact file contents".into());}
                    eprintln!("Actual Shell command: cat source.txt; exit code 0; output bytes {}",marker.len());
                    observed_output = true;
                }
                EventKind::Completed {usage} => {
                    eprintln!("DeepSeek Shell reply usage={usage:?}");
                    break;
                }
                EventKind::Failed(error) => return Err(error),
                _ => {}
            }
        }
        if !approved || !observed_output {return Err("the approved real Shell round trip did not occur".into());}
        let snapshot = agent.snapshot(chat).await.map_err(|error|error.to_string())?;
        if !snapshot.messages.iter().rev().find(|message|message.role==Role::Assistant).is_some_and(|message|message.content.iter().any(|content|matches!(content,Content::Text{text} if text.contains(&marker)))) {
            return Err("the final reply did not use the observed Shell output".into());
        }
        Ok::<(),String>(())
    }).await;
    agent.shutdown().await;
    result
        .expect("live Shell validation timed out")
        .expect("live Shell validation failed");
}
