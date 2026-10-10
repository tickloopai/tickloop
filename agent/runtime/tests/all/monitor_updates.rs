//! When a Chat has handled its Monitor instances' updates: a turn takes them in, or the user opens
//! the row. Later updates are unread.

use std::{sync::Arc, time::Duration};

use serde_json::json;
use tick_protocol::InstanceRef;
use tickloop_agent_protocol::{ChatId, ChatSnapshot, Content, Event, EventKind};
use tickloop_agent_runtime::Agent;
use tickloop_agent_test_support::{ScriptedModel, call, config, next_event, text};
use tokio::sync::broadcast;

async fn finish_run(agent: &Agent, chat: ChatId, events: &mut broadcast::Receiver<Event>) {
    loop {
        match next_event(events).await.kind {
            EventKind::PermissionRequested { request } => {
                agent
                    .answer_permission(chat, request.id, true)
                    .await
                    .unwrap();
            }
            EventKind::Completed { .. } => return,
            EventKind::Failed(error) => panic!("run failed: {error}"),
            _ => {}
        }
    }
}

async fn wait_for_snapshot(
    agent: &Agent,
    chat: ChatId,
    predicate: impl Fn(&ChatSnapshot) -> bool,
) -> ChatSnapshot {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let snapshot = agent.snapshot(chat).await.unwrap();
            if predicate(&snapshot) {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("snapshot condition timed out")
}

fn has_notification(snapshot: &ChatSnapshot) -> bool {
    snapshot
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .any(|content| {
            matches!(content, Content::Text { text } if text.starts_with("<task-notification>"))
        })
}

#[tokio::test]
async fn an_ended_shell_is_unread_until_a_turn_takes_in_its_notification() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(Vec::new()));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .submit_user_shell_for_goal(chat, None, "printf done".into(), true)
        .await
        .unwrap();
    let ended = wait_for_snapshot(&agent, chat, has_notification).await;
    let shell = InstanceRef::shell(ended.user_shells[0].id);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .monitor
            .handled_at(&shell),
        None,
        "a notification no turn has read leaves the end unread"
    );

    model.enqueue(text("noted"));
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "what happened?".into())
        .await
        .unwrap();
    finish_run(&agent, chat, &mut events).await;
    let monitor = agent.snapshot(chat).await.unwrap().monitor;
    assert!(
        monitor.handled_at(&shell).is_some(),
        "the turn that took in the notification handled the end"
    );
    assert_eq!(
        monitor.taken_in_at(&shell),
        monitor.handled_at(&shell),
        "a turn handled it, not the user"
    );
}

#[tokio::test]
async fn reading_a_running_shells_output_handles_only_what_came_before_its_end() {
    let directory = tempfile::tempdir().unwrap();
    let gate = directory.path().join("release");
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "background",
            "Bash",
            json!({
                "command": format!("while [ ! -e '{}' ]; do sleep 0.05; done", gate.display()),
                "run_in_background": true,
                "description": "Wait for the release gate"
            }),
        ),
        text("started"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, None, "start it".into()).await.unwrap();
    finish_run(&agent, chat, &mut events).await;
    let started = agent.snapshot(chat).await.unwrap();
    let command = &started.commands[0];
    let shell = InstanceRef::shell(command.operation_id);
    assert_eq!(started.monitor.handled_at(&shell), None);

    // The output file is known only once the command runs, so the Read comes in a later turn.
    model.enqueue(call(
        "read",
        "Read",
        json!({"file_path": command.output_ref}),
    ));
    model.enqueue(text("still waiting"));
    agent
        .submit(chat, Some(started.loops[0].id), "check it".into())
        .await
        .unwrap();
    finish_run(&agent, chat, &mut events).await;
    let read_at = agent
        .snapshot(chat)
        .await
        .unwrap()
        .monitor
        .handled_at(&shell)
        .expect("reading the output file handled the Shell so far");

    model.enqueue(text("it finished"));
    std::fs::write(&gate, b"release").unwrap();
    let finished = wait_for_snapshot(&agent, chat, |snapshot| {
        snapshot
            .monitor
            .handled_at(&shell)
            .is_some_and(|at| at > read_at)
    })
    .await;
    let ended_at = finished.commands[0].updated_at_ms.unwrap_or_default();
    assert!(
        finished.monitor.handled_at(&shell).unwrap() >= ended_at,
        "the continuation that took in the notification handled the end"
    );
}

#[tokio::test]
async fn the_user_opening_a_row_handles_it_and_the_mark_survives_resume() {
    let directory = tempfile::tempdir().unwrap();
    let store = directory.path().join("archive");
    let instance: InstanceRef = "calendar:quotes".parse().unwrap();
    let agent = Agent::open_embedded(
        store.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![text("ok")])),
    )
    .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    // A Chat is saved once it has a conversation.
    let mut events = agent.subscribe();
    agent.submit(chat, None, "hello".into()).await.unwrap();
    finish_run(&agent, chat, &mut events).await;
    let membership = agent
        .mark_monitor_handled(chat, instance.clone())
        .await
        .unwrap();
    let handled = membership
        .handled_at(&instance)
        .expect("opening the row handles it");
    assert_eq!(
        membership.taken_in_at(&instance),
        None,
        "no turn took the updates in"
    );
    agent.shutdown().await;
    drop(agent);

    let restored =
        Agent::open_embedded(store, config(), Arc::new(ScriptedModel::new(Vec::new()))).unwrap();
    let snapshot = restored.resume_chat(chat).await.unwrap();
    assert_eq!(snapshot.monitor.handled_at(&instance), Some(handled));
    restored.shutdown().await;
}

#[tokio::test]
async fn a_reopened_instance_starts_from_what_it_already_did() {
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(Vec::new())));
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let lost = InstanceRef::hosted(uuid::Uuid::new_v4());
    let instance = InstanceRef::hosted(uuid::Uuid::new_v4());
    let before = tickloop_agent_base::private_fs::unix_time_milliseconds();
    let membership = agent
        .record_reopened_tick(chat, &lost, instance.clone())
        .await
        .unwrap();
    assert!(
        membership
            .handled_at(&instance)
            .is_some_and(|at| at >= before),
        "updates from before it joined are no news"
    );
    assert_eq!(membership.taken_in_at(&instance), None);
}
