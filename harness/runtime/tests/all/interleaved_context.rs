use std::sync::Arc;
use tickloop_harness_protocol::{Content, EventKind, ModelChange, Receipt};
use tickloop_harness_runtime::Agent;
use tickloop_harness_test_support::harness::{ChosenModels, embedded_agent};
use tickloop_harness_test_support::{ScriptedModel, config, next_event, text};

async fn completed(
    agent: &Agent,
    events: &mut tokio::sync::broadcast::Receiver<tickloop_harness_protocol::Event>,
    receipt: &Receipt,
) {
    loop {
        let event = next_event(events).await;
        match event.kind {
            EventKind::Completed { .. } if event.run_id == receipt.run_id => return,
            EventKind::Failed(error) => panic!(
                "{error}; snapshot: {:?}",
                agent.snapshot(receipt.chat_id).await.unwrap().chat_loop
            ),
            _ => {}
        }
    }
}

#[tokio::test]
async fn raw_legacy_model_name_change_keeps_same_source_private_history() {
    let directory = tempfile::tempdir().unwrap();
    let raw = config();
    assert!(raw.card.is_none());
    let private = Content::Thinking {
        thinking: "private reasoning from the first name".into(),
        signature: Some("signed-private-state".into()),
    };
    let first_client = Arc::new(ScriptedModel::new(vec![vec![
        private.clone(),
        Content::Text {
            text: "First answer".into(),
        },
    ]]));
    let models = ChosenModels::new(first_client);
    let agent = embedded_agent(raw.clone(), models.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let first = agent.submit(chat, "First prompt".into()).await.unwrap();
    completed(&agent, &mut events, &first).await;
    let initial = agent.snapshot(chat).await.unwrap();
    assert!(initial.model.card.is_some());
    assert!(!initial.model.provider_id.is_empty());

    let mut renamed = raw;
    renamed.name = "renamed fixture".into();
    let second_client = Arc::new(ScriptedModel::new(vec![text("Second answer")]));
    models.choose(second_client.clone());
    assert_eq!(
        agent.set_model(chat, renamed).await.unwrap(),
        ModelChange::Applied
    );
    let selected = agent.snapshot(chat).await.unwrap();
    assert_eq!(selected.model.name, "renamed fixture");
    assert_eq!(selected.model.provider_id, initial.model.provider_id);
    assert!(
        selected
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .any(|content| *content == private)
    );

    let next = agent
        .submit(chat, "Continue after the name change".into())
        .await
        .unwrap();
    completed(&agent, &mut events, &next).await;
    assert!(
        second_client.requests.lock().unwrap()[0]
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .any(|content| *content == private)
    );
    agent.shutdown().await;
}
