//! The wake-up base across restarts, processes and hosts: what a crash, a second process or a
//! merge must not lose or run twice.

use std::sync::Arc;

use tickloop_agent_protocol::LoopTriggerNoticeKind;
use tickloop_agent_runtime::Agent;
use tickloop_agent_runtime::trigger_pauses::PausePoint;
use tickloop_agent_test_support::config;
use tickloop_agent_test_support::shell_ticks::ShellTick;
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
/// update, held while the other process ran it, is known as delivered instead of running a
/// second time.
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

    // The held delivery is retried once the lease is free, and sorted out as delivered.
    tokio::time::timeout(WAIT, async {
        while other
            .loop_trigger_notices()
            .iter()
            .any(|notice| notice.chat_id == chat)
        {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the other process takes the lease over");
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

/// Two Shell Ticks of one Chat on one stream, both subscribed or watched.
struct TwoTicks {
    first: ShellTick,
    second: ShellTick,
}

impl TwoTicks {
    async fn installed(agent: &Agent, first: Uuid, second: Uuid) -> Self {
        let stream = tickloop_agent_runtime::tick_stream::InMemoryTickStream::new();
        let ticks = Self {
            first: ShellTick::on(stream.clone(), first),
            second: ShellTick::on(stream, second),
        };
        ticks.first.install(agent).await;
        ticks
    }
}

/// A process taking the lease over sorts out every update the holder delivered, whatever
/// batch it comes in: a batch of its own that repeats one of them in part delivers only the
/// rest.
#[tokio::test(flavor = "multi_thread")]
async fn a_takeover_delivers_only_what_a_partly_repeated_batch_adds() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("state");
    let (first_task, second_task) = (Uuid::new_v4(), Uuid::new_v4());
    let holder_model = ContractModel::new();
    let holder = Agent::open_embedded(root.clone(), config(), holder_model.clone()).unwrap();
    let holder_ticks = TwoTicks::installed(&holder, first_task, second_task).await;
    let chat = holder.create_chat(directory.path().into()).await.unwrap();
    complete_turn(&holder, chat, "watch both").await;
    holder_ticks.first.subscribe(&holder, chat).await;
    holder_ticks.second.subscribe(&holder, chat).await;
    holder.flush_transcripts().await;

    let other_model = ContractModel::new();
    let other = Agent::open_embedded(root.clone(), config(), other_model.clone()).unwrap();
    let other_ticks = TwoTicks::installed(&other, first_task, second_task).await;
    other.resume_chat(chat).await.unwrap();
    complete_turn(&other, chat, "resume here").await;
    other_ticks.first.wait_until_live(&other, chat).await;
    other_ticks.second.wait_until_live(&other, chat).await;

    // The holder takes both updates in one batch.
    hold_a_turn(&holder, chat, &holder_model).await;
    holder_ticks.first.update(2, "first-two");
    holder_ticks.second.update(3, "second-three");
    tokio::time::sleep(SETTLE).await;
    holder_model.release_held_turn();
    holder_model.wait_seen("second-three", 1).await;
    assert_eq!(
        holder_model.first_turn_with("first-two"),
        holder_model.first_turn_with("second-three"),
        "one batch: {:?}",
        holder_model.prompts()
    );

    other_ticks.first.update(2, "first-two");
    other_ticks.second.update(4, "second-four");
    wait_for_notice(&other, chat, LoopTriggerNoticeKind::HeldByAnotherProcess).await;
    holder.shutdown().await;

    // A held update keeps its data while it waits.
    other_model.wait_seen("second-four", 1).await;
    tokio::time::sleep(SETTLE).await;
    assert_eq!(
        other_model.seen("\"seq\":\"2\""),
        0,
        "{:?}",
        other_model.prompts()
    );
    other.shutdown().await;
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

/// A Tick Update accepted and not started when its process went down runs once the Chat is
/// continued in the next process, which watches the Tick through a watch of its own.
#[tokio::test(flavor = "multi_thread")]
async fn an_accepted_tick_update_runs_after_a_crash_through_the_new_watch() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("state");
    let task = Uuid::new_v4();
    let model = ContractModel::new();
    let (agent, shell, chat) =
        subscribed_process(&root, directory.path(), task, model.clone()).await;
    agent
        .set_execution_mode(chat, tickloop_agent_protocol::ExecutionMode::FullAccess)
        .await
        .unwrap();
    // A foreground `!` command keeps the accepted update from starting.
    agent
        .submit_user_shell(chat, "sleep 30".into(), false)
        .await
        .unwrap();
    shell.update(1, "accepted-not-started");
    tokio::time::timeout(WAIT, async {
        while !agent
            .snapshot(chat)
            .await
            .unwrap()
            .pending_inputs
            .iter()
            .any(|pending| pending.trigger_source.is_some())
        {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the update waits behind the command");
    agent.flush_transcripts().await;
    let image = crash_image(&root, directory.path());
    agent.shutdown().await;

    let restarted_model = ContractModel::new();
    let (restarted, _shell) = resumed_process(&image, chat, task, restarted_model.clone()).await;
    restarted_model.wait_seen("accepted-not-started", 1).await;
    tokio::time::sleep(SETTLE).await;
    assert_eq!(restarted_model.seen("accepted-not-started"), 1);
    assert_eq!(model.seen("accepted-not-started"), 0);
    restarted.shutdown().await;
}

/// Two processes holding a Chat, one of which has Trigger input waiting behind a turn.
struct WaitingInput {
    _directory: tempfile::TempDir,
    root: std::path::PathBuf,
    chat: Uuid,
    here: Agent,
    model: Arc<ContractModel>,
    elsewhere: Agent,
    elsewhere_model: Arc<ContractModel>,
}

impl WaitingInput {
    const MARKER: &str = "waits-behind-the-turn";

    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("state");
        let model = ContractModel::new();
        let here = Agent::open_embedded(root.clone(), config(), model.clone()).unwrap();
        let chat = here.create_chat(directory.path().into()).await.unwrap();
        complete_turn(&here, chat, "get ready").await;
        here.flush_transcripts().await;
        let elsewhere_model = ContractModel::new();
        let elsewhere =
            Agent::open_embedded(root.clone(), config(), elsewhere_model.clone()).unwrap();
        elsewhere.resume_chat(chat).await.unwrap();
        complete_turn(&elsewhere, chat, "resume elsewhere").await;
        hold_a_turn(&here, chat, &model).await;
        here.deliver_trigger_for_tests(chat, scheduled("task", 1), Self::MARKER)
            .await
            .unwrap();
        here.flush_transcripts().await;
        Self {
            _directory: directory,
            root,
            chat,
            here,
            model,
            elsewhere,
            elsewhere_model,
        }
    }

    /// The other process learns of a pause, is continued by its user and asks for the lease;
    /// it may get it or not.
    async fn continue_elsewhere(&self) {
        let _ = self
            .elsewhere
            .deliver_trigger_for_tests(self.chat, scheduled("other", 1), "delivered-elsewhere")
            .await;
        complete_turn(&self.elsewhere, self.chat, "continue elsewhere").await;
        let _ = self
            .elsewhere
            .deliver_trigger_for_tests(self.chat, scheduled("other", 1), "delivered-elsewhere")
            .await;
    }

    /// The waiting input ran in one of the two processes, and in only one, once.
    async fn ran_once(self) {
        tokio::time::timeout(WAIT, async {
            while self.model.seen(Self::MARKER) + self.elsewhere_model.seen(Self::MARKER) == 0 {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the waiting input runs");
        tokio::time::sleep(SETTLE).await;
        assert_eq!(
            self.model.seen(Self::MARKER) + self.elsewhere_model.seen(Self::MARKER),
            1,
            "here: {:?} elsewhere: {:?}",
            self.model.prompts(),
            self.elsewhere_model.prompts()
        );
        self.elsewhere.shutdown().await;
        self.here.shutdown().await;
    }
}

/// A takeover that read the store before a `/stop` here and a takeover elsewhere: the stop
/// waits for it, and the waiting input runs once, wherever it runs.
#[tokio::test(flavor = "multi_thread")]
async fn a_stop_during_a_takeover_runs_the_waiting_input_once() {
    let setup = WaitingInput::new().await;
    let (here, chat) = (setup.here.clone(), setup.chat);
    // The next permit here takes the lease anew, reads the store and stops before applying it.
    take_the_lease_away(&setup.root);
    let takeover = here.pause_next_for_tests(chat, PausePoint::TakeoverRead);
    setup.model.release_held_turn();
    tokio::time::timeout(WAIT, takeover.entered)
        .await
        .expect("the takeover read the store")
        .unwrap();

    let stop = tokio::spawn({
        let here = here.clone();
        async move { here.stop(chat).await.unwrap() }
    });
    tokio::time::sleep(SETTLE).await;
    drop(takeover.release);
    stop.await.unwrap();
    setup.continue_elsewhere().await;
    complete_turn(&here, chat, "continue here").await;
    setup.ran_once().await;
}

/// Taking queued Trigger input and a pause's release of its holding do not interleave: an old
/// `/stop` finishing after a Continue, while the input here passed its checks, and a takeover
/// elsewhere run the waiting input once.
#[tokio::test(flavor = "multi_thread")]
async fn an_old_stop_finishing_late_runs_the_waiting_input_once() {
    let setup = WaitingInput::new().await;
    let (here, chat) = (setup.here.clone(), setup.chat);
    let stop_tail = here.pause_next_for_tests(chat, PausePoint::ReceivingRecorded);
    let mut checked = here.pause_next_for_tests(chat, PausePoint::TriggerChecked);
    let stop = tokio::spawn({
        let here = here.clone();
        async move { here.stop(chat).await.unwrap() }
    });
    tokio::time::timeout(WAIT, stop_tail.entered)
        .await
        .expect("the stop recorded the pause")
        .unwrap();
    // Continued here while the stop has not released its holding.
    let continued = tokio::spawn({
        let here = here.clone();
        async move { here.submit(chat, "continue here".into()).await.unwrap() }
    });
    // Without one boundary, the waiting input passes its checks here now.
    let _ = tokio::time::timeout(SETTLE, &mut checked.entered).await;
    drop(stop_tail.release);
    stop.await.unwrap();
    setup.continue_elsewhere().await;
    drop(checked.release);
    continued.await.unwrap();
    setup.ran_once().await;
}

/// Waits until `count` wait to enter the Chat's Trigger boundary.
async fn boundary_waiters(agent: &Agent, chat: Uuid, count: usize) {
    tokio::time::timeout(WAIT, async {
        while agent.trigger_boundary_waiters_for_tests(chat) != count {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{count} wait(s) to enter the boundary"));
}

/// Whether the Chat's recorded receiving switch, which other processes read, is on.
fn recorded_receiving(root: &std::path::Path) -> bool {
    let state = files_under(root, "trigger")
        .into_iter()
        .find(|path| path.ends_with("receiving.state"))
        .expect("the receiving switch is recorded");
    serde_json::from_slice::<serde_json::Value>(&std::fs::read(state).unwrap()).unwrap()
        ["receiving"]
        .as_bool()
        .unwrap()
}

/// A `/stop` and a Continue racing each turn the switch here and record it in one operation:
/// the Continue waits for the stop, and the switch here and the recorded one end alike.
#[tokio::test(flavor = "multi_thread")]
async fn a_stop_and_a_continue_leave_the_switch_and_its_record_alike() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("state");
    let model = ContractModel::new();
    let agent = Agent::open_embedded(root.clone(), config(), model.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    complete_turn(&agent, chat, "get ready").await;
    let changed = agent.pause_next_for_tests(chat, PausePoint::ReceivingChanged);
    let stop = tokio::spawn({
        let agent = agent.clone();
        async move { agent.stop(chat).await.unwrap() }
    });
    tokio::time::timeout(WAIT, changed.entered)
        .await
        .expect("the stop turned the switch here")
        .unwrap();
    let continued = tokio::spawn({
        let agent = agent.clone();
        async move { agent.continue_loop(chat).await.unwrap() }
    });
    boundary_waiters(&agent, chat, 1).await;
    drop(changed.release);
    stop.await.unwrap();
    continued.await.unwrap();
    assert!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .chat_loop
            .receiving
            .is_receiving()
    );
    assert!(
        recorded_receiving(&root),
        "the record follows the switch here"
    );
    agent.shutdown().await;
}

/// A lease check under way as its delivery is dropped and its Agent shuts down takes no lease
/// the shutdown would not release: another process takes the lease once the Agent is gone.
#[tokio::test(flavor = "multi_thread")]
async fn a_lease_check_cut_short_by_a_shutdown_leaves_the_lease_free() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("state");
    let model = ContractModel::new();
    let leaving = Agent::open_embedded(root.clone(), config(), model.clone()).unwrap();
    let chat = leaving.create_chat(directory.path().into()).await.unwrap();
    complete_turn(&leaving, chat, "get ready").await;
    leaving.flush_transcripts().await;
    let staying_model = ContractModel::new();
    let staying = Agent::open_embedded(root.clone(), config(), staying_model.clone()).unwrap();
    staying.resume_chat(chat).await.unwrap();
    complete_turn(&staying, chat, "resume here").await;

    let checking = leaving.pause_next_for_tests(chat, PausePoint::LeaseChecking);
    let delivery = tokio::spawn({
        let leaving = leaving.clone();
        async move {
            leaving
                .deliver_trigger_for_tests(chat, scheduled("cut-short", 1), "cut-short")
                .await
        }
    });
    tokio::time::timeout(WAIT, checking.entered)
        .await
        .expect("the lease check is under way")
        .unwrap();
    delivery.abort();
    let shutdown = tokio::spawn({
        let leaving = leaving.clone();
        async move { leaving.shutdown().await }
    });
    tokio::time::timeout(WAIT, async {
        while !shutdown.is_finished() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the shutdown completes while the check waits");
    drop(checking.release);
    let _ = delivery.await;

    staying
        .deliver_trigger_for_tests(chat, scheduled("after", 1), "taken-over")
        .await
        .unwrap();
    staying_model.wait_seen("taken-over", 1).await;
    assert_eq!(model.seen("cut-short") + staying_model.seen("cut-short"), 0);
    staying.shutdown().await;
}

/// Trigger input waiting while only the lease keeps it out, here a lock another host wrote, runs
/// once the lease can be taken, with nothing else delivered or submitted.
#[tokio::test(flavor = "multi_thread")]
async fn waiting_trigger_input_runs_once_the_lease_can_be_taken() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("state");
    let model = ContractModel::new();
    let agent = Agent::open_embedded(root.clone(), config(), model.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    complete_turn(&agent, chat, "get ready").await;
    hold_a_turn(&agent, chat, &model).await;
    agent
        .deliver_trigger_for_tests(chat, scheduled("task", 1), "waits-for-the-lease")
        .await
        .unwrap();
    let lease = files_under(&root, "trigger")
        .into_iter()
        .find(|path| path.ends_with("lease.lock"))
        .unwrap();
    std::fs::remove_file(&lease).unwrap();
    std::fs::write(
        &lease,
        serde_json::json!({"chatId": chat, "pid": 1, "nonce": Uuid::new_v4(), "startedAt": 1, "host": "another-host.invalid"})
            .to_string(),
    )
    .unwrap();
    model.release_held_turn();
    wait_for_notice(&agent, chat, LoopTriggerNoticeKind::Warning).await;
    tokio::time::sleep(SETTLE).await;
    assert_eq!(model.seen("waits-for-the-lease"), 0);
    std::fs::remove_file(&lease).unwrap();
    model.wait_seen("waits-for-the-lease", 1).await;
    tokio::time::sleep(SETTLE).await;
    assert_eq!(model.seen("waits-for-the-lease"), 1);
    agent.shutdown().await;
}

/// Trigger input waiting on a takeover that could not read the store runs once the store can be
/// read again, with nothing else delivered or submitted.
#[tokio::test(flavor = "multi_thread")]
async fn waiting_trigger_input_runs_once_the_store_reads_again() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("state");
    let model = ContractModel::new();
    let agent = Agent::open_embedded(root.clone(), config(), model.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    complete_turn(&agent, chat, "get ready").await;
    hold_a_turn(&agent, chat, &model).await;
    agent
        .deliver_trigger_for_tests(chat, scheduled("task", 1), "waits-for-the-store")
        .await
        .unwrap();
    agent.flush_transcripts().await;
    take_the_lease_away(&root);
    let transcript = tickloop_agent_test_support::transcript_file(&root, chat);
    std::fs::set_permissions(&transcript, std::fs::Permissions::from_mode(0o000)).unwrap();
    model.release_held_turn();
    wait_for_notice(&agent, chat, LoopTriggerNoticeKind::Warning).await;
    std::fs::set_permissions(&transcript, std::fs::Permissions::from_mode(0o600)).unwrap();
    model.wait_seen("waits-for-the-store", 1).await;
    tokio::time::sleep(SETTLE).await;
    assert_eq!(model.seen("waits-for-the-store"), 1);
    agent.shutdown().await;
}
