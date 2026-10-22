//! An armed event Trigger under a durable host: a load keeps it armed and its Loop waiting, runs
//! nothing on its own, and the next explicit event starts a turn.

use std::sync::Arc;

use tickloop_agent_protocol::{LoopState, Trigger, TriggerPromptSource, TriggerSource};
use tickloop_agent_runtime::{ParentSelection, Quiescence, WaitHosting};
use tickloop_agent_test_support::text;
use tickloop_agent_test_support::transcripts::InMemoryTranscriptStore;
use tickloop_agent_transcript::SettledOutcome;
use uuid::Uuid;

use super::durable_queue::{Script, Step, harness, lost_holder, prompt, settled};

const EVENT_PROMPT: &str = "Report the new feed entry.";

#[tokio::test]
async fn a_durable_load_keeps_an_armed_event_trigger_waiting_for_its_next_event() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let first = harness(
        store.clone(),
        Script::new(vec![Step::Reply(text("watching"))]),
        WaitHosting::Durable,
    );
    let chat = first.create_chat_without_folder().await.unwrap();
    let request = prompt("watch the feed", ParentSelection::CurrentLeaf);
    let loop_id = first
        .advance(chat, request.clone())
        .await
        .unwrap()
        .receipt()
        .loop_id;
    assert!(matches!(
        settled(&first, chat, request.input_id).await,
        SettledOutcome::Completed { .. }
    ));
    first
        .configure_trigger(
            chat,
            loop_id,
            Some(Trigger {
                id: Uuid::new_v4(),
                prompt: EVENT_PROMPT.into(),
                prompt_source: TriggerPromptSource::Explicit,
                accepts_events: true,
                armed: true,
                subscriptions: Vec::new(),
            }),
        )
        .await
        .unwrap();
    let saved = lost_holder(&first, &store, chat).await;
    first.shutdown().await;

    let model = Script::new(vec![Step::Reply(text("reported"))]);
    let next = harness(saved, model.clone(), WaitHosting::Durable);
    next.resume_chat(chat).await.unwrap();
    assert!(model.requests().is_empty());
    assert!(matches!(next.quiesce(chat).await, Quiescence::Waiting(_)));
    let goal = next.snapshot(chat).await.unwrap().loops[0].clone();
    assert_eq!(goal.state, LoopState::WaitingTrigger);
    assert!(goal.trigger.unwrap().armed);

    next.trigger(
        chat,
        loop_id,
        TriggerSource {
            source: "feed".into(),
            payload: serde_json::json!({"entry": 1}),
        },
    )
    .await
    .unwrap();
    assert!(matches!(next.quiesce(chat).await, Quiescence::Waiting(_)));
    let requests = model.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].contains(EVENT_PROMPT), "{}", requests[0]);
    next.shutdown().await;
}
