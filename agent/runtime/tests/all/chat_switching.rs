use std::{sync::Arc, time::Duration};

use tickloop_agent_model::ModelRequest;
use tickloop_agent_protocol::{
    BranchRequest, ChatId, EventKind, Receipt, ShellTaskState, TickSubscriptionChange,
};
use tickloop_agent_runtime::{Agent, AgentError, ShellTaskCenter};
use tickloop_agent_test_support::shell_ticks::ShellTick;
use tickloop_agent_test_support::{ScriptedModel, config, next_event, text, transcript_file};
use uuid::Uuid;

async fn complete_turn(agent: &Agent, chat: ChatId, prompt: &str) -> Receipt {
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, prompt.into()).await.unwrap();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != receipt.run_id {
            continue;
        }
        match event.kind {
            EventKind::Completed { .. } => return receipt,
            EventKind::Failed(error) => panic!("turn failed: {error}"),
            _ => {}
        }
    }
}

fn request_text(request: &ModelRequest) -> String {
    serde_json::to_string(&request.messages).unwrap()
}

fn conversation_text(agent_snapshot: &tickloop_agent_protocol::ChatSnapshot) -> String {
    serde_json::to_string(&agent_snapshot.messages).unwrap()
}

#[tokio::test]
async fn forking_a_saved_chat_writes_a_new_transcript_and_leaves_the_source_unchanged() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let agent = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![text("Saved the word cedar.")])),
    )
    .unwrap();
    let source = agent.create_chat(directory.path().into()).await.unwrap();
    complete_turn(&agent, source, "Remember cedar.").await;
    agent.shutdown().await;
    drop(agent);
    let source_path = transcript_file(&root, source);
    let source_bytes = std::fs::read(&source_path).unwrap();

    let model = Arc::new(ScriptedModel::new(vec![text("The word was cedar.")]));
    let copying = Agent::open_embedded(root.clone(), config(), model.clone()).unwrap();
    assert!(matches!(
        copying.resume_as_new_chat(Uuid::new_v4()).await,
        Err(AgentError::NotFound)
    ));
    let copy = copying.resume_as_new_chat(source).await.unwrap();
    assert_ne!(copy.id, source);
    assert!(conversation_text(&copy).contains("Remember cedar."));
    assert!(
        copying
            .list_chats()
            .await
            .iter()
            .all(|chat| chat.id != source),
        "copying must not load the source Chat"
    );
    complete_turn(&copying, copy.id, "What was the word?").await;
    let sent = request_text(&model.requests.lock().unwrap()[0]);
    assert!(sent.contains("Remember cedar."), "{sent}");
    assert!(sent.contains("Saved the word cedar."), "{sent}");
    copying.shutdown().await;
    drop(copying);

    assert_eq!(std::fs::read(&source_path).unwrap(), source_bytes);
    let copy_path = transcript_file(&root, copy.id);
    assert_ne!(copy_path, source_path);
    let copy_file = std::fs::read_to_string(&copy_path).unwrap();
    assert!(copy_file.contains("Remember cedar."));
    assert!(copy_file.contains("What was the word?"));

    let reopened = Agent::open_embedded(
        root,
        config(),
        Arc::new(ScriptedModel::new(Vec::<Vec<_>>::new())),
    )
    .unwrap();
    let saved = reopened.saved_chats(directory.path()).await.unwrap();
    assert!(saved.iter().any(|chat| chat.id == source));
    assert!(saved.iter().any(|chat| chat.id == copy.id));
    let resumed_copy = reopened.resume_chat(copy.id).await.unwrap();
    let resumed_text = conversation_text(&resumed_copy);
    assert!(resumed_text.contains("Remember cedar."), "{resumed_text}");
    assert!(
        resumed_text.contains("The word was cedar."),
        "{resumed_text}"
    );
    let resumed_source = reopened.resume_chat(source).await.unwrap();
    assert!(!conversation_text(&resumed_source).contains("What was the word?"));
    reopened.shutdown().await;
}

