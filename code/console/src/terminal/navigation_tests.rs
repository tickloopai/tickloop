use super::{Agent, Console, Menus, navigate_agent};
use crate::agent_console::{AgentConsole, Destination, Intent};
use futures_util::FutureExt;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    os::unix::fs::{FileTypeExt, MetadataExt},
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tickhost_connect::herdr::{HerdrContext, HerdrPort, Pane, SocketHerdr};
use tickloop_code_access::{
    AgentAccess, AgentCommand, CommandEnvelope, CommandResult, DisplayUpdate, ExecutionRef,
    HostLocation, HostWorkContext, LaunchSpec, UserInput, ViewRef,
};
use tickloop_code_daemon::EmbeddedAgentAccess;
use tickloop_harness::protocol::{EventKind, UserShellRecord, UserShellState};

use tickloop_harness::runtime::Agent as RuntimeAgent;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const NAVIGATION_TIMEOUT: Duration = Duration::from_secs(10);

async fn host_request(host: &SocketHerdr, method: &str, params: Value) -> Value {
    host.request(method, params, CancellationToken::new())
        .await
        .unwrap_or_else(|error| panic!("isolated Herdr {method}: {error}"))
}

fn host_location(socket: &Path, pane: &Pane) -> HostLocation {
    let metadata = std::fs::metadata(socket).unwrap();
    HostLocation {
        host_instance: format!("herdr:{}:{}", metadata.dev(), metadata.ino()),
        socket: socket.into(),
        workspace: Some(pane.workspace_id.clone()),
        tab: Some(pane.tab_id.clone()),
        pane: Some(pane.pane_id.clone()),
        terminal: Some(pane.terminal_id.clone()),
        client: None,
    }
}

async fn create_execution(
    access: &EmbeddedAgentAccess,
    launch: LaunchSpec,
) -> (ExecutionRef, Uuid) {
    let receipt = access
        .execute(CommandEnvelope {
            daemon_epoch: access.daemon_epoch(),
            command_id: Uuid::new_v4(),
            command: AgentCommand::Create {
                launch,
                initial: None,
                origin: None,
            },
        })
        .await
        .unwrap();
    let Some(CommandResult::Created {
        execution,
        snapshot,
        ..
    }) = receipt.result
    else {
        panic!("{receipt:?}");
    };
    (execution, snapshot.conversation.id.into())
}

