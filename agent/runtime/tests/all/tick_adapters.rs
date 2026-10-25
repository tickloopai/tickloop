use crate::support;
use serde_json::json;
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tick_protocol::InstanceRef;
use tickloop_agent_runtime::{
    Agent, ShellTaskCenter,
    tick_adapter::TickAdapters,
    tick_stream::{
        StreamTasks, TickObservation, TickStreamEvent, TickStreamHandle, TickStreamPort,
    },
};
use tickloop_agent_settings::local_agent::LocalAgent;
use tickloop_agent_test_support::{ScriptedModel, config};
use tickloop_local_environment::AgentLocalExt;
use uuid::Uuid;

/// How long a Shell watch is given to show that it publishes nothing: the Shell Tick looks at its
/// task several times meanwhile.
const SHELL_QUIET_WAIT: Duration = Duration::from_secs(1);

#[tokio::test]
async fn shell_subscription_preserves_its_policy_including_final_admission() {
    use support::tick_admission_contract::{AdmissionCase, verify_admission_policy};
    use tickloop_agent_protocol::UserShellState;
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let task = agent
        .submit_user_shell(chat, "sleep 0.1; printf shell-body".into(), true)
        .await
        .unwrap();
    let instance = InstanceRef::shell(task.command_id);
    // A later Shell exit legitimately starts another model turn; admission needs a settled baseline.
    let mut last_shell_state = None;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(record) = agent
                .snapshot(chat)
                .await
                .unwrap()
                .user_shells
                .into_iter()
                .find(|record| record.id == task.command_id)
            {
                let completed = record.state == UserShellState::Completed;
                let failed = matches!(
                    record.state,
                    UserShellState::Failed | UserShellState::Unknown
                );
                last_shell_state =
                    Some((record.state, record.pid, record.reason, record.exit_code));
                assert!(
                    !failed,
                    "Shell baseline failed for {}: last (state, pid, reason, exit_code) = {last_shell_state:?}",
                    task.command_id
                );
                if completed {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "Shell baseline timed out for {}: last (state, pid, reason, exit_code) = {last_shell_state:?}",
            task.command_id
        )
    });
    agent
        .install_tick_stream(Arc::new(TickAdapters::builtin(agent.clone(), None)))
        .await;
    verify_admission_policy(
        &agent,
        &model,
        chat.uuid(),
        &[AdmissionCase {
            plan_denied: false,
            name: "TickManage",
            input: json!({"action":"subscribe","tick_instance_id":instance}),
            policy_name: "TickManage",
            policy_input: json!({"action":"subscribe","tick_instance_id":instance}),
        }],
    )
    .await;
    agent.shutdown().await;
}

async fn next_update(stream: &mut TickStreamHandle) -> TickObservation {
    loop {
        if let TickStreamEvent::Observation { value, .. } = stream.events.recv().await.unwrap() {
            return value;
        }
    }
}

