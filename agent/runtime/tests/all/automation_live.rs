use std::time::Duration;

use tickloop_agent_model::{LocalModelClients, ModelClients};
use tickloop_agent_protocol::{
    Content, EventKind, InputModality, LoopState, ModelConfig, Protocol, Role, Trigger,
    TriggerPromptSource,
};
use tickloop_agent_runtime::Agent;

const LIVE_TIMEOUT: Duration = Duration::from_secs(180);
const INTERVAL_SECONDS: u64 = 10;

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

#[tokio::test]
#[ignore = "requires DeepSeek credentials and a real local timer; waits for a scheduled model/tool run and verifies stopping"]
async fn live_deepseek_interval_reuses_the_goal_and_stops_future_execution() {
    let directory = tempfile::tempdir().unwrap();
    let marker = format!("timer-marker-{}", uuid::Uuid::new_v4());
    std::fs::write(directory.path().join("tick-source.txt"), &marker).unwrap();
    let agent = live_agent();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let result = tokio::time::timeout(LIVE_TIMEOUT, async {
        let initial = agent.submit(chat,None,"Prepare to check a local file on a later timer. For this initial reply, say READY only and call no tools.".into()).await.map_err(|error|error.to_string())?;
        loop {
            match events.recv().await.map_err(|error|error.to_string())?.kind {
                EventKind::Completed {usage} => {eprintln!("Timer setup reply usage={usage:?}");break;}
                EventKind::ToolStarted {..}|EventKind::PermissionRequested {..} => return Err("initial timer setup unexpectedly used a tool".into()),
                EventKind::Failed(error) => return Err(error),
                _ => {}
            }
        }
        agent.configure_trigger(chat,initial.loop_id,Some(Trigger { title: None, paused_at_ms: None,
            id:uuid::Uuid::new_v4(),
            prompt:"Use Read to read tick-source.txt in the current working directory, then reply with the exact marker from that file. Do not use other tools.".into(),
            prompt_source: TriggerPromptSource::Explicit,
            schedule:Some(tickloop_agent_protocol::Recurrence::parse(&format!("every {INTERVAL_SECONDS}s")).unwrap()),
            armed_at_ms:None,
            last_fired_at_ms:None,
            confirmation: None,
            expires_at_ms:None,
            accepts_events:false,
            armed:true,
            subscriptions:Vec::new(),
        })).await.map_err(|error|error.to_string())?;
        let mut timed_run = None;
        let mut timed_answer = String::new();
        let mut read_call = None;
        let mut saw_actual_file = false;
        loop {
            let event = events.recv().await.map_err(|error|error.to_string())?;
            if event.chat_id!=chat || event.loop_id!=initial.loop_id {return Err("scheduled events changed Chat or goal identity".into());}
            match event.kind {
                EventKind::Started if event.run_id!=initial.run_id => {
                    eprintln!("Actual timer started goal {} with run {}",event.loop_id,event.run_id);
                    timed_run=Some(event.run_id);
                    timed_answer.clear();
                },
                EventKind::Text(text) if Some(event.run_id)==timed_run => timed_answer.push_str(&text),
                EventKind::ToolStarted {call_id,name,..} => {
                    if name!="Read" {return Err("timer used an unexpected tool".into());}
                    eprintln!("Timed run requested its actual Read operation.");
                    read_call=Some(call_id);
                }
                EventKind::ToolCompleted { call_id, result, .. } if read_call.as_ref()==Some(&call_id) => {
                    if result.is_error || !result.content.contains(&marker) {return Err("timer did not read the actual temporary file".into());}
                    saw_actual_file=true;
                }
                EventKind::Completed {usage} if Some(event.run_id)==timed_run => {
                    eprintln!("Actual interval {INTERVAL_SECONDS}s: same goal, new run, real file read; reply usage={usage:?}");
                    break;
                }
                EventKind::Failed(error) => return Err(error),
                EventKind::PermissionRequested {..} => return Err("ordinary timer file read unexpectedly required approval".into()),
                _ => {}
            }
        }
        if timed_run.is_none() || !saw_actual_file {return Err("a real timer-driven model/tool run was not observed".into());}
        let before_stop = agent.snapshot(chat).await.map_err(|error|error.to_string())?;
        if before_stop.loops.len()!=1 || !timed_answer.contains(&marker) {return Err("scheduled reply did not retain its goal and actual file marker".into());}
        agent.stop(chat,initial.loop_id).await.map_err(|error|error.to_string())?;
        let stopped = agent.snapshot(chat).await.map_err(|error|error.to_string())?;
        if stopped.loops[0].state!=LoopState::Stopped {return Err("stop did not stop the scheduled goal".into());}
        tokio::time::sleep(Duration::from_secs(INTERVAL_SECONDS+1)).await;
        let later = agent.observe(chat,stopped.last_sequence).await.map_err(|error|error.to_string())?;
        if later.events.iter().any(|event|matches!(event.kind,EventKind::Started|EventKind::ToolStarted {..})) {
            return Err("stopped timer launched additional model or tool work".into());
        }
        eprintln!("No model or tool start during {} seconds after stop; shutting down the host.",INTERVAL_SECONDS+1);
        Ok::<(),String>(())
    }).await;
    agent.shutdown().await;
    result
        .expect("live timer validation timed out")
        .expect("live timer validation failed");
}
