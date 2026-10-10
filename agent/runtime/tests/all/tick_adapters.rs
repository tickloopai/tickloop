use crate::support;
use serde_json::json;
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tick_protocol::InstanceRef;
use tickloop_agent_runtime::{
    Agent, ShellTaskCenter,
    tick_adapter::TickAdapters,
    tick_stream::{TickObservation, TickStreamEvent, TickStreamHandle, TickStreamPort},
};
use tickloop_agent_test_support::{ScriptedModel, config};
use uuid::Uuid;

/// How long a Shell watch is given to show that it publishes nothing: the Shell Tick looks at its
/// task several times meanwhile.
const SHELL_QUIET_WAIT: Duration = Duration::from_secs(1);

#[tokio::test]
async fn shell_subscription_preserves_its_policy_including_final_admission() {
    use support::tick_admission_contract::{AdmissionCase, verify_admission_policy};
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let task = agent
        .submit_user_shell_for_goal(chat, None, "printf shell-body".into(), true)
        .await
        .unwrap();
    let instance = InstanceRef::shell(task.command_id);
    agent
        .install_tick_stream(Arc::new(TickAdapters::builtin(agent.clone(), None)))
        .await;
    verify_admission_policy(
        &agent,
        &model,
        chat,
        &[AdmissionCase {
            plan_denied: false,
            name: "SubscribeTick",
            input: json!({"tick_instance_id":instance}),
            policy_name: "SubscribeTick",
            policy_input: json!({"tick_instance_id":instance}),
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
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let task = agent
        .submit_user_shell_for_goal(
            chat,
            None,
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
    let mut stream = adapters.open();
    let instance = InstanceRef::shell(task.command_id);
    let first = Uuid::new_v4();
    adapters.set_watch_chats(BTreeMap::from([(first, chat)]));
    stream
        .desired
        .send_replace(BTreeMap::from([(first, instance.clone())]));
    wait_until_ready(&mut stream, first).await;

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

    let late = Uuid::new_v4();
    adapters.set_watch_chats(BTreeMap::from([(first, chat), (late, chat)]));
    stream.desired.send_replace(BTreeMap::from([
        (first, instance.clone()),
        (late, instance),
    ]));
    wait_until_ready(&mut stream, late).await;
    assert_quiet(
        &mut stream,
        "a late watch received the exit it started after",
    )
    .await;
    agent.shutdown().await;
}