async fn wait_until_ready(stream: &mut TickStreamHandle, watch: Uuid) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match stream.events.recv().await.unwrap() {
                TickStreamEvent::Ready { watch: ready } if ready == watch => return,
                TickStreamEvent::Observation { value, .. } => {
                    panic!(
                        "a watch took what came before it as an update: {:?}",
                        value.data
                    )
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap();
}

async fn assert_quiet(stream: &mut TickStreamHandle, why: &str) {
    if let Ok(update) = tokio::time::timeout(SHELL_QUIET_WAIT, next_update(stream)).await {
        panic!("{why}: {:?}", update.data);
    }
}

/// A Shell Tick updates once, when it exits: output while it runs is no update, and a watch that
/// starts after the exit takes it as its baseline.
#[tokio::test]
async fn a_shell_updates_only_when_it_exits_and_a_late_watch_takes_the_exit_as_its_baseline() {
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(vec![])));
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let task = agent
        .submit_user_shell(
            chat,
            "printf baseline; while [ ! -f output ]; do sleep 0.02; done; printf ' live-output'; while [ ! -f finish ]; do sleep 0.02; done; exit 7".into(),
            true,
        )
        .await
        .unwrap();
    let center = ShellTaskCenter::new(agent.clone());
    let wait_for_output = |bytes: usize| {
        let center = center.clone();
        async move {
            tokio::time::timeout(Duration::from_secs(5), async {
                while !center
                    .snapshot(chat)
                    .await
                    .unwrap()
                    .tasks
                    .iter()
                    .any(|row| row.id == task.command_id && row.output_bytes == bytes as u64)
                {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
        }
    };
    wait_for_output("baseline".len()).await;
    let adapters = TickAdapters::builtin(agent.clone(), None);
    let mut stream = adapters.open(&StreamTasks::default());
    let instance = InstanceRef::shell(task.command_id);
    let first = tickloop_agent_runtime::tick_stream::WatchId::new();
    adapters.set_watch_chats(BTreeMap::from([(first, chat.uuid())]));
    stream
        .desired
        .send_replace(BTreeMap::from([(first, instance.clone())]));
    wait_until_ready(&mut stream, first.into()).await;

    std::fs::write(directory.path().join("output"), "").unwrap();
    wait_for_output("baseline live-output".len()).await;
    assert_quiet(&mut stream, "output while the Shell runs is no update").await;

    std::fs::write(directory.path().join("finish"), "").unwrap();
    let exit = tokio::time::timeout(Duration::from_secs(5), next_update(&mut stream))
        .await
        .expect("the exit is an update");
    assert_eq!(exit.data["status"], "exit 7");
    assert_eq!(exit.data["exit_code"], 7);
    assert_eq!(exit.data["output"], "baseline live-output");
    assert_quiet(&mut stream, "a Shell updates once").await;

    let late = tickloop_agent_runtime::tick_stream::WatchId::new();
    adapters.set_watch_chats(BTreeMap::from([(first, chat.uuid()), (late, chat.uuid())]));
    stream.desired.send_replace(BTreeMap::from([
        (first, instance.clone()),
        (late, instance),
    ]));
    wait_until_ready(&mut stream, late.into()).await;
    assert_quiet(
        &mut stream,
        "a late watch received the exit it started after",
    )
    .await;
    agent.shutdown().await;
}

/// The one update of a Shell Tick is the end this Agent recorded for its task, as the task's row
/// reports it, not the Tickhost's own account of the exit; a delivery that no longer holds it
/// reads the same end again, and nothing while the task runs.
#[tokio::test]
async fn a_shells_exit_update_is_the_end_its_record_tells() {
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(vec![])));
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let task = agent
        .submit_user_shell(
            chat,
            "printf ran; while [ ! -f finish ]; do sleep 0.02; done; exit 3".into(),
            true,
        )
        .await
        .unwrap();
    let adapters = TickAdapters::builtin(agent.clone(), None);
    let mut stream = adapters.open(&StreamTasks::default());
    let watch = tickloop_agent_runtime::tick_stream::WatchId::new();
    adapters.set_watch_chats(BTreeMap::from([(watch, chat.uuid())]));
    stream.desired.send_replace(BTreeMap::from([(
        watch,
        InstanceRef::shell(task.command_id),
    )]));
    wait_until_ready(&mut stream, watch.into()).await;
    let instance = InstanceRef::shell(task.command_id);
    assert_eq!(
        adapters.read_current(chat.uuid(), &instance).await,
        Ok(None)
    );
    std::fs::write(directory.path().join("finish"), "").unwrap();
    let exit = tokio::time::timeout(Duration::from_secs(5), next_update(&mut stream))
        .await
        .expect("the exit is an update");
    let read_again = adapters
        .read_current(chat.uuid(), &instance)
        .await
        .unwrap()
        .expect("an ended task reads its end again");
    assert_eq!(
        (read_again.sequence, &read_again.data),
        (exit.sequence, &exit.data)
    );
    let center = ShellTaskCenter::new(agent.clone());
    let listed = center
        .snapshot(chat)
        .await
        .unwrap()
        .tasks
        .into_iter()
        .find(|row| row.id == task.command_id)
        .unwrap();
    assert!(listed.state.is_terminal());
    assert_eq!(
        exit.data,
        json!({
            "task_id": listed.id,
            "status": listed.status_label(),
            "state": listed.state,
            "exit_code": listed.exit_code,
            "signal": listed.signal,
            "reason": listed.reason,
            "output_bytes": listed.output_bytes,
            "output": "ran",
            "truncated": false,
        })
    );
    agent.shutdown().await;
}

/// The stream a Worker installs runs under its Agent: when `shutdown` returns, the stream's
/// tasks have ended though the Agent is still held, as a host holds it for its final `sync`.
#[tokio::test]
async fn shutdown_ends_the_tick_stream_tasks_of_an_agent_still_held() {
    let metrics = tokio::runtime::Handle::current().metrics();
    let baseline = metrics.num_alive_tasks();
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::open_embedded(
        directory.path().join("state"),
        config(),
        Arc::new(ScriptedModel::new(vec![])),
    )
    .unwrap();
    agent
        .install_tick_stream(Arc::new(TickAdapters::builtin(agent.clone(), None)))
        .await;
    assert!(metrics.num_alive_tasks() > baseline);

    agent.shutdown().await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while metrics.num_alive_tasks() > baseline {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "{} tasks still alive after shutdown, {baseline} before",
            metrics.num_alive_tasks()
        )
    });
    agent.sync().await.unwrap();
}
