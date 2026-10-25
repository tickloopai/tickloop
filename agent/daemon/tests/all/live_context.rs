use std::sync::Arc;

use tickloop_agent_protocol::{Content, EventKind, Usage};
use tickloop_agent_runtime::Agent;
use tickloop_agent_test_support::{ScriptedModel, next_event, text};
use uuid::Uuid;

async fn wait_for_completion(
    events: &mut tokio::sync::broadcast::Receiver<tickloop_agent_protocol::Event>,
) {
    loop {
        match next_event(events).await.kind {
            EventKind::Completed { .. } => return,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
}

#[tokio::test]
async fn context_status_tracks_the_active_window_and_drops_immediately_after_compaction() {
    let directory = tempfile::tempdir().unwrap();
    let client = Arc::new(ScriptedModel::with_usages(
        vec![
            text(&"older material ".repeat(10_000)),
            text("A compact summary"),
        ],
        vec![Usage {
            input_tokens: Some(40_000),
            output_tokens: Some(50_000),
            ..Usage::default()
        }],
    ));
    let agent = Agent::new_embedded(tickloop_agent_test_support::config(), client);
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent.set_context_window(chat, 1_000_000).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Collect the material".into())
        .await
        .unwrap();
    wait_for_completion(&mut events).await;
    let before = agent.status(chat).await.unwrap().context_tokens.unwrap();
    assert_eq!(
        before, 90_000,
        "current context includes measured input and the completed reply"
    );
    agent.compact(chat).await.unwrap();
    let after = agent.status(chat).await.unwrap().context_tokens.unwrap();
    assert!(
        after < before,
        "compaction must immediately reduce current context: {before} -> {after}"
    );
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(
        snapshot.messages.iter().any(|message| message
            .content
            .iter()
            .any(|content| matches!(content, Content::Text {text} if text.len() > 30_000))),
        "raw history is retained"
    );
    let execution = tickloop_agent_access::ExecutionRef {
        daemon_epoch: Uuid::new_v4(),
        agent_id: Uuid::new_v4(),
        generation: 1,
    };
    let live =
        tickloop_agent_access::LiveChatSnapshot::from_snapshot(execution, snapshot, Vec::new());
    let restored: tickloop_agent_access::LiveChatSnapshot =
        serde_json::from_value(serde_json::to_value(&live).unwrap()).unwrap();
    assert_eq!(live.status().chat.context_tokens, Some(after));
    assert_eq!(restored.status().chat.context_tokens, Some(after));
    let mut reopened = live.conversation.clone();
    reopened.request_usage = Default::default();
    let estimated = tickloop_agent_protocol::ChatStatus::of(&reopened)
        .context_tokens
        .unwrap();
    assert!(
        estimated > 0 && estimated < before,
        "restored history must exclude archived context: {estimated}"
    );
    agent.shutdown().await;
}
