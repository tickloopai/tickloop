//! The wake-up base across restarts, processes and hosts: what a crash, a second process or a
//! merge must not lose or run twice.

use std::sync::Arc;

use tickloop_agent_protocol::LoopTriggerNoticeKind;
use tickloop_agent_runtime::Agent;
use tickloop_agent_test_support::config;
use tickloop_agent_test_support::transcripts::FaultyTranscriptStore;
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

/// The lease file of `root`'s Chats, as a process that holds none of them finds it gone: a
/// process that dies leaves its lease to the next taker the same way.
fn take_the_lease_away(root: &std::path::Path) {
    for lease in files_under(root, "trigger")
        .into_iter()
        .filter(|path| path.ends_with("lease.lock"))
    {
        std::fs::remove_file(lease).unwrap();
    }
}

/// A local Agent on `root` whose Transcript store is `store`, as `Agent::open_embedded` would
/// open it on its own store.
fn process_on(
    root: &std::path::Path,
    store: Arc<dyn tickloop_agent_runtime::transcript::TranscriptStore>,
    model: Arc<ContractModel>,
) -> Agent {
    Agent::with_seams(
        tickloop_agent_runtime::HarnessSeams {
            transcripts: Some(store),
            environment: Some(Arc::new(
                tickloop_agent_runtime::environment::LocalEnvironment::bare(),
            )),
            models: tickloop_agent_test_support::harness::ChosenModels::new(model),
        },
        tickloop_agent_runtime::HarnessOptions {
            settings: Some(tickloop_agent_config::UserSettings::in_root(root)),
            ..tickloop_agent_runtime::HarnessOptions::local(config())
        },
    )
    .unwrap()
}

/// Deliveries arriving while a process takes the lease over all wait for it to take in what
/// the previous holder delivered: none of them runs what the holder ran.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_deliveries_wait_for_the_takeover() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("state");
    let holder_model = ContractModel::new();
    let holder = Agent::open_embedded(root.clone(), config(), holder_model.clone()).unwrap();
    let chat = holder.create_chat(directory.path().into()).await.unwrap();
    complete_turn(&holder, chat, "get ready").await;
    holder.flush_transcripts().await;

    let other_model = ContractModel::new();
    let store = Arc::new(FaultyTranscriptStore::new(Arc::new(
        tickloop_agent_runtime::transcript::JsonlTranscriptStore::open(root.clone()).unwrap(),
    )));
    let other = process_on(&root, store.clone(), other_model.clone());
    other.resume_chat(chat).await.unwrap();
    complete_turn(&other, chat, "resume here").await;

    let mut first_receipts = Vec::new();
    for n in 0..4 {
        first_receipts.push(
            holder
                .deliver_trigger_for_tests(
                    chat,
                    scheduled(&format!("task-{n}"), 1),
                    "ran-by-the-holder",
                )
                .await
                .unwrap()
                .run_id,
        );
    }
    holder_model.wait_seen("ran-by-the-holder", 4).await;
    wait_for_state(&holder, chat, tickloop_agent_protocol::LoopState::Idle).await;
    holder.shutdown().await;

    // The first delivery takes the lease and reads the store; the rest arrive meanwhile.
    store.hold_loads();
    let deliver = |n: usize| {
        let other = other.clone();
        tokio::spawn(async move {
            other
                .deliver_trigger_for_tests(
                    chat,
                    scheduled(&format!("task-{n}"), 1),
                    "ran-by-the-holder",
                )
                .await
                .unwrap()
                .run_id
        })
    };
    let first = deliver(0);
    tokio::time::timeout(WAIT, async {
        while store.waiting_loads() == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the takeover reads the store");
    let rest: Vec<_> = (1..4).map(deliver).collect();
    tokio::time::sleep(SETTLE).await;
    store.release_loads();
    let mut receipts = vec![first.await.unwrap()];
    for repeat in rest {
        receipts.push(repeat.await.unwrap());
    }
    assert_eq!(receipts, first_receipts);
    tokio::time::sleep(SETTLE).await;
    assert_eq!(
        other_model.seen("ran-by-the-holder"),
        0,
        "{:?}",
        other_model.prompts()
    );
    other.shutdown().await;
}

/// A takeover that cannot read the Transcript delivers nothing, and tries again on the next
/// delivery.
#[tokio::test(flavor = "multi_thread")]
async fn a_takeover_that_cannot_read_the_store_delivers_nothing_until_it_can() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("state");
    let holder_model = ContractModel::new();
    let holder = Agent::open_embedded(root.clone(), config(), holder_model.clone()).unwrap();
    let chat = holder.create_chat(directory.path().into()).await.unwrap();
    complete_turn(&holder, chat, "get ready").await;
    holder.flush_transcripts().await;

    let other_model = ContractModel::new();
    let other = Agent::open_embedded(root.clone(), config(), other_model.clone()).unwrap();
    other.resume_chat(chat).await.unwrap();
    complete_turn(&other, chat, "resume here").await;
    other.flush_transcripts().await;

    let first = holder
        .deliver_trigger_for_tests(chat, scheduled("task", 1), "ran-by-the-holder")
        .await
        .unwrap();
    holder_model.wait_seen("ran-by-the-holder", 1).await;
    wait_for_state(&holder, chat, tickloop_agent_protocol::LoopState::Idle).await;
    holder.shutdown().await;

    let transcript = tickloop_agent_test_support::transcript_file(&root, chat);
    std::fs::set_permissions(&transcript, std::fs::Permissions::from_mode(0o000)).unwrap();
    let refused = other
        .deliver_trigger_for_tests(chat, scheduled("task", 1), "ran-by-the-holder")
        .await;
    std::fs::set_permissions(&transcript, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(refused.is_err(), "{refused:?}");
    wait_for_notice(&other, chat, LoopTriggerNoticeKind::Warning).await;

    let replayed = other
        .deliver_trigger_for_tests(chat, scheduled("task", 1), "ran-by-the-holder")
        .await
        .unwrap();
    assert_eq!(replayed.run_id, first.run_id);
    tokio::time::sleep(SETTLE).await;
    assert_eq!(
        other_model.seen("ran-by-the-holder"),
        0,
        "{:?}",
        other_model.prompts()
    );
    other.shutdown().await;
}

/// A process that loaded the Chat before the holder accepted a delivery, and takes the lease
/// over from it, runs what the holder accepted and did not start, under the holder's receipt.
#[tokio::test(flavor = "multi_thread")]
async fn a_preloaded_process_taking_over_runs_what_the_holder_left_waiting() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("state");
    let holder_model = ContractModel::new();
    let holder = Agent::open_embedded(root.clone(), config(), holder_model.clone()).unwrap();
    let chat = holder.create_chat(directory.path().into()).await.unwrap();
    complete_turn(&holder, chat, "get ready").await;
    holder.flush_transcripts().await;

    let other_model = ContractModel::new();
    let other = Agent::open_embedded(root.clone(), config(), other_model.clone()).unwrap();
    other.resume_chat(chat).await.unwrap();
    complete_turn(&other, chat, "resume here").await;

    hold_a_turn(&holder, chat, &holder_model).await;
    let accepted = holder
        .deliver_trigger_for_tests(chat, scheduled("task", 1), "left-waiting")
        .await
        .unwrap();
    holder.flush_transcripts().await;
    // The holder is gone for the other process, its turn never ending.
    take_the_lease_away(&root);

    let repeated = other
        .deliver_trigger_for_tests(chat, scheduled("task", 1), "left-waiting")
        .await
        .unwrap();
    assert_eq!(repeated.run_id, accepted.run_id);
    other_model.wait_seen("left-waiting", 1).await;
    tokio::time::sleep(SETTLE).await;
    assert_eq!(other_model.seen("left-waiting"), 1);
    assert_eq!(holder_model.seen("left-waiting"), 0);
    other.shutdown().await;
    holder_model.release_held_turn();
    holder.shutdown().await;
}

