//! Opening the Chat a View handoff carries. A second `make dev` stops the daemon, so the Chat the
//! old View showed no longer runs when the View execs the new build; the restored View must open
//! without it instead of exiting with "Chat is not known to this View".
use super::*;
use std::sync::Arc;
use tickloop_code_access::{LaunchSpec, ViewRef};
use tickloop_code_daemon::EmbeddedAgentAccess;
use uuid::Uuid;

struct Daemon {
    core: tickloop_harness::runtime::Agent,
    agent: Agent,
    workdir: std::path::PathBuf,
    _directory: tempfile::TempDir,
}

fn daemon() -> Daemon {
    let directory = tempfile::tempdir().unwrap();
    let workdir = directory.path().canonicalize().unwrap();
    let core = tickloop_harness::runtime::Agent::new_embedded(
        tickloop_harness::test_support::config(),
        Arc::new(tickloop_harness::test_support::ScriptedModel::new(vec![])),
    );
    let access = Arc::new(EmbeddedAgentAccess::new(core.clone()));
    let mut launch = LaunchSpec::embedded(workdir.clone());
    launch.model = Some(tickloop_harness::test_support::config());
    launch.session_persistence = false;
    let agent = Agent::from_access_for_test(
        access.clone(),
        access.daemon_epoch(),
        ViewRef {
            session_id: Uuid::new_v4(),
            view_id: Uuid::new_v4(),
        },
        launch,
        None,
    );
    Daemon {
        core,
        agent,
        workdir,
        _directory: directory,
    }
}

fn saved_view(chat: Uuid, draft: &str) -> console::ConsoleUpgrade {
    let console = Console::test_for_chat(chat);
    console.test_type(draft);
    console.test_upgrade_snapshot()
}

#[tokio::test]
async fn a_handoff_whose_chat_the_daemon_no_longer_runs_opens_the_view_without_it() {
    let daemon = daemon();
    let ended = Uuid::new_v4();
    let mut saved = saved_view(ended, "unsent words");

    let opened = open_exact_chat(
        &daemon.agent,
        ended.into(),
        &daemon.workdir,
        Some(&mut saved),
    )
    .await;

    assert!(
        matches!(opened, Ok(StartupChat::Ended)),
        "{:?}",
        opened.err()
    );
    let restored = Console::test_for_chat(Uuid::new_v4());
    restored.select_chat(None).unwrap();
    restored.restore_upgrade(saved).unwrap();
    assert_eq!(restored.test_draft(), "unsent words");
    assert!(
        daemon.agent.catalog().await.unwrap().is_empty(),
        "a handoff neither loads nor creates the ended Chat"
    );
    daemon.core.shutdown().await;
}

#[tokio::test]
async fn a_handoff_reconnects_the_chat_the_daemon_runs() {
    let daemon = daemon();
    let chat = daemon
        .agent
        .create_chat(daemon.workdir.clone())
        .await
        .unwrap();
    let mut saved = saved_view(chat, "kept");

    let opened = open_exact_chat(
        &daemon.agent,
        chat.into(),
        &daemon.workdir,
        Some(&mut saved),
    )
    .await
    .unwrap();

    let StartupChat::Resumed(snapshot) = opened else {
        panic!("the live Chat opens with its history");
    };
    assert_eq!(Uuid::from(snapshot.id), chat);
    let restored = Console::test_for_chat(chat);
    restored.restore_upgrade(saved).unwrap();
    assert_eq!(restored.test_draft(), "kept");
    daemon.core.shutdown().await;
}

#[tokio::test]
async fn without_a_handoff_an_unknown_chat_still_starts_empty_under_its_id() {
    let daemon = daemon();
    let chat = Uuid::new_v4();

    let opened = open_exact_chat(&daemon.agent, chat.into(), &daemon.workdir, None)
        .await
        .unwrap();

    let StartupChat::Resumed(snapshot) = opened else {
        panic!("an unknown Chat starts empty");
    };
    assert_eq!(Uuid::from(snapshot.id), chat);
    assert!(snapshot.messages.is_empty());
    assert!(daemon.agent.runs_chat(chat).await.unwrap());
    daemon.core.shutdown().await;
}

#[tokio::test]
async fn only_a_chat_in_the_daemon_catalog_counts_as_running() {
    let daemon = daemon();
    let chat = daemon
        .agent
        .create_chat(daemon.workdir.clone())
        .await
        .unwrap();

    assert!(daemon.agent.runs_chat(chat).await.unwrap());
    assert!(!daemon.agent.runs_chat(Uuid::new_v4()).await.unwrap());
    daemon.core.shutdown().await;
}
