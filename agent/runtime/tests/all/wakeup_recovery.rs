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
