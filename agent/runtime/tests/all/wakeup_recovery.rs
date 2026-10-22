//! The wake-up base across restarts, processes and hosts: what a crash, a second process or a
//! merge must not lose or run twice.

use tickloop_agent_protocol::LoopTriggerNoticeKind;
use tickloop_agent_runtime::Agent;
use tickloop_agent_test_support::config;
use uuid::Uuid;

use super::wakeup_support::*;

/// Trigger input accepted and not started when its process went down waits through the load
/// for the Chat to be continued: its source already moved past it and delivers it no more.
#[tokio::test(flavor = "multi_thread")]
async fn unstarted_trigger_input_waits_through_a_crash() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("state");
    let model = ContractModel::new();
    let agent = Agent::open_embedded(root.clone(), config(), model.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    complete_turn(&agent, chat, "get ready").await;
    hold_a_turn(&agent, chat, &model).await;
    agent
        .deliver_trigger_for_tests(chat, scheduled("waited", 1), "waited-through-the-crash")
        .await
        .unwrap();
    agent.flush_transcripts().await;
    let image = crash_image(&root, directory.path());
    model.release_held_turn();
    agent.shutdown().await;

    let restarted_model = ContractModel::new();
    let restarted = Agent::open_embedded(image, config(), restarted_model.clone()).unwrap();
    restarted.resume_chat(chat).await.unwrap();
    tokio::time::sleep(SETTLE).await;
    assert_eq!(
        restarted_model.seen("waited-through-the-crash"),
        0,
        "a loaded Loop waits to be continued"
    );
    complete_turn(&restarted, chat, "carry on").await;
    restarted_model
        .wait_seen("waited-through-the-crash", 1)
        .await;
    restarted.shutdown().await;
}

/// A Tick Update the lease kept out leaves no watermark behind: a later process that the
/// stream gives the same update delivers it.
#[tokio::test(flavor = "multi_thread")]
async fn an_update_the_lease_kept_out_is_not_marked_delivered() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("state");
    let task = Uuid::new_v4();
    let holder_model = ContractModel::new();
    let (holder, holder_shell, chat) =
        subscribed_process(&root, directory.path(), task, holder_model.clone()).await;
    holder_shell.update(1, "taken-by-the-holder");
    holder_model.wait_seen("taken-by-the-holder", 1).await;

    let (other, other_shell) = resumed_process(&root, chat, task, ContractModel::new()).await;
    other_shell.update(5, "kept-out-by-the-lease");
    wait_for_notice(&other, chat, LoopTriggerNoticeKind::HeldByAnotherProcess).await;
    other.shutdown().await;
    holder.shutdown().await;

    let later_model = ContractModel::new();
    let (later, later_shell) = resumed_process(&root, chat, task, later_model.clone()).await;
    later_shell.update(5, "kept-out-by-the-lease");
    later_model.wait_seen("kept-out-by-the-lease", 1).await;
    later.shutdown().await;
}