#[tokio::test]
async fn a_triggered_loop_in_a_background_chat_keeps_running_and_writes_only_to_its_own_chat() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let model = Arc::new(ScriptedModel::new(vec![
        text("Watching the market."),
        text("Foreground answer."),
        text("BACKGROUND-TICK-ONE"),
        text("BACKGROUND-TICK-TWO"),
    ]));
    let agent = Agent::open_embedded(root.clone(), config(), model.clone()).unwrap();
    let market = ShellTick::new(Uuid::new_v4());
    market.install(&agent).await;
    let background = agent.create_chat(directory.path().into()).await.unwrap();
    complete_turn(&agent, background, "Watch the market.").await;

    let foreground = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    market.subscribe(&agent, background).await;
    complete_turn(&agent, foreground, "Foreground question.").await;
    for sequence in 1..=2 {
        market.update(sequence, "triggered market check");
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let event = events.recv().await.unwrap();
                if event.chat_id == background && matches!(event.kind, EventKind::Completed { .. })
                {
                    break;
                }
            }
        })
        .await
        .expect("the background Chat's triggered Loop stopped running");
    }
    let subscription = agent.tick_subscriptions(background).await.unwrap()[0].id;
    agent
        .update_tick_subscription(background, subscription, TickSubscriptionChange::Cancel)
        .await
        .unwrap();

    let background_text = conversation_text(&agent.snapshot(background).await.unwrap());
    let foreground_text = conversation_text(&agent.snapshot(foreground).await.unwrap());
    for marker in ["BACKGROUND-TICK-ONE", "BACKGROUND-TICK-TWO"] {
        assert!(background_text.contains(marker), "{background_text}");
        assert!(!foreground_text.contains(marker), "{foreground_text}");
    }
    assert!(!foreground_text.contains("triggered market check"));
    agent.shutdown().await;
    drop(agent);

    let background_file = std::fs::read_to_string(transcript_file(&root, background)).unwrap();
    let foreground_file = std::fs::read_to_string(transcript_file(&root, foreground)).unwrap();
    assert!(background_file.contains("BACKGROUND-TICK-TWO"));
    assert!(!foreground_file.contains("BACKGROUND-TICK"));
    assert!(!foreground_file.contains("scheduled market check"));
}

#[tokio::test]
async fn the_task_center_lists_tasks_from_every_chat_with_their_owner() {
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(Vec::<Vec<_>>::new())));
    agent.install_own_tickhost();
    let background = agent.create_chat(directory.path().into()).await.unwrap();
    let foreground = agent.create_chat(directory.path().into()).await.unwrap();
    let receipt = agent
        .submit_user_shell(background, "sleep 30".into(), true)
        .await
        .unwrap();
    let center = ShellTaskCenter::new(agent.clone());
    let snapshot = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = center.snapshot(foreground).await.unwrap();
            if snapshot.tasks.iter().any(|task| {
                task.id == receipt.command_id && task.state == ShellTaskState::Background
            }) {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the background Chat's task did not appear in the foreground Task Center");
    assert_eq!(snapshot.chat_id, foreground);
    let task = snapshot
        .tasks
        .iter()
        .find(|task| task.id == receipt.command_id)
        .unwrap();
    assert_eq!(task.chat_id, background);

    let stopped = center
        .dispatch(
            foreground,
            receipt.command_id,
            tickloop_agent_protocol::ShellTaskAction::Stop,
        )
        .await
        .unwrap();
    assert_eq!(stopped.chat_id, background);
    agent.shutdown().await;
}

#[tokio::test]
async fn branch_titles_follow_claude_code_and_count_saved_branches() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let agent = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![text("answer")])),
    )
    .unwrap();
    let source = agent.create_chat(directory.path().into()).await.unwrap();
    complete_turn(&agent, source, "  compare   two stocks ").await;
    let first = agent
        .branch_chat(BranchRequest {
            request_id: Uuid::new_v4(),
            source_chat_id: source,
            name: None,
        })
        .await
        .unwrap();
    assert_eq!(first.name, "compare two stocks (Branch)");
    let named = agent
        .branch_chat(BranchRequest {
            request_id: Uuid::new_v4(),
            source_chat_id: source,
            name: Some("value path".into()),
        })
        .await
        .unwrap();
    assert_eq!(named.name, "value path (Branch)");
    agent.shutdown().await;
    drop(agent);

    let reopened = Agent::open_embedded(
        root,
        config(),
        Arc::new(ScriptedModel::new(Vec::<Vec<_>>::new())),
    )
    .unwrap();
    reopened.resume_chat(source).await.unwrap();
    let second = reopened
        .branch_chat(BranchRequest {
            request_id: Uuid::new_v4(),
            source_chat_id: source,
            name: None,
        })
        .await
        .unwrap();
    assert_eq!(second.name, "compare two stocks (Branch 2)");
    let saved = reopened.saved_chats(directory.path()).await.unwrap();
    let listed = saved
        .iter()
        .find(|chat| chat.id == second.child_chat_id)
        .unwrap();
    assert_eq!(
        listed.title.as_deref(),
        Some("compare two stocks (Branch 2)")
    );
    assert_eq!(listed.branch_source, Some(source));
    reopened.shutdown().await;
}