/// Another process that only loads the Chat interrupts, in its records, what it found running
/// and the Agent message it found waiting; it ran none of it. A holder taking its lease again
/// while they run and wait here goes on with its running Trigger turn, the Agent message and
/// the Trigger input waiting behind it.
#[tokio::test(flavor = "multi_thread")]
async fn a_process_that_only_loaded_the_chat_ran_nothing_of_it() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("state");
    let model = ContractModel::new();
    let holder = Agent::open_embedded(root.clone(), config(), model.clone()).unwrap();
    let chat = holder.create_chat(directory.path().into()).await.unwrap();
    holder
        .set_execution_mode(chat, tickloop_agent_protocol::ExecutionMode::FullAccess)
        .await
        .unwrap();
    holder.submit(chat, SPAWN_HELD_CHILD.into()).await.unwrap();
    model.wait_held(HELD_CHILD).await;
    wait_for_state(&holder, chat, tickloop_agent_protocol::LoopState::Idle).await;
    holder
        .deliver_trigger_for_tests(chat, scheduled("running", 1), "HOLD:turn running-trigger")
        .await
        .unwrap();
    model.wait_held("turn").await;
    holder
        .deliver_trigger_for_tests(chat, scheduled("queued", 1), "queued-trigger")
        .await
        .unwrap();
    model.release(HELD_CHILD);
    tokio::time::timeout(WAIT, async {
        while !holder
            .snapshot(chat)
            .await
            .unwrap()
            .pending_inputs
            .iter()
            .any(|pending| pending.agent_message.is_some())
        {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the child's completion waits behind the running turn");
    holder.flush_transcripts().await;

    let loader = Agent::open_embedded(root.clone(), config(), ContractModel::new()).unwrap();
    loader.resume_chat(chat).await.unwrap();
    loader.flush_transcripts().await;
    loader.shutdown().await;
    // The holder takes the lease again with a delivery while its turn runs and the rest waits.
    take_the_lease_away(&root);
    holder
        .deliver_trigger_for_tests(chat, scheduled("after", 1), "delivered-after-the-load")
        .await
        .unwrap();

    let turns_before = model.prompts().len();
    model.release_held_turn();
    model.wait_seen("delivered-after-the-load", 1).await;
    let after = &model.prompts()[turns_before..];
    assert!(
        after.iter().any(|prompt| !prompt.contains("queued-trigger")
            && !prompt.contains("delivered-after-the-load")
            && !prompt.contains("child task")),
        "the child's completion reached the holder: {:?}",
        model.prompts()
    );
    assert_eq!(model.seen("queued-trigger"), 1, "{:?}", model.prompts());
    holder.flush_transcripts().await;
    let running =
        tickloop_agent_transcript::input_ids::trigger(chat, &scheduled("running", 1).identity());
    let deliveries = stored_trigger_deliveries(&root, chat);
    let running = deliveries
        .iter()
        .find(|delivery| delivery.input_id == running)
        .unwrap();
    assert!(
        matches!(
            running.state,
            tickloop_agent_transcript::InputState::Settled {
                outcome: tickloop_agent_transcript::SettledOutcome::Completed { .. }
            }
        ),
        "the running Trigger turn ends as it ran here: {:?}",
        running.state
    );
    holder.shutdown().await;
}