/// A process that takes the lease over learns what the previous holder delivered: the same
/// update, held while the other process ran it, is replayed from the ledger instead of running
/// a second time.
#[tokio::test(flavor = "multi_thread")]
async fn a_process_taking_the_lease_over_does_not_run_what_the_holder_ran() {
    const SECOND: &str = "\"seq\":\"2\"";
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("state");
    let task = Uuid::new_v4();
    let holder_model = ContractModel::new();
    let (holder, holder_shell, chat) =
        subscribed_process(&root, directory.path(), task, holder_model.clone()).await;
    holder_shell.update(1, "taken-by-the-holder");
    holder_model.wait_seen("taken-by-the-holder", 1).await;

    let other_model = ContractModel::new();
    let (other, other_shell) = resumed_process(&root, chat, task, other_model.clone()).await;
    other_shell.update(2, "given-to-both");
    wait_for_notice(&other, chat, LoopTriggerNoticeKind::HeldByAnotherProcess).await;
    holder_shell.update(2, "given-to-both");
    holder_model.wait_seen(SECOND, 1).await;
    holder.shutdown().await;

    // The held delivery is retried once the lease is free, and answered by the ledger.
    tokio::time::timeout(WAIT, async {
        while other.tick_subscriptions(chat).await.unwrap()[0]
            .last_delivery_ms
            .is_none()
        {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the other process retries the held update");
    tokio::time::sleep(SETTLE).await;
    assert_eq!(other_model.seen(SECOND), 0, "{:?}", other_model.prompts());
    other.shutdown().await;
}

/// `/stop` in another process holding the same Chat pauses this one before it takes the Trigger
/// input it queued: the turn running here ends, and nothing starts after it.
#[tokio::test(flavor = "multi_thread")]
async fn a_stop_elsewhere_keeps_queued_trigger_input_from_starting() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("state");
    let model = ContractModel::new();
    let agent = Agent::open_embedded(root.clone(), config(), model.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    complete_turn(&agent, chat, "get ready").await;
    agent
        .deliver_trigger_for_tests(chat, scheduled("lease", 1), "takes-the-lease")
        .await
        .unwrap();
    model.wait_seen("takes-the-lease", 1).await;
    agent.flush_transcripts().await;

    let elsewhere = Agent::open_embedded(root, config(), ContractModel::new()).unwrap();
    elsewhere.resume_chat(chat).await.unwrap();
    hold_a_turn(&agent, chat, &model).await;
    agent
        .deliver_trigger_for_tests(chat, scheduled("queued", 2), "queued-behind-the-turn")
        .await
        .unwrap();
    elsewhere.stop(chat).await.unwrap();
    model.release_held_turn();
    tokio::time::sleep(SETTLE).await;
    assert_eq!(
        model.seen("queued-behind-the-turn"),
        0,
        "{:?}",
        model.prompts()
    );
    assert_eq!(
        agent.snapshot(chat).await.unwrap().chat_loop.receiving,
        tickloop_agent_protocol::Receiving::Paused {
            reason: tickloop_agent_protocol::PauseReason::Stopped
        }
    );
    elsewhere.shutdown().await;
    agent.shutdown().await;
}

/// The ledger record of a delivery merged into one of the same key that waits reaches the
/// Transcript on its own, without anything else of the Chat changing.
#[tokio::test(flavor = "multi_thread")]
async fn a_merged_delivery_is_recorded_durably() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("state");
    let model = ContractModel::new();
    let agent = Agent::open_embedded(root.clone(), config(), model.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    complete_turn(&agent, chat, "get ready").await;
    hold_a_turn(&agent, chat, &model).await;
    agent
        .deliver_trigger_for_tests(chat, scheduled("same-key", 1), "waits")
        .await
        .unwrap();
    tokio::time::sleep(SETTLE).await;
    let merged = scheduled("same-key", 2);
    agent
        .deliver_trigger_for_tests(chat, merged.clone(), "merges")
        .await
        .unwrap();
    let merged_id = tickloop_agent_transcript::input_ids::trigger(chat, &merged.identity());
    let transcript = tickloop_agent_test_support::transcript_file(&root, chat);
    tokio::time::timeout(WAIT, async {
        loop {
            let recorded = std::fs::read_to_string(&transcript)
                .unwrap()
                .lines()
                .filter_map(|line| serde_json::from_str::<tickloop_agent_transcript::Line>(line).ok())
                .any(|line| {
                    matches!(
                        line,
                        tickloop_agent_transcript::Line::Input(entry)
                            if entry.input_id == merged_id
                                && matches!(
                                    entry.state,
                                    tickloop_agent_transcript::InputState::Settled {
                                        outcome: tickloop_agent_transcript::SettledOutcome::Merged { .. }
                                    }
                                )
                    )
                });
            if recorded {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the merged delivery's record reaches the Transcript");
    model.release_held_turn();
    agent.shutdown().await;
}

/// A delivery merged into the one of its key that waits answers with that one's receipt, the
/// run that takes both, on its first acceptance and when it is handed over again.
#[tokio::test(flavor = "multi_thread")]
async fn a_merged_delivery_answers_with_the_receipt_it_merged_into() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("state");
    let model = ContractModel::new();
    let agent = Agent::open_embedded(root, config(), model.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    complete_turn(&agent, chat, "get ready").await;
    hold_a_turn(&agent, chat, &model).await;
    let waiting = agent
        .deliver_trigger_for_tests(chat, scheduled("same-key", 1), "waits")
        .await
        .unwrap();
    let merged = agent
        .deliver_trigger_for_tests(chat, scheduled("same-key", 2), "merges")
        .await
        .unwrap();
    let replayed = agent
        .deliver_trigger_for_tests(chat, scheduled("same-key", 2), "merges")
        .await
        .unwrap();
    assert_eq!(merged.run_id, waiting.run_id);
    assert_eq!(replayed.run_id, waiting.run_id);
    model.release_held_turn();
    model.wait_seen("waits", 1).await;
    tokio::time::sleep(SETTLE).await;
    assert_eq!(model.seen("merges"), 0, "{:?}", model.prompts());
    agent.shutdown().await;
}

/// The stored Transcript says which task a scheduled run's prompt came from, by the identity
/// its ledger id is named by.
#[tokio::test(flavor = "multi_thread")]
async fn a_triggered_prompt_shows_the_source_it_came_from() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("state");
    let model = ContractModel::new();
    let agent = Agent::open_embedded(root.clone(), config(), model.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let source = scheduled("every-5m", 300_000);
    let receipt = agent
        .deliver_trigger_for_tests(chat, source.clone(), "scheduled-prompt")
        .await
        .unwrap();
    model.wait_seen("scheduled-prompt", 1).await;
    agent.shutdown().await;

    let lines: Vec<tickloop_agent_transcript::Line> =
        std::fs::read_to_string(tickloop_agent_test_support::transcript_file(&root, chat))
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
    let origins: Vec<_> = tickloop_agent_transcript::display::project(&lines)
        .into_iter()
        .filter_map(|node| match node.kind {
            tickloop_agent_transcript::display::DisplayKind::Prompt { origin, .. } => Some(origin),
            tickloop_agent_transcript::display::DisplayKind::Answer { .. } => None,
        })
        .collect();
    assert_eq!(
        origins,
        [tickloop_agent_transcript::display::PromptOrigin::Trigger {
            identity: source.identity(),
            authority: tickloop_agent_protocol::Authority::Instruction,
            merged: 1,
        }]
    );
    let delivery = tickloop_agent_transcript::display::trigger_deliveries(&lines);
    assert_eq!(delivery.len(), 1);
    assert_eq!(delivery[0].run_id, receipt.run_id);
    assert_eq!(
        delivery[0].input_id,
        tickloop_agent_transcript::input_ids::trigger(chat, &source.identity())
    );
}