async fn shell_record(agent: &Agent, chat: Uuid, state: UserShellState) -> UserShellRecord {
    tokio::time::timeout(NAVIGATION_TIMEOUT, async {
        loop {
            let snapshot = agent.snapshot(chat).await.unwrap();
            if let Some(record) = snapshot
                .user_shells
                .iter()
                .find(|record| record.state == state)
            {
                break record.clone();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("Agent A Shell did not reach {state:?}"))
}

async fn run_navigation(
    host: &SocketHerdr,
    socket: &Path,
    workspace: &str,
    first_pane: Pane,
    runtime: &RuntimeAgent,
    directory: &Path,
) {
    let work_a = directory.join("agent-a");
    let work_b = directory.join("agent-b");
    std::fs::create_dir_all(&work_a).unwrap();
    std::fs::create_dir_all(&work_b).unwrap();
    let split = host_request(
        host,
        "pane.split",
        json!({"workspace_id":workspace,"target_pane_id":first_pane.pane_id,"direction":"right","cwd":work_b,"focus":false,"env":{"HOME":directory,"HISTFILE":"/dev/null"}}),
    )
    .await;
    let second_pane: Pane = serde_json::from_value(split["pane"].clone()).unwrap();
    let source_host = host_location(socket, &first_pane);
    let destination_host = host_location(socket, &second_pane);
    let access = Arc::new(EmbeddedAgentAccess::new(runtime.clone()));
    let mut launch_a = LaunchSpec::embedded(work_a.clone());
    launch_a.model = Some(tickloop_harness::test_support::config());
    launch_a.session_persistence = false;
    launch_a.host_work_context = Some(HostWorkContext {
        socket: socket.into(),
        workspace: Some(workspace.into()),
    });
    let mut launch_b = launch_a.clone();
    launch_b.folder = work_b;
    let (execution_a, chat_a) = create_execution(&access, launch_a.clone()).await;
    let (execution_b, chat_b) = create_execution(&access, launch_b).await;
    let caller_view = ViewRef {
        session_id: Uuid::new_v4(),
        view_id: Uuid::new_v4(),
    };
    let other_view = ViewRef {
        session_id: Uuid::new_v4(),
        view_id: Uuid::new_v4(),
    };
    let agent = Agent::from_access_for_test(
        access.clone(),
        access.daemon_epoch(),
        caller_view,
        launch_a,
        Some(source_host.clone()),
    );
    agent.display_chat(Some(chat_a)).await.unwrap();
    let source_before = agent.origin().unwrap();
    let other_display = access
        .show_view(DisplayUpdate::Displayed {
            view: other_view,
            expected_version: 0,
            execution: execution_b,
            host: Some(destination_host),
        })
        .await
        .unwrap();
    let submitted = access.execute(CommandEnvelope {
        daemon_epoch: access.daemon_epoch(),
        command_id: Uuid::new_v4(),
        command: AgentCommand::Submit {
            execution: execution_a,
            input: UserInput::Shell {
                command: "printf 'A started'; while [ ! -f finish ]; do sleep 0.02; done; printf ' A finished'".into(),
                background: true,
            },
            origin: Some(source_before.clone()),
        },
    }).await.unwrap();
    assert!(submitted.error.is_none(), "{submitted:?}");
    let running = shell_record(&agent, chat_a, UserShellState::Background).await;
    assert_eq!(running.cwd, work_a.canonicalize().unwrap());
    let (console, _input) = Console::start(false).unwrap();
    let mut menus = Menus::new(None, None);
    let mut global = AgentConsole::new(
        access.clone(),
        access.daemon_epoch(),
        work_a.clone(),
        Some(source_host),
    );
    global.visible_ready(Some(execution_a));
    global.refresh().await.unwrap();
    let effect = global
        .interpret(Intent::Open(execution_b))
        .unwrap()
        .unwrap();
    assert!(
        matches!(&effect.destination, Destination::Focus(destination) if destination.view == other_view)
    );
    assert!(
        navigate_agent(
            &mut global,
            &agent,
            &console,
            &mut menus,
            effect,
            &HashMap::new(),
            &mut HashMap::new()
        )
        .await
        .unwrap()
        .is_none()
    );
    assert_eq!(global.projection().visible, Some(execution_a));
    assert_eq!(agent.origin(), Some(source_before));
    let focused = host_request(host, "pane.get", json!({"pane_id":second_pane.pane_id})).await;
    assert_eq!(focused["pane"]["focused"], true);
    let catalog = agent.catalog().await.unwrap();
    assert_eq!(catalog.len(), 2);
    assert!(
        catalog
            .iter()
            .find(|row| row.execution == execution_a)
            .unwrap()
            .views
            .iter()
            .any(|view| view.view == caller_view)
    );
    assert!(
        !catalog
            .iter()
            .find(|row| row.execution == execution_b)
            .unwrap()
            .views
            .iter()
            .any(|view| view.view == caller_view)
    );
    shell_record(&agent, chat_a, UserShellState::Background).await;

    let moved = host_request(host, "pane.move", json!({"pane_id":second_pane.pane_id,"destination":{"type":"new_tab","workspace_id":workspace,"label":"navigation moved View"},"focus":false})).await;
    let moved_pane: Pane = serde_json::from_value(moved["move_result"]["pane"].clone()).unwrap();
    assert_ne!(moved_pane.tab_id, second_pane.tab_id);
    global.refresh().await.unwrap();
    let stale = global
        .interpret(Intent::Open(execution_b))
        .unwrap()
        .unwrap();
    assert!(matches!(&stale.destination, Destination::Focus(_)));
    let presented = navigate_agent(
        &mut global,
        &agent,
        &console,
        &mut menus,
        stale,
        &HashMap::new(),
        &mut HashMap::new(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(presented.id, chat_b);
    assert_eq!(global.projection().visible, Some(execution_b));
    assert_eq!(agent.catalog().await.unwrap().len(), 2);
    shell_record(&agent, chat_a, UserShellState::Background).await;

    global.refresh().await.unwrap();
    let return_a = global
        .interpret(Intent::Open(execution_a))
        .unwrap()
        .unwrap();
    assert!(matches!(&return_a.destination, Destination::Present));
    let returned = navigate_agent(
        &mut global,
        &agent,
        &console,
        &mut menus,
        return_a,
        &HashMap::new(),
        &mut HashMap::new(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(returned.id, chat_a);
    let displayed = access
        .show_view(DisplayUpdate::Displayed {
            view: other_view,
            expected_version: other_display.version,
            execution: execution_b,
            host: Some(host_location(socket, &moved_pane)),
        })
        .await
        .unwrap();
    assert!(displayed.version > other_display.version);
    global.refresh().await.unwrap();
    let closing = global
        .interpret(Intent::Open(execution_b))
        .unwrap()
        .unwrap();
    assert!(matches!(&closing.destination, Destination::Focus(_)));
    host_request(host, "pane.close", json!({"pane_id":moved_pane.pane_id})).await;
    let gone = host
        .request(
            "pane.get",
            json!({"pane_id":moved_pane.pane_id}),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    println!("Closed owned Pane lookup: {gone:?}");
    let recovered = navigate_agent(
        &mut global,
        &agent,
        &console,
        &mut menus,
        closing,
        &HashMap::new(),
        &mut HashMap::new(),
    )
    .await
    .unwrap()
    .expect("a known closed target must present the existing execution in place");
    assert_eq!(recovered.id, chat_b);
    assert_eq!(global.projection().visible, Some(execution_b));
    let catalog = agent.catalog().await.unwrap();
    assert_eq!(catalog.len(), 2);
    let original = catalog
        .iter()
        .find(|row| row.execution == execution_a)
        .unwrap();
    assert_eq!(original.folder, Some(work_a.canonicalize().unwrap()));
    assert_eq!(original.workspace.as_deref(), Some(workspace));
    shell_record(&agent, chat_a, UserShellState::Background).await;
    std::fs::write(work_a.join("finish"), "finish").unwrap();
    let completed = shell_record(&agent, chat_a, UserShellState::Completed).await;
    assert_eq!(completed.id, running.id);
    assert_eq!(
        std::fs::read_to_string(completed.output_ref.unwrap()).unwrap(),
        "A started A finished"
    );
    assert!(
        agent
            .snapshot(chat_a)
            .await
            .unwrap()
            .events
            .iter()
            .all(|event| !matches!(event.kind, EventKind::Started))
    );
    agent.leave_view().await.unwrap();
    access
        .show_view(DisplayUpdate::Left {
            view: other_view,
            version: displayed.version,
        })
        .await
        .unwrap();
    println!(
        "Isolated Herdr navigation verified server Pane focus; client focus scope remains Unknown (no client selector/acknowledgement in pane.focus)."
    );
}

#[tokio::test]
#[ignore = "requires TICKLOOP_TEST_HERDR_SOCKET for an isolated owned Herdr server"]
async fn isolated_herdr_terminal_navigation_focuses_then_recovers_moved_and_closed_views() {
    let socket = PathBuf::from(std::env::var("TICKLOOP_TEST_HERDR_SOCKET").unwrap());
    assert!(socket.is_absolute());
    assert!(
        socket
            .canonicalize()
            .unwrap()
            .starts_with("/private/tmp/tickloop-herdr-implementation")
    );
    assert!(std::fs::metadata(&socket).unwrap().file_type().is_socket());
    let directory = tempfile::tempdir_in("/private/tmp/tickloop-herdr-implementation").unwrap();
    let host = SocketHerdr::new(HerdrContext::without_view(socket.clone()));
    let created = host_request(&host, "workspace.create", json!({"cwd":directory.path(),"focus":false,"label":format!("navigation-{}",Uuid::new_v4()),"env":{"HOME":directory.path(),"HISTFILE":"/dev/null"}})).await;
    let workspace = created["workspace"]["workspace_id"].as_str().unwrap();
    let first_pane: Pane = serde_json::from_value(created["root_pane"].clone()).unwrap();
    let runtime = RuntimeAgent::new_embedded(
        tickloop_harness::test_support::config(),
        Arc::new(tickloop_harness::test_support::ScriptedModel::new(
            Vec::new(),
        )),
    );
    let result = AssertUnwindSafe(run_navigation(
        &host,
        &socket,
        workspace,
        first_pane,
        &runtime,
        directory.path(),
    ))
    .catch_unwind()
    .await;
    runtime.shutdown().await;
    host_request(&host, "workspace.close", json!({"workspace_id":workspace})).await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
