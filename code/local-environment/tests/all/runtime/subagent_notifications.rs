use std::sync::Arc;
use tickloop_code_local_environment::AgentLocalExt;

use tickloop_harness::protocol::EventKind;
use tickloop_harness::runtime::Agent;
use tickloop_harness::test_support::subagents::{
    SubagentTimingModel, completed, task_notifications, wait_for_snapshot,
};

#[tokio::test]
async fn racing_root_stop_and_child_completion_records_and_delivers_each_run_once() {
    use tickloop_harness::protocol::{ChildRunStatus, SubagentNotificationDelivery};
    use tickloop_harness::runtime::family_pauses::Point;

    for natural_end_first in [true, false] {
        let model = Arc::new(SubagentTimingModel::new(true));
        let core = Agent::new_embedded(tickloop_harness::test_support::config(), model.clone());
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
            root.uuid(),
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
        let snapshot =
            wait_for_snapshot(&core, root.uuid(), "one delivered completion", |snapshot| {
                task_notifications(snapshot).len() == 1
                    && snapshot.pending_inputs.is_empty()
                    && matches!(
                        snapshot.chat_loop.state,
                        tickloop_harness::protocol::LoopState::Idle
                            | tickloop_harness::protocol::LoopState::Interrupted
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
    let core = Agent::new_embedded(tickloop_harness::test_support::config(), model.clone());
    core.install_own_tickhost();
    let directory = tempfile::tempdir().unwrap();
    let root = core.create_chat(directory.path().into()).await.unwrap();
    core.set_execution_mode(root, tickloop_harness::protocol::ExecutionMode::FullAccess)
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
        let event = tickloop_harness::test_support::next_event(&mut events).await;
        if event.chat_id == root
            && matches!(&event.kind, EventKind::ToolStarted { name, .. } if name == "Bash")
        {
            break;
        }
    }
    core.interrupt(root).await.unwrap();
    wait_for_snapshot(&core, root.uuid(), "the Esc interruption", |snapshot| {
        snapshot.chat_loop.state == tickloop_harness::protocol::LoopState::Interrupted
    })
    .await;
    model.release.notify_one();
    let delivered = wait_for_snapshot(
        &core,
        root.uuid(),
        "the background completion after Esc",
        |snapshot| {
            task_notifications(snapshot).len() == 1
                && snapshot.chat_loop.state == tickloop_harness::protocol::LoopState::Idle
        },
    )
    .await;
    assert_ne!(delivered.chat_loop.run_id, Some(initial.run_id));
    assert_eq!(delivered.family_completions.len(), 1);
    core.shutdown().await;
}

/// Creating, running and finishing a Subagent keeps the parent's request prefix byte for byte the
/// same: the family roster stays out of the system prompt, the Subagent result names the child
/// and its completion arrives as a task notification.
#[tokio::test]
async fn a_subagents_life_keeps_the_parents_prefix() {
    let model = Arc::new(SubagentTimingModel::new(true));
    let core = Agent::new_embedded(tickloop_harness::test_support::config(), model.clone());
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
    let follow_up = core.submit(root, "How is it going?".into()).await.unwrap();
    completed(&mut events, &follow_up).await;
    model.release.notify_one();
    wait_for_snapshot(&core, root.uuid(), "the completion turn", |snapshot| {
        task_notifications(snapshot).len() == 1
            && snapshot.chat_loop.state == tickloop_harness::protocol::LoopState::Idle
    })
    .await;
    let requests = model
        .routed
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| {
            tickloop_harness::test_support::routed::state(request)["delegation"].is_null()
        })
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        requests.len(),
        4,
        "launch, finish, follow-up and completion"
    );
    for request in &requests {
        assert_eq!(
            serde_json::to_vec(&request.messages[..2]).unwrap(),
            serde_json::to_vec(&requests[0].messages[..2]).unwrap()
        );
        assert_eq!(
            serde_json::to_vec(&request.tools).unwrap(),
            serde_json::to_vec(&requests[0].tools).unwrap()
        );
    }
    let system = serde_json::to_string(&requests[0].messages[0]).unwrap();
    assert!(!system.contains("family_members"));
    assert!(!system.contains("\"audit\""));
    core.shutdown().await;
}

/// A Subagent's system prompt names only what was fixed when it was delegated, so a denied call
/// and later inputs leave its prefix byte for byte the same. It lives in the system prompt, which
/// a compaction never summarizes.
#[tokio::test]
async fn a_subagents_delegation_stays_fixed_through_denials() {
    use tickloop_harness::test_support::routed::{RoutedModel, has_tool_result, state};
    let model = Arc::new(RoutedModel::new(|request| {
        if request.tools.is_empty() {
            return tickloop_harness::test_support::text("Unused summary.");
        }
        if state(request)["delegation"].is_null() {
            return if has_tool_result(request, "launch") {
                tickloop_harness::test_support::text("Parent done")
            } else {
                tickloop_harness::test_support::call(
                    "launch",
                    "Subagent",
                    serde_json::json!({"task":"Inspect bounded evidence","name":"audit","run_in_background":true}),
                )
            };
        }
        if has_tool_result(request, "denied") {
            tickloop_harness::test_support::text("CHILD-FINAL-REPORT")
        } else {
            tickloop_harness::test_support::call(
                "denied",
                "Bash",
                serde_json::json!({"command":"curl -s https://example.com","description":"Fetch a page"}),
            )
        }
    }));
    let core = Agent::new_embedded(tickloop_harness::test_support::config(), model.clone());
    let directory = tempfile::tempdir().unwrap();
    let root = core.create_chat(directory.path().into()).await.unwrap();
    let mut events = core.subscribe();
    core.submit(root, "Start a background audit".into())
        .await
        .unwrap();
    let child = loop {
        let event = tickloop_harness::test_support::next_event(&mut events).await;
        if let EventKind::PermissionRequested { request } = event.kind {
            assert_ne!(event.chat_id, root);
            core.answer_permission(event.chat_id, request.id, false)
                .await
                .unwrap();
            break event.chat_id;
        }
    };
    // A bare refusal ends the child's run.
    wait_for_snapshot(&core, child.uuid(), "the refused call", |snapshot| {
        snapshot.chat_loop.state == tickloop_harness::protocol::LoopState::Interrupted
    })
    .await;
    for prompt in ["Report what you found", "Anything more?"] {
        core.submit(child, prompt.into()).await.unwrap();
        wait_for_snapshot(&core, child.uuid(), prompt, |snapshot| {
            snapshot.chat_loop.state == tickloop_harness::protocol::LoopState::Idle
                && snapshot.messages.iter().any(|message| {
                    message.content.iter().any(|content| {
                        matches!(content, tickloop_harness::protocol::Content::Text { text } if text == prompt)
                    })
                })
        })
        .await;
    }
    let requests = model
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| !request.tools.is_empty() && !state(request)["delegation"].is_null())
        .cloned()
        .collect::<Vec<_>>();
    assert!(requests.len() >= 3, "{}", requests.len());
    for request in &requests {
        assert_eq!(
            serde_json::to_vec(&request.messages[0]).unwrap(),
            serde_json::to_vec(&requests[0].messages[0]).unwrap()
        );
        assert_eq!(
            serde_json::to_vec(&request.tools).unwrap(),
            serde_json::to_vec(&requests[0].tools).unwrap()
        );
    }
    // The child reads its own environment once, into its fixed system prompt.
    let system = match &requests[0].messages[0].content[0] {
        tickloop_harness::protocol::Content::Text { text } => text.clone(),
        other => panic!("{other:?}"),
    };
    let folder = core.snapshot(child).await.unwrap().folder.unwrap();
    for expected in [
        format!(
            "# Environment\nYou have been invoked in the following environment:\n - Primary working directory: {}\n - Is a git repository: false\n - Platform: {}",
            folder.display(),
            std::env::consts::OS
        ),
        format!(
            "The exact model ID is {}.",
            core.snapshot(child).await.unwrap().model.model
        ),
    ] {
        assert!(system.contains(&expected), "{expected}\n{system}");
    }
    let delegation = state(requests.last().unwrap())["delegation"].clone();
    assert_eq!(delegation["task"], "Inspect bounded evidence");
    assert_eq!(delegation["depth"], 1);
    for internal in [
        "session_denials",
        "host_tools",
        "trusted_user_inputs",
        "host_context",
    ] {
        assert!(!delegation.to_string().contains(internal), "{internal}");
    }
    core.shutdown().await;
}
