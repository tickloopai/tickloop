//! When a Chat has handled its Monitor instances' updates: a turn takes them in or reads them, or
//! the user opens the row. Later updates are unread. Apart from that, when the Chat's Agent last
//! used each instance with a tool.

use std::{sync::Arc, time::Duration};
use tickloop_code_local_environment::AgentLocalExt;
use tickloop_code_settings::local_agent::LocalAgent;

use async_trait::async_trait;
use serde_json::{Value, json};
use tick_protocol::InstanceRef;
use tickloop_harness::protocol::{
    ChatId, ChatSnapshot, Content, Event, EventKind, ToolDefinition, ToolResult,
};
use tickloop_harness::runtime::{
    Agent,
    host_tools::{HostToolPermission, HostToolProvider},
    tick_tools::{TICK_CALL, TICK_MANAGE, TICK_READ},
};
use tickloop_harness::test_support::{ScriptedModel, call, config, next_event, text};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

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
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .submit_user_shell(chat, "printf done".into(), true)
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
    agent.submit(chat, "what happened?".into()).await.unwrap();
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
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "start it".into()).await.unwrap();
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
    agent.submit(chat, "check it".into()).await.unwrap();
    finish_run(&agent, chat, &mut events).await;
    let read = agent.snapshot(chat).await.unwrap().monitor;
    let read_at = read
        .handled_at(&shell)
        .expect("reading the output file handled the Shell so far");
    assert_eq!(
        read.used_at(&shell),
        Some(read_at),
        "reading the output file used the Shell"
    );
    assert_eq!(
        read.taken_in_at(&shell),
        None,
        "a read handles the Shell without taking an update in"
    );

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
    assert_eq!(
        finished.monitor.taken_in_at(&shell),
        finished.monitor.handled_at(&shell)
    );
}

/// Tick tools that answer every call, and fail the calls on `failing`.
struct TickTools {
    failing: InstanceRef,
}

#[async_trait]
impl HostToolProvider for TickTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        [TICK_READ, TICK_CALL, TICK_MANAGE]
            .map(|name| ToolDefinition {
                name: name.into(),
                description: format!("{name} on a fixture Tick."),
                input_schema: json!({"type": "object"}),
            })
            .into()
    }

    fn permission(&self, _: ChatId, _: &str, _: &Value) -> HostToolPermission {
        HostToolPermission::Automatic
    }

    async fn execute(&self, _: ChatId, _: &str, input: Value, _: CancellationToken) -> ToolResult {
        let is_error = input["tick_instance_id"] == self.failing.to_string();
        ToolResult {
            content: if is_error {
                "InstanceGone: the Tick ended".into()
            } else {
                json!({"seq": 1, "data": {"price": 1}}).to_string()
            },
            is_error,
            output_ref: None,
        }
    }
}

#[tokio::test]
async fn every_successful_tick_tool_use_marks_the_instance_used_and_only_a_read_handles_it() {
    let directory = tempfile::tempdir().unwrap();
    let [read, called, typed, failing, closed] = ["read", "called", "typed", "failing", "closed"]
        .map(|id| InstanceRef::new("hosted", id).unwrap());
    let model = Arc::new(ScriptedModel::new(vec![
        call("read", TICK_READ, json!({"tick_instance_id": read})),
        call(
            "call",
            TICK_CALL,
            json!({"tick_instance_id": called, "tool": "refresh", "args": {}}),
        ),
        call(
            "type",
            TICK_MANAGE,
            json!({"action": "send_input", "tick_instance_id": typed, "text": "q"}),
        ),
        call(
            "fail",
            TICK_CALL,
            json!({"tick_instance_id": failing, "tool": "refresh", "args": {}}),
        ),
        call(
            "close",
            TICK_MANAGE,
            json!({"action": "close", "tick_instance_id": closed}),
        ),
        text("done"),
    ]));
    let agent = Agent::new_embedded(config(), model);
    agent
        .install_host_tools(Arc::new(TickTools {
            failing: failing.clone(),
        }))
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let before = tickloop_harness::base::private_fs::unix_time_milliseconds();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "look at the Ticks".into())
        .await
        .unwrap();
    finish_run(&agent, chat, &mut events).await;

    let monitor = agent.snapshot(chat).await.unwrap().monitor;
    let read_at = monitor.used_at(&read).expect("TickRead used its instance");
    assert!(read_at >= before);
    assert_eq!(
        monitor.handled_at(&read),
        Some(read_at),
        "a read handles the updates so far"
    );
    assert_eq!(
        monitor.taken_in_at(&read),
        None,
        "a read is no hand-over to the Agent"
    );
    for operated in [&called, &typed] {
        assert!(
            monitor
                .used_at(operated)
                .is_some_and(|used| used >= read_at),
            "{operated} was operated"
        );
        assert_eq!(
            monitor.handled_at(operated),
            None,
            "operating {operated} handles nothing"
        );
        assert_eq!(monitor.taken_in_at(operated), None);
    }
    assert_eq!(
        monitor.used_at(&failing),
        None,
        "a failed call uses nothing"
    );
    assert_eq!(
        monitor.used_at(&closed),
        None,
        "closing an instance is not using it"
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
    agent.submit(chat, "hello".into()).await.unwrap();
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
    let before = tickloop_harness::base::private_fs::unix_time_milliseconds();
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
    assert!(
        membership.is_removed(&lost),
        "the reopened instance takes the lost row's place"
    );
}
