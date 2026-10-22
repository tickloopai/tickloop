use std::sync::Arc;

use tickloop_agent_protocol::EventKind;
use tickloop_agent_runtime::Agent;
use tickloop_agent_test_support::subagents::{
    SubagentTimingModel, completed, task_notifications, wait_for_snapshot,
};

#[tokio::test]
async fn racing_root_stop_and_child_completion_records_and_delivers_each_run_once() {
    use tickloop_agent_protocol::{ChildRunStatus, SubagentNotificationDelivery};
    use tickloop_agent_runtime::family_pauses::Point;

    for natural_end_first in [true, false] {
        let model = Arc::new(SubagentTimingModel::new(true));
        let core = Agent::new_embedded(tickloop_agent_test_support::config(), model.clone());
        let directory = tempfile::tempdir().unwrap();
        let root = core.create_chat(directory.path().into()).await.unwrap();
        let mut events = core.subscribe();
        let initial = core
            .submit(root, "Start a background audit".into())
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), model.entered.notified())
            .await
            .unwrap();
        completed(&mut events, &initial).await;
        let child = core.subagents(root).await.unwrap()[0].child_chat_id;
        let mut completion = core.pause_family_for_tests(child, Point::CompletionRecorded);
        let stopping = core.pause_family_for_tests(root, Point::StopAdmitted);
        if natural_end_first {
            model.release.notify_one();
            tokio::time::timeout(std::time::Duration::from_secs(3), &mut completion.entered)
                .await
                .unwrap()
                .unwrap();
        }
        let stop = tokio::spawn({
            let core = core.clone();
            async move { core.stop(root).await.unwrap() }
        });
        tokio::time::timeout(std::time::Duration::from_secs(3), stopping.entered)
            .await
            .unwrap()
            .unwrap();
        if !natural_end_first {
            model.release.notify_one();
            tokio::time::timeout(std::time::Duration::from_secs(3), &mut completion.entered)
                .await
                .unwrap()
                .unwrap();
        }
        drop(completion.release);
        let held = wait_for_snapshot(
            &core,
            root,
            "completion delivery during the stop",
            |snapshot| {
                snapshot.family_completions.len() == 1
                    && matches!(
                        snapshot.family_completions[0].notification,
                        SubagentNotificationDelivery::Queued
                    )
            },
        )
        .await;
        assert_eq!(held.pending_inputs.len(), 1);
        assert!(!held.pending_inputs[0].armed);
        assert_eq!(
            held.family_completions[0].run_status,
            Some(if natural_end_first {
                ChildRunStatus::Completed
            } else {
                ChildRunStatus::Interrupted
            })
        );
        drop(stopping.release);
        assert!(stop.await.unwrap().is_empty());
        core.continue_loop(root).await.unwrap();
        let snapshot = wait_for_snapshot(&core, root, "one delivered completion", |snapshot| {
            task_notifications(snapshot).len() == 1
                && snapshot.pending_inputs.is_empty()
                && matches!(
                    snapshot.chat_loop.state,
                    tickloop_agent_protocol::LoopState::Idle
                        | tickloop_agent_protocol::LoopState::Interrupted
                )
        })
        .await;
        assert_eq!(snapshot.family_completions.len(), 1);
        core.stop(root).await.unwrap();
        core.continue_loop(root).await.unwrap();
        assert_eq!(
            task_notifications(&core.snapshot(root).await.unwrap()).len(),
            1
        );
        core.shutdown().await;
    }
}

#[tokio::test]
async fn background_completion_after_esc_starts_a_new_parent_turn() {
    let model = Arc::new(SubagentTimingModel::with_boundary(
        true,
        Some("while [ ! -f parent-release ]; do sleep 0.01; done".into()),
    ));
    let core = Agent::new_embedded(tickloop_agent_test_support::config(), model.clone());
    core.install_own_tickhost();
    let directory = tempfile::tempdir().unwrap();
    let root = core.create_chat(directory.path().into()).await.unwrap();
    core.set_execution_mode(root, tickloop_agent_protocol::ExecutionMode::FullAccess)
        .await
        .unwrap();
    let mut events = core.subscribe();
    let initial = core
        .submit(root, "Start and await a tool boundary".into())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), model.entered.notified())
        .await
        .unwrap();
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        if event.chat_id == root
            && matches!(&event.kind, EventKind::ToolStarted { name, .. } if name == "Bash")
        {
            break;
        }
    }
    core.interrupt(root).await.unwrap();
    wait_for_snapshot(&core, root, "the Esc interruption", |snapshot| {
        snapshot.chat_loop.state == tickloop_agent_protocol::LoopState::Interrupted
    })
    .await;
    model.release.notify_one();
    let delivered = wait_for_snapshot(
        &core,
        root,
        "the background completion after Esc",
        |snapshot| {
            task_notifications(snapshot).len() == 1
                && snapshot.chat_loop.state == tickloop_agent_protocol::LoopState::Idle
        },
    )
    .await;
    assert_ne!(delivered.chat_loop.run_id, Some(initial.run_id));
    assert_eq!(delivered.family_completions.len(), 1);
    core.shutdown().await;
}
