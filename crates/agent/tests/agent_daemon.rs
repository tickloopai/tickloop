mod support;

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use tickloop_agent::{agent_access::*, runtime::Agent};
use tokio::{
    net::UnixListener,
    sync::{Mutex, Notify},
};
use uuid::Uuid;

struct FixtureFactory {
    started: AtomicUsize,
    runtimes: Mutex<Vec<Agent>>,
    accesses: Mutex<Vec<Arc<dyn AgentAccess>>>,
    availability: Mutex<Vec<Arc<AtomicBool>>>,
    exits: Mutex<Vec<Arc<AtomicBool>>>,
    model: Arc<support::ScriptedModel>,
    gate: Option<Arc<Notify>>,
    snapshot_gate: Option<Arc<SnapshotGate>>,
}

impl FixtureFactory {
    fn new(replies: Vec<Vec<tickloop_agent::model::Content>>) -> Self {
        Self {
            started: AtomicUsize::new(0),
            runtimes: Mutex::new(Vec::new()),
            accesses: Mutex::new(Vec::new()),
            availability: Mutex::new(Vec::new()),
            exits: Mutex::new(Vec::new()),
            model: Arc::new(support::ScriptedModel::new(replies)),
            gate: None,
            snapshot_gate: None,
        }
    }
    async fn shutdown(&self) {
        for runtime in self.runtimes.lock().await.iter() {
            runtime.shutdown().await;
        }
    }
}

#[async_trait]
impl WorkerFactory for FixtureFactory {
    async fn start(
        &self,
        launch: &LaunchSpec,
        epoch: Uuid,
    ) -> Result<Arc<dyn AgentAccess>, AccessError> {
        self.started.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = &self.gate {
            gate.notified().await;
        }
        let runtime = match &launch.config_root {
            Some(root) => {
                Agent::open_embedded(root.clone(), support::config(), self.model.clone()).unwrap()
            }
            None => Agent::new_embedded(support::config(), self.model.clone()),
        };
        self.runtimes.lock().await.push(runtime.clone());
        let available = Arc::new(AtomicBool::new(true));
        self.availability.lock().await.push(available.clone());
        self.exits
            .lock()
            .await
            .push(Arc::new(AtomicBool::new(false)));
        let access = Arc::new(FixtureWorker {
            access: EmbeddedAgentAccess::for_worker(runtime, epoch),
            available,
            snapshot_gate: self.snapshot_gate.clone(),
        });
        self.accesses.lock().await.push(access.clone());
        Ok(access)
    }
    async fn exited(&self, worker: &Arc<dyn AgentAccess>) -> bool {
        let index = self
            .accesses
            .lock()
            .await
            .iter()
            .position(|known| Arc::ptr_eq(known, worker));
        match index {
            Some(index) => self.exits.lock().await[index].load(Ordering::SeqCst),
            None => false,
        }
    }
}

struct FixtureWorker {
    access: EmbeddedAgentAccess,
    available: Arc<AtomicBool>,
    snapshot_gate: Option<Arc<SnapshotGate>>,
}

#[derive(Default)]
struct SnapshotGate {
    next: AtomicBool,
    entered: Notify,
    release: Notify,
}

#[async_trait]
impl AgentAccess for FixtureWorker {
    async fn observe(&self, query: ObservationRequest) -> Result<Observation, AccessError> {
        if !self.available.load(Ordering::SeqCst) {
            return Err(AccessError::unavailable("fixture worker disconnected"));
        }
        let directory_query = matches!(query, ObservationRequest::Overview { .. });
        let observation = self.access.observe(query).await?;
        if directory_query
            && let Some(gate) = &self.snapshot_gate
            && gate.next.swap(false, Ordering::SeqCst)
        {
            gate.entered.notify_one();
            gate.release.notified().await;
        }
        Ok(observation)
    }
    async fn execute(&self, command: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
        if !self.available.load(Ordering::SeqCst) {
            return Err(AccessError::unavailable("fixture worker disconnected"));
        }
        self.access.execute(command).await
    }
    async fn show_view(&self, update: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
        if !self.available.load(Ordering::SeqCst) {
            return Err(AccessError::unavailable("fixture worker disconnected"));
        }
        self.access.show_view(update).await
    }
}

fn envelope(epoch: Uuid, command: AgentCommand) -> CommandEnvelope {
    CommandEnvelope {
        owner_epoch: epoch,
        command_id: Uuid::new_v4(),
        command,
    }
}
fn create(epoch: Uuid, path: &Path) -> CommandEnvelope {
    envelope(
        epoch,
        AgentCommand::Create {
            launch: LaunchSpec::embedded(path.to_path_buf()),
            initial: None,
            origin: None,
        },
    )
}
fn execution(receipt: CommandReceipt) -> ExecutionRef {
    match receipt.into_result().unwrap() {
        CommandResult::Created { execution, .. } | CommandResult::Loaded { execution, .. } => {
            execution
        }
        other => panic!("unexpected result: {other:?}"),
    }
}
async fn agents(access: &dyn AgentAccess) -> Vec<AgentSummary> {
    match access
        .observe(ObservationRequest::Overview {
            filter: AgentFilter::default(),
            page: Page {
                offset: 0,
                limit: 256,
            },
            after: None,
        })
        .await
        .unwrap()
        .initial
    {
        ObservationValue::Overview(entries) => entries,
        other => panic!("unexpected directory: {other:?}"),
    }
}
async fn serve(
    directory: &Path,
    owner: Arc<OwnerServer>,
) -> (LocalAgentClient, tokio::task::JoinHandle<()>) {
    let socket = directory.join("a.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let epoch = owner.owner_epoch();
    let server = tokio::spawn(async move {
        serve_listener(listener, owner, epoch).await.unwrap();
    });
    (LocalAgentClient::connect(socket).await.unwrap(), server)
}

#[tokio::test]
async fn ipc_duplicate_commands_and_generations_preserve_the_original_target() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let factory = Arc::new(FixtureFactory::new(vec![]));
    let owner = Arc::new(OwnerServer::new(Uuid::new_v4(), factory.clone()));
    let (client, server) = serve(directory.path(), owner).await;
    let command = create(client.owner_epoch(), directory.path());
    let first = execution(client.execute(command.clone()).await.unwrap());
    let second = execution(client.execute(command.clone()).await.unwrap());
    assert_eq!(first, second);
    assert_eq!(factory.started.load(Ordering::SeqCst), 1);
    let mut changed = command.clone();
    changed.command = AgentCommand::FlushTranscripts;
    assert_eq!(
        client.execute(changed).await.unwrap_err().code,
        AccessErrorCode::CommandIdConflict
    );
    let observed = client
        .observe(ObservationRequest::Receipt {
            epoch: client.owner_epoch(),
            command_id: command.command_id,
        })
        .await
        .unwrap();
    assert!(
        matches!(observed.initial, ObservationValue::Receipt(Some(receipt)) if receipt.target == Some(first) && matches!(receipt.state, ReceiptState::Completed))
    );
    let invalid = ExecutionRef {
        generation: first.generation + 1,
        ..first
    };
    let rejected = client
        .execute(envelope(
            client.owner_epoch(),
            AgentCommand::Submit {
                execution: invalid,
                target_loop: None,
                input: UserInput::Text {
                    text: "must not run".into(),
                    contexts: vec![],
                    paths: vec![],
                },
                origin: None,
            },
        ))
        .await
        .unwrap();
    assert_eq!(
        rejected.error.unwrap().code,
        AccessErrorCode::StaleExecution
    );
    assert!(factory.model.requests.lock().unwrap().is_empty());
    assert_eq!(agents(&client).await.len(), 1);
    drop(client);
    server.abort();
    factory.shutdown().await;
}

#[tokio::test]
async fn views_are_versioned_per_session_and_disconnection_does_not_stop_the_agent() {
    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let factory = Arc::new(FixtureFactory::new(vec![]));
    let owner = Arc::new(OwnerServer::new(Uuid::new_v4(), factory.clone()));
    let (first, server) = serve(temporary.path(), owner.clone()).await;
    let target = execution(
        first
            .execute(create(first.owner_epoch(), temporary.path()))
            .await
            .unwrap(),
    );
    let second = LocalAgentClient::connect(temporary.path().join("a.sock"))
        .await
        .unwrap();
    let first_view = ViewRef {
        session_id: first.session_id(),
        view_id: Uuid::new_v4(),
    };
    let second_view = ViewRef {
        session_id: second.session_id(),
        view_id: Uuid::new_v4(),
    };
    for (client, view) in [(&first, first_view), (&second, second_view)] {
        assert_eq!(
            client
                .show_view(DisplayUpdate::Displayed {
                    view,
                    expected_version: 0,
                    execution: target,
                    host: None
                })
                .await
                .unwrap()
                .version,
            1
        );
    }
    assert_eq!(agents(&first).await[0].views.len(), 2);
    first
        .show_view(DisplayUpdate::Left {
            view: first_view,
            version: 1,
        })
        .await
        .unwrap();
    assert_eq!(
        first
            .show_view(DisplayUpdate::Displayed {
                view: first_view,
                expected_version: 0,
                execution: target,
                host: None
            })
            .await
            .unwrap_err()
            .code,
        AccessErrorCode::VersionConflict
    );
    assert_eq!(
        second
            .show_view(DisplayUpdate::Renew {
                view: first_view,
                version: 1
            })
            .await
            .unwrap_err()
            .code,
        AccessErrorCode::AccessDenied
    );
    let second_session = second.session_id();
    drop(second);
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if agents(&first).await[0].views.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        owner
            .show_view(DisplayUpdate::Displayed {
                view: second_view,
                expected_version: 0,
                execution: target,
                host: None
            })
            .await
            .unwrap_err()
            .code,
        AccessErrorCode::AccessDenied
    );
    assert_eq!(second_session, second_view.session_id);
    assert!(matches!(
        first
            .observe(ObservationRequest::Agent {
                execution: target,
                detail: AgentDetailQuery::Snapshot,
                after: None
            })
            .await
            .unwrap()
            .initial,
        ObservationValue::Agent(_)
    ));
    server.abort();
    factory.shutdown().await;
}

#[tokio::test]
async fn an_accepted_create_survives_a_lost_ack_and_can_be_queried_after_reconnect() {
    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let gate = Arc::new(Notify::new());
    let factory = Arc::new(FixtureFactory {
        gate: Some(gate.clone()),
        ..FixtureFactory::new(vec![])
    });
    let owner = Arc::new(OwnerServer::new(Uuid::new_v4(), factory.clone()));
    let (client, server) = serve(temporary.path(), owner).await;
    let command = create(client.owner_epoch(), temporary.path());
    let task_client = client.clone();
    let sent = command.clone();
    let task = tokio::spawn(async move { task_client.execute(sent).await });
    tokio::time::timeout(Duration::from_secs(3), async {
        while factory.started.load(Ordering::SeqCst) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    task.abort();
    drop(client);
    gate.notify_one();
    let replacement = LocalAgentClient::connect(temporary.path().join("a.sock"))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let observation = replacement.observe(ObservationRequest::Receipt { epoch: command.owner_epoch, command_id: command.command_id }).await.unwrap();
            if matches!(observation.initial, ObservationValue::Receipt(Some(receipt)) if matches!(receipt.state, ReceiptState::Completed)) { break; }
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    let target = execution(replacement.execute(command).await.unwrap());
    assert_eq!(agents(&replacement).await[0].execution, target);
    assert_eq!(factory.started.load(Ordering::SeqCst), 1);
    server.abort();
    factory.shutdown().await;
}

#[tokio::test]
async fn a_scene_can_supply_the_first_input_without_becoming_an_agent_view() {
    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let factory = Arc::new(FixtureFactory::new(vec![support::text("Finished.")]));
    let owner = Arc::new(OwnerServer::new(Uuid::new_v4(), factory.clone()));
    let (client, server) = serve(temporary.path(), owner).await;
    let view = ViewRef {
        session_id: client.session_id(),
        view_id: Uuid::new_v4(),
    };
    let scene = client
        .show_view(DisplayUpdate::Scene {
            view,
            expected_version: 0,
            host: None,
        })
        .await
        .unwrap();
    assert!(!scene.displayed);
    let receipt = client
        .execute(envelope(
            client.owner_epoch(),
            AgentCommand::Create {
                launch: LaunchSpec::embedded(temporary.path().to_path_buf()),
                initial: Some(UserInput::Text {
                    text: "Finish the task".into(),
                    contexts: vec![],
                    paths: vec![],
                }),
                origin: Some(InteractionSource {
                    view,
                    display_version: scene.version,
                    host: None,
                }),
            },
        ))
        .await
        .unwrap();
    let target = execution(receipt);
    assert!(agents(&client).await[0].views.is_empty());
    client
        .show_view(DisplayUpdate::Displayed {
            view,
            expected_version: scene.version,
            execution: target,
            host: None,
        })
        .await
        .unwrap();
    let stale = client
        .execute(envelope(
            client.owner_epoch(),
            AgentCommand::Submit {
                execution: target,
                target_loop: None,
                input: UserInput::Text {
                    text: "stale input".into(),
                    contexts: vec![],
                    paths: vec![],
                },
                origin: Some(InteractionSource {
                    view,
                    display_version: scene.version,
                    host: None,
                }),
            },
        ))
        .await
        .unwrap();
    assert_eq!(stale.error.unwrap().code, AccessErrorCode::VersionConflict);
    server.abort();
    factory.shutdown().await;
}

#[tokio::test]
async fn a_real_worker_keeps_shell_execution_after_ui_disconnect_and_exits_on_lifeline_eof() {
    use std::{os::unix::fs::PermissionsExt, process::Stdio};
    use tokio::io::AsyncWriteExt;

    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    std::fs::set_permissions(temporary.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = temporary.path().join("w.sock");
    let epoch = Uuid::new_v4();
    let mut launch = LaunchSpec::embedded(temporary.path().to_path_buf());
    launch.session_persistence = false;
    let mut model = support::config();
    model.api_key_env = "TICKLOOP_FIXTURE_API_KEY".into();
    launch.model = Some(model);
    let mut worker = tokio::process::Command::new(env!("CARGO_BIN_EXE_tickloop"))
        .arg("--agent-worker")
        .arg(&socket)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", temporary.path())
        .env("TICKLOOP_FIXTURE_API_KEY", "fixture-key")
        .env("DEEPSEEK_API_KEY", "fixture-compiled-selection-key")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lifeline = worker.stdin.take().unwrap();
    let control = serde_json::json!({
        "endpoint": temporary.path().join("worker-control.sock"),
        "owner_epoch": epoch,
        "binding": Uuid::new_v4(),
        "capability": "0".repeat(64),
        "host_endpoint": temporary.path().join("host.sock"),
        "intent_renewal": Duration::from_secs(100),
    });
    let mut bootstrap = serde_json::to_vec(
        &serde_json::json!({ "epoch": epoch, "launch": launch, "control": control }),
    )
    .unwrap();
    bootstrap.push(b'\n');
    lifeline.write_all(&bootstrap).await.unwrap();
    let client = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(client) = LocalAgentClient::connect(&socket).await {
                break client;
            }
            if let Some(status) = worker.try_wait().unwrap() {
                panic!("worker exited during startup: {status}");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let target = execution(
        client
            .execute(create(epoch, temporary.path()))
            .await
            .unwrap(),
    );
    let result = client
        .execute(envelope(
            epoch,
            AgentCommand::Submit {
                execution: target,
                target_loop: None,
                input: UserInput::Shell {
                    command: "sleep 0.1; printf 'still alive'".into(),
                    background: true,
                },
                origin: None,
            },
        ))
        .await
        .unwrap()
        .into_result()
        .unwrap();
    let CommandResult::ShellSubmitted(shell) = result else {
        panic!("expected background shell");
    };
    drop(client);
    let replacement = LocalAgentClient::connect(&socket).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = replacement.observe(ObservationRequest::Agent { execution: target, detail: AgentDetailQuery::Snapshot, after: None }).await.unwrap();
            if matches!(snapshot.initial, ObservationValue::Agent(snapshot) if snapshot.user_shells.iter().any(|record| record.id == shell.command_id && record.state == tickloop_agent::runtime::UserShellState::Completed)) { break; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.unwrap();
    let output = replacement
        .observe(ObservationRequest::Agent {
            execution: target,
            detail: AgentDetailQuery::TaskOutput {
                task: TaskRef {
                    execution: target,
                    task_id: shell.command_id,
                    origin: TaskOrigin::User,
                },
                offset: 0,
                limit: 1024,
            },
            after: None,
        })
        .await
        .unwrap();
    assert!(
        matches!(output.initial, ObservationValue::TaskOutput(chunk) if chunk.output.contains("still alive"))
    );
    assert!(worker.try_wait().unwrap().is_none());
    drop(lifeline);
    let status = tokio::time::timeout(Duration::from_secs(5), worker.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success());
    assert!(!socket.exists());
}

/// Stops whatever owner serves a test's private runtime directory when the test ends, also
/// after a failed assertion. The test's runtime may already be gone by then, so the shutdown
/// runs on its own thread and runtime and is awaited.
struct PrivateOwnerGuard {
    socket: std::path::PathBuf,
}
impl PrivateOwnerGuard {
    fn new(directory: &Path) -> Self {
        Self {
            socket: directory.join("owner.sock"),
        }
    }
}
impl Drop for PrivateOwnerGuard {
    fn drop(&mut self) {
        if !self.socket.exists() {
            return;
        }
        let socket = self.socket.clone();
        let _ = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async {
                let _ = tokio::time::timeout(Duration::from_secs(15), async {
                    if let Ok(client) = LocalAgentClient::connect(&socket).await {
                        let _ = client.shutdown_owner().await;
                        while socket.exists() {
                            tokio::time::sleep(Duration::from_millis(25)).await;
                        }
                    }
                })
                .await;
            });
            Ok::<(), std::io::Error>(())
        })
        .join();
    }
}

#[tokio::test]
async fn an_owner_killed_without_cleanup_never_blocks_the_next_owner() {
    use std::os::unix::fs::PermissionsExt;
    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let private_directory = temporary.path().join("agent-runtime");
    std::fs::create_dir(&private_directory).unwrap();
    std::fs::set_permissions(&private_directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let executable = std::path::PathBuf::from(env!("CARGO_BIN_EXE_tickloop"));
    let killed = tickloop_agent::agent_daemon::connect_or_start_at(
        private_directory.clone(),
        executable.clone(),
    )
    .await
    .unwrap();
    let killed_pid = killed.server_process_id() as libc::pid_t;
    assert!(private_directory.join("worker-control.sock").exists());
    unsafe { libc::kill(killed_pid, libc::SIGKILL) };
    for _ in 0..400 {
        if unsafe { libc::kill(killed_pid, 0) } != 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        private_directory.join("worker-control.sock").exists(),
        "a killed owner leaves its endpoints behind"
    );
    let _guard = PrivateOwnerGuard::new(&private_directory);
    let next = tickloop_agent::agent_daemon::connect_or_start_at(private_directory, executable)
        .await
        .expect("the next owner replaces the stale endpoints");
    assert_ne!(next.owner_epoch(), killed.owner_epoch());
}

#[tokio::test]
async fn a_real_owner_answers_hosted_lifecycle_requests_only_for_live_grants() {
    use std::os::unix::fs::PermissionsExt;
    use tickloop_agent::hosted::worker_control::{OwnerControl, WorkerControlGrant, WorkerRequest};
    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let private_directory = temporary.path().join("agent-runtime");
    std::fs::create_dir(&private_directory).unwrap();
    std::fs::set_permissions(&private_directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let _guard = PrivateOwnerGuard::new(&private_directory);
    let client = tickloop_agent::agent_daemon::connect_or_start_at(
        private_directory.clone(),
        std::path::PathBuf::from(env!("CARGO_BIN_EXE_tickloop")),
    )
    .await
    .unwrap();
    let epoch = client.owner_epoch();
    let endpoint = private_directory.join("worker-control.sock");
    assert_eq!(
        std::fs::metadata(&endpoint).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let forged = |owner_epoch| WorkerControlGrant {
        endpoint: endpoint.clone(),
        owner_epoch,
        binding: Uuid::new_v4(),
        capability: "0".repeat(64),
        host_endpoint: temporary.path().join("host.sock"),
        intent_renewal: std::time::Duration::from_secs(100),
    };
    let list = |chat| WorkerRequest::ListHostedTicks { chat, cursor: None };
    let unknown_grant = OwnerControl::new(forged(epoch))
        .request(list(Uuid::new_v4()))
        .await
        .unwrap_err();
    assert_eq!(unknown_grant.code, "Forbidden");
    assert!(!unknown_grant.dispatched);
    let earlier_owner = OwnerControl::new(forged(Uuid::new_v4()))
        .request(list(Uuid::new_v4()))
        .await
        .unwrap_err();
    assert_eq!(earlier_owner.code, "EpochChanged");
    let reconcile = {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let mut stream = tokio::net::UnixStream::connect(&endpoint).await.unwrap();
        let envelope = serde_json::json!({"id":1,"envelope":{"owner_epoch":epoch,"binding":Uuid::new_v4(),"capability":"0".repeat(64),"request":{"method":"reconcile"}}});
        stream
            .write_all(format!("{envelope}\n").as_bytes())
            .await
            .unwrap();
        let mut line = String::new();
        tokio::io::BufReader::new(stream)
            .read_line(&mut line)
            .await
            .unwrap();
        serde_json::from_str::<serde_json::Value>(&line).unwrap()
    };
    assert_eq!(reconcile["error"]["code"], "InvalidArguments");
}

#[tokio::test]
async fn concurrent_cold_start_uses_one_isolated_owner_and_owner_exit_closes_worker_lifelines() {
    use std::os::unix::fs::PermissionsExt;
    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let private_directory = temporary.path().join("agent-runtime");
    std::fs::create_dir(&private_directory).unwrap();
    std::fs::set_permissions(&private_directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let _guard = PrivateOwnerGuard::new(&private_directory);
    let executable = std::path::PathBuf::from(env!("CARGO_BIN_EXE_tickloop"));
    let mut calls = Vec::new();
    for _ in 0..8 {
        let directory = private_directory.clone();
        let executable = executable.clone();
        calls.push(tokio::spawn(async move {
            tickloop_agent::agent_daemon::connect_or_start_at(directory, executable)
                .await
                .unwrap()
        }));
    }
    let mut clients = Vec::new();
    for call in calls {
        clients.push(call.await.unwrap());
    }
    let epoch = clients[0].owner_epoch();
    assert!(clients.iter().all(|client| client.owner_epoch() == epoch));
    let owner_pid = clients[0].server_process_id() as i32;
    assert!(owner_pid > 0 && owner_pid != std::process::id() as i32);
    assert_eq!(
        std::fs::metadata(private_directory.join("owner.sock"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(private_directory.join("owner.lock"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let mut targets = Vec::new();
    for label in ["alpha", "beta"] {
        let mut launch = LaunchSpec::embedded(temporary.path().to_path_buf());
        launch.session_persistence = false;
        let mut model = support::config();
        model.name = label.into();
        model.api_key_env = "TICKLOOP_FIXTURE_API_KEY".into();
        launch.model = Some(model);
        launch.environment = [
            ("PATH", "/usr/bin:/bin"),
            ("TICKLOOP_FIXTURE_API_KEY", "fixture-key"),
            ("DEEPSEEK_API_KEY", "fixture-compiled-selection-key"),
            ("TICKLOOP_FIXTURE_WORKER", label),
        ]
        .into_iter()
        .map(|(name, value)| EnvironmentValue {
            name: name.as_bytes().to_vec(),
            value: value.as_bytes().to_vec(),
        })
        .collect();
        let target = execution(
            clients[0]
                .execute(envelope(
                    epoch,
                    AgentCommand::Create {
                        launch,
                        initial: None,
                        origin: None,
                    },
                ))
                .await
                .unwrap(),
        );
        let result = clients[0]
            .execute(envelope(
                epoch,
                AgentCommand::Submit {
                    execution: target,
                    target_loop: None,
                    input: UserInput::Shell {
                        command: "sleep 0.1; printf '%s' \"$TICKLOOP_FIXTURE_WORKER\"".into(),
                        background: true,
                    },
                    origin: None,
                },
            ))
            .await
            .unwrap()
            .into_result()
            .unwrap();
        let CommandResult::ShellSubmitted(shell) = result else {
            panic!("expected shell receipt");
        };
        targets.push((label, target, shell.command_id));
    }
    drop(clients);
    let replacement = tickloop_agent::agent_daemon::connect_or_start_at(
        private_directory.clone(),
        executable.clone(),
    )
    .await
    .unwrap();
    assert_eq!(replacement.owner_epoch(), epoch);
    for (label, target, task_id) in &targets {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let observation = replacement
                    .observe(ObservationRequest::Agent {
                        execution: *target,
                        detail: AgentDetailQuery::Snapshot,
                        after: None,
                    })
                    .await
                    .unwrap();
                if let ObservationValue::Agent(snapshot) = observation.initial {
                    assert_eq!(
                        snapshot.model,
                        tickloop_agent::model::cards::ModelCatalog::defaults()
                            .get("deepseek-api", "deepseek-flash")
                            .unwrap()
                    );
                    if snapshot.user_shells.iter().any(|record| {
                        record.id == *task_id
                            && record.state == tickloop_agent::runtime::UserShellState::Completed
                    }) {
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let output = replacement
            .observe(ObservationRequest::Agent {
                execution: *target,
                detail: AgentDetailQuery::TaskOutput {
                    task: TaskRef {
                        execution: *target,
                        task_id: *task_id,
                        origin: TaskOrigin::User,
                    },
                    offset: 0,
                    limit: 1024,
                },
                after: None,
            })
            .await
            .unwrap();
        assert!(
            matches!(output.initial, ObservationValue::TaskOutput(output) if output.output == *label)
        );
    }
    let worker_sockets = std::fs::read_dir(&private_directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("w-")
        })
        .collect::<Vec<_>>();
    assert_eq!(worker_sockets.len(), 2);
    replacement.shutdown_owner().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while worker_sockets.iter().any(|path| path.exists()) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let new_owner =
        tickloop_agent::agent_daemon::connect_or_start_at(private_directory.clone(), executable)
            .await
            .unwrap();
    assert_ne!(new_owner.owner_epoch(), epoch);
    assert!(agents(&new_owner).await.is_empty());
    let old = match new_owner
        .observe(ObservationRequest::Agent {
            execution: targets[0].1,
            detail: AgentDetailQuery::Snapshot,
            after: None,
        })
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("old execution must be fenced after owner restart"),
    };
    assert_eq!(old.code, AccessErrorCode::EpochChanged);
    new_owner.shutdown_owner().await.unwrap();
}

async fn controlled_connections(
    directory: &Path,
    owner: Arc<OwnerServer>,
) -> (
    LocalAgentClient,
    Arc<Mutex<Vec<tokio::task::AbortHandle>>>,
    tokio::task::JoinHandle<()>,
) {
    let socket = directory.join("c.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let handles = Arc::new(Mutex::new(Vec::new()));
    let saved = handles.clone();
    let epoch = owner.owner_epoch();
    let listener_task = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let owner = owner.clone();
            let task = tokio::spawn(async move {
                let _ = serve_connection(stream, owner, epoch).await;
            });
            saved.lock().await.push(task.abort_handle());
        }
    });
    (
        LocalAgentClient::connect(socket).await.unwrap(),
        handles,
        listener_task,
    )
}

#[tokio::test]
async fn a_recovering_client_rebuilds_observation_in_the_same_epoch_and_fences_old_ui_ready() {
    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let factory = Arc::new(FixtureFactory::new(vec![]));
    let owner = Arc::new(OwnerServer::new(Uuid::new_v4(), factory.clone()));
    let (client, connections, listener) = controlled_connections(temporary.path(), owner).await;
    let epoch = client.owner_epoch();
    let old_session = client.session_id();
    let socket = temporary.path().join("c.sock");
    let reconnects = Arc::new(AtomicUsize::new(0));
    let count = reconnects.clone();
    let reconnect: ReconnectFactory = Arc::new(move || {
        let socket = socket.clone();
        let count = count.clone();
        Box::pin(async move {
            count.fetch_add(1, Ordering::SeqCst);
            LocalAgentClient::connect(socket).await
        })
    });
    let access = RecoveringAgentClient::new(client, reconnect);
    let target = execution(
        access
            .execute(create(epoch, temporary.path()))
            .await
            .unwrap(),
    );
    let old_view = ViewRef {
        session_id: old_session,
        view_id: Uuid::new_v4(),
    };
    access
        .show_view(DisplayUpdate::Displayed {
            view: old_view,
            expected_version: 0,
            execution: target,
            host: None,
        })
        .await
        .unwrap();
    let mut observed = access
        .observe(ObservationRequest::Agent {
            execution: target,
            detail: AgentDetailQuery::Snapshot,
            after: None,
        })
        .await
        .unwrap();
    connections.lock().await[0].abort();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let update = observed.recv().await.unwrap();
            if matches!(
                update.change,
                ObservationChange::Reset { .. } | ObservationChange::Resync { .. }
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(access.owner_epoch(), epoch);
    assert_ne!(access.session_id(), old_session);
    assert_eq!(access.connection_revision(), 2);
    assert_eq!(reconnects.load(Ordering::SeqCst), 1);
    assert_eq!(
        access
            .show_view(DisplayUpdate::Displayed {
                view: old_view,
                expected_version: 1,
                execution: target,
                host: None
            })
            .await
            .unwrap_err()
            .code,
        AccessErrorCode::AccessDenied
    );
    let fresh_view = ViewRef {
        session_id: access.session_id(),
        view_id: old_view.view_id,
    };
    access
        .show_view(DisplayUpdate::Displayed {
            view: fresh_view,
            expected_version: 0,
            execution: target,
            host: None,
        })
        .await
        .unwrap();
    assert_eq!(agents(&access).await[0].views.len(), 1);
    drop(observed);
    listener.abort();
    for connection in connections.lock().await.iter() {
        connection.abort();
    }
    factory.shutdown().await;
}

#[tokio::test]
async fn reconnecting_a_lost_command_ack_queries_the_original_receipt_without_replaying() {
    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let gate = Arc::new(Notify::new());
    let factory = Arc::new(FixtureFactory {
        gate: Some(gate.clone()),
        ..FixtureFactory::new(vec![])
    });
    let owner = Arc::new(OwnerServer::new(Uuid::new_v4(), factory.clone()));
    let (client, connections, listener) = controlled_connections(temporary.path(), owner).await;
    let socket = temporary.path().join("c.sock");
    let reconnect: ReconnectFactory = Arc::new(move || {
        let socket = socket.clone();
        Box::pin(async move { LocalAgentClient::connect(socket).await })
    });
    let access = RecoveringAgentClient::new(client, reconnect);
    let command = create(access.owner_epoch(), temporary.path());
    let sent = command.clone();
    let cloned = access.clone();
    let executing = tokio::spawn(async move { cloned.execute(sent).await.unwrap() });
    tokio::time::timeout(Duration::from_secs(3), async {
        while factory.started.load(Ordering::SeqCst) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    connections.lock().await[0].abort();
    let accepted = tokio::time::timeout(Duration::from_secs(3), executing)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        accepted.state,
        ReceiptState::Accepted | ReceiptState::Running
    ));
    assert_eq!(accepted.command_id, command.command_id);
    assert_eq!(factory.started.load(Ordering::SeqCst), 1);
    gate.notify_one();
    tokio::time::timeout(Duration::from_secs(3), async { loop {
        let observation = access.observe(ObservationRequest::Receipt { epoch: command.owner_epoch, command_id: command.command_id }).await.unwrap();
        if matches!(observation.initial, ObservationValue::Receipt(Some(receipt)) if matches!(receipt.state, ReceiptState::Completed)) { break; }
        tokio::task::yield_now().await;
    }}).await.unwrap();
    assert_eq!(agents(&access).await.len(), 1);
    assert_eq!(factory.started.load(Ordering::SeqCst), 1);
    listener.abort();
    for connection in connections.lock().await.iter() {
        connection.abort();
    }
    factory.shutdown().await;
}

#[tokio::test]
async fn record_preview_is_read_only_and_concurrent_resume_reuses_one_live_execution() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("state");
    let first_factory = Arc::new(FixtureFactory::new(vec![support::text("Recorded result.")]));
    let first = OwnerServer::new(Uuid::new_v4(), first_factory.clone());
    let mut launch = LaunchSpec::embedded(temporary.path().to_path_buf());
    launch.config_root = Some(root.clone());
    let created = first
        .execute(envelope(
            first.owner_epoch(),
            AgentCommand::Create {
                launch: launch.clone(),
                initial: Some(UserInput::Text {
                    text: "Record the result".into(),
                    contexts: vec![],
                    paths: vec![],
                }),
                origin: None,
            },
        ))
        .await
        .unwrap()
        .into_result()
        .unwrap();
    let CommandResult::Created {
        execution: target,
        snapshot,
        ..
    } = created
    else {
        panic!("expected Create");
    };
    let record = TranscriptRef {
        store_root: root.clone(),
        chat_id: snapshot.conversation.id,
    };
    tokio::time::timeout(Duration::from_secs(3), async { loop {
        let snapshot = first.observe(ObservationRequest::Agent { execution: target, detail: AgentDetailQuery::Snapshot, after: None }).await.unwrap();
        if matches!(snapshot.initial, ObservationValue::Agent(snapshot) if snapshot.conversation.loops.iter().any(|goal| goal.state == tickloop_agent::runtime::LoopState::Completed)) { break; }
        tokio::task::yield_now().await;
    }}).await.unwrap();
    first
        .execute(envelope(
            first.owner_epoch(),
            AgentCommand::FlushTranscripts,
        ))
        .await
        .unwrap()
        .into_result()
        .unwrap();
    first_factory.shutdown().await;
    let factory = Arc::new(FixtureFactory::new(vec![]));
    let owner = OwnerServer::new(Uuid::new_v4(), factory.clone());
    let preview = owner
        .observe(ObservationRequest::Transcript {
            record: record.clone(),
            page: Page {
                offset: 0,
                limit: 256,
            },
        })
        .await
        .unwrap();
    assert!(
        matches!(preview.initial, ObservationValue::Transcript(transcript) if transcript.messages.iter().any(|entry| entry.message.content.iter().any(|block| matches!(block, tickloop_agent::model::Content::Text { text } if text == "Recorded result."))))
    );
    assert_eq!(factory.started.load(Ordering::SeqCst), 0);
    assert!(agents(&owner).await.is_empty());
    let command = || {
        envelope(
            owner.owner_epoch(),
            AgentCommand::LoadRecord {
                record: record.clone(),
                as_new: false,
                launch: Some(launch.clone()),
            },
        )
    };
    let (one, two) = tokio::join!(owner.execute(command()), owner.execute(command()));
    assert_eq!(execution(one.unwrap()), execution(two.unwrap()));
    assert_eq!(factory.started.load(Ordering::SeqCst), 1);
    assert_eq!(agents(&owner).await.len(), 1);
    assert!(factory.model.requests.lock().unwrap().is_empty());
    factory.shutdown().await;
}

#[tokio::test]
async fn a_recovering_client_fences_an_old_execution_when_the_owner_epoch_changes() {
    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let socket = temporary.path().join("owner.sock");
    let factory = Arc::new(FixtureFactory::new(vec![]));
    let owner = Arc::new(OwnerServer::new(Uuid::new_v4(), factory.clone()));
    let listener = UnixListener::bind(&socket).unwrap();
    let epoch = owner.owner_epoch();
    let old_server = tokio::spawn(serve_owner_listener(listener, owner, epoch));
    let initial = LocalAgentClient::connect(&socket).await.unwrap();
    let old_epoch = initial.owner_epoch();
    let old_session = initial.session_id();
    let target = execution(
        initial
            .execute(create(old_epoch, temporary.path()))
            .await
            .unwrap(),
    );
    let endpoint = socket.clone();
    let reconnect: ReconnectFactory = Arc::new(move || {
        let endpoint = endpoint.clone();
        Box::pin(async move { LocalAgentClient::connect(endpoint).await })
    });
    let access = RecoveringAgentClient::new(initial.clone(), reconnect);
    let mut observation = access
        .observe(ObservationRequest::Agent {
            execution: target,
            detail: AgentDetailQuery::Snapshot,
            after: None,
        })
        .await
        .unwrap();
    initial.shutdown_owner().await.unwrap();
    old_server.await.unwrap().unwrap();
    factory.shutdown().await;
    std::fs::remove_file(&socket).unwrap();
    let replacement_factory = Arc::new(FixtureFactory::new(vec![]));
    let replacement_owner = Arc::new(OwnerServer::new(
        Uuid::new_v4(),
        replacement_factory.clone(),
    ));
    let epoch = replacement_owner.owner_epoch();
    let listener = UnixListener::bind(&socket).unwrap();
    let new_server = tokio::spawn(serve_owner_listener(listener, replacement_owner, epoch));
    let error = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match observation.recv().await {
                Err(error) => break error,
                Ok(_) => continue,
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(error.code, AccessErrorCode::EpochChanged);
    assert_ne!(access.owner_epoch(), old_epoch);
    assert_ne!(access.session_id(), old_session);
    assert!(agents(&access).await.is_empty());
    let rejected = access
        .execute(envelope(
            old_epoch,
            AgentCommand::Submit {
                execution: target,
                target_loop: None,
                input: UserInput::Text {
                    text: "Must not silently continue".into(),
                    contexts: vec![],
                    paths: vec![],
                },
                origin: None,
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(rejected.code, AccessErrorCode::EpochChanged);
    assert_eq!(
        access
            .show_view(DisplayUpdate::Scene {
                view: ViewRef {
                    session_id: old_session,
                    view_id: Uuid::new_v4()
                },
                expected_version: 0,
                host: None
            })
            .await
            .unwrap_err()
            .code,
        AccessErrorCode::AccessDenied
    );
    let replacement = LocalAgentClient::connect(socket).await.unwrap();
    replacement.shutdown_owner().await.unwrap();
    new_server.await.unwrap().unwrap();
    replacement_factory.shutdown().await;
}

#[tokio::test]
async fn a_lost_user_shell_agent_keeps_its_task_title_in_the_owner_directory() {
    let temporary = tempfile::tempdir().unwrap();
    let factory = Arc::new(FixtureFactory::new(vec![]));
    let owner = OwnerServer::new(Uuid::new_v4(), factory.clone());
    let target = execution(
        owner
            .execute(envelope(
                owner.owner_epoch(),
                AgentCommand::Create {
                    launch: LaunchSpec::embedded(temporary.path().to_path_buf()),
                    initial: Some(UserInput::Shell {
                        command: "printf alpha".into(),
                        background: true,
                    }),
                    origin: None,
                },
            ))
            .await
            .unwrap(),
    );
    let live = agents(&owner).await;
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].execution, target);
    assert_eq!(live[0].title, "!printf alpha");
    factory.availability.lock().await[0].store(false, Ordering::SeqCst);
    let lost = agents(&owner).await;
    assert_eq!(lost[0].availability, ExecutionAvailability::Lost);
    assert_eq!(lost[0].title, "!printf alpha");
    factory.shutdown().await;
}

#[tokio::test]
async fn a_lost_worker_stays_visible_and_only_explicit_resume_creates_a_new_execution() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("state");
    let factory = Arc::new(FixtureFactory::new(vec![support::text(
        "Saved before disconnect.",
    )]));
    let owner = OwnerServer::new(Uuid::new_v4(), factory.clone());
    let mut launch = LaunchSpec::embedded(temporary.path().to_path_buf());
    launch.config_root = Some(root.clone());
    let target = execution(
        owner
            .execute(envelope(
                owner.owner_epoch(),
                AgentCommand::Create {
                    launch: launch.clone(),
                    initial: Some(UserInput::Text {
                        text: "Retain this record".into(),
                        contexts: vec![],
                        paths: vec![],
                    }),
                    origin: None,
                },
            ))
            .await
            .unwrap(),
    );
    tokio::time::timeout(Duration::from_secs(3), async { loop {
        let snapshot = owner.observe(ObservationRequest::Agent { execution: target, detail: AgentDetailQuery::Snapshot, after: None }).await.unwrap();
        if matches!(snapshot.initial, ObservationValue::Agent(snapshot) if snapshot.conversation.loops.iter().any(|goal| goal.state == tickloop_agent::runtime::LoopState::Completed)) { break; }
        tokio::task::yield_now().await;
    }}).await.unwrap();
    let saved = owner
        .observe(ObservationRequest::SavedRecords {
            store_root: Some(root),
            workdir: temporary.path().to_path_buf(),
            page: Page {
                offset: 0,
                limit: 256,
            },
        })
        .await
        .unwrap();
    assert!(matches!(saved.initial, ObservationValue::Records(records) if records.len() == 1));
    let before = agents(&owner).await;
    let record = before[0].record.clone().unwrap();
    factory.availability.lock().await[0].store(false, Ordering::SeqCst);
    let lost = agents(&owner).await;
    assert_eq!(lost.len(), 1);
    assert_eq!(lost[0].execution, target);
    assert_eq!(lost[0].availability, ExecutionAvailability::Lost);
    assert_eq!(lost[0].record.as_ref(), Some(&record));
    assert_eq!(factory.started.load(Ordering::SeqCst), 1);
    let rejected = owner
        .execute(envelope(
            owner.owner_epoch(),
            AgentCommand::Submit {
                execution: target,
                target_loop: None,
                input: UserInput::Text {
                    text: "Must not restart".into(),
                    contexts: vec![],
                    paths: vec![],
                },
                origin: None,
            },
        ))
        .await
        .unwrap();
    assert_eq!(
        rejected.error.unwrap().code,
        AccessErrorCode::StaleExecution
    );
    factory.runtimes.lock().await[0].shutdown().await;
    factory.exits.lock().await[0].store(true, Ordering::SeqCst);
    let restored = execution(
        owner
            .execute(envelope(
                owner.owner_epoch(),
                AgentCommand::LoadRecord {
                    record: record.clone(),
                    as_new: false,
                    launch: Some(launch),
                },
            ))
            .await
            .unwrap(),
    );
    assert_ne!(restored, target);
    assert_eq!(factory.started.load(Ordering::SeqCst), 2);
    let after = agents(&owner).await;
    assert_eq!(after.len(), 2);
    assert!(after.iter().any(
        |entry| entry.execution == target && entry.availability == ExecutionAvailability::Lost
    ));
    assert!(
        after.iter().any(|entry| entry.execution == restored
            && entry.availability == ExecutionAvailability::Live)
    );
    assert_eq!(factory.model.requests.lock().unwrap().len(), 1);
    factory.shutdown().await;
}

/// A scripted model that holds each request until the test releases it, so a run stays in flight.
struct HeldModel {
    script: support::ScriptedModel,
    entered: Notify,
    release: Notify,
}

#[async_trait]
impl tickloop_agent::model::ModelClient for HeldModel {
    async fn generate_title(
        &self,
        request: tickloop_agent::model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent::model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent::model::ModelOutput, tickloop_agent::model::ModelError> {
        Ok(support::title_output(request))
    }

    fn identity(&self) -> tickloop_agent::model::ModelIdentity {
        tickloop_agent::model::ModelClient::identity(&self.script)
    }
    async fn complete(
        &self,
        request: tickloop_agent::model::ModelRequest,
        events: tokio::sync::mpsc::UnboundedSender<tickloop_agent::model::ModelDelta>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent::model::ModelOutput, tickloop_agent::model::ModelError> {
        self.entered.notify_one();
        self.release.notified().await;
        tickloop_agent::model::ModelClient::complete(&self.script, request, events, cancel).await
    }
}

/// Serves every started worker runtime behind its own listener, as a worker process does, so a
/// test can close the owner's link or stop the listener while the runtime itself keeps running.
struct LinkFixtureFactory {
    directory: PathBuf,
    model: Arc<HeldModel>,
    started: AtomicUsize,
    workers: Mutex<Vec<LinkWorker>>,
}

struct LinkWorker {
    access: Arc<dyn AgentAccess>,
    served: Arc<dyn AgentAccess>,
    epoch: Uuid,
    endpoint: PathBuf,
    runtime: Agent,
    listener: Option<tokio::task::JoinHandle<()>>,
    connections: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    accepted: Arc<AtomicUsize>,
    exited: Arc<AtomicBool>,
}

impl LinkFixtureFactory {
    fn new(directory: &Path, replies: Vec<Vec<tickloop_agent::model::Content>>) -> Self {
        Self {
            directory: directory.to_path_buf(),
            model: Arc::new(HeldModel {
                script: support::ScriptedModel::new(replies),
                entered: Notify::new(),
                release: Notify::new(),
            }),
            started: AtomicUsize::new(0),
            workers: Mutex::new(Vec::new()),
        }
    }
    async fn close_links(&self) {
        for worker in self.workers.lock().await.iter() {
            for connection in worker.connections.lock().await.drain(..) {
                connection.abort();
                let _ = connection.await;
            }
        }
    }
    async fn stop_listening(&self) {
        for worker in self.workers.lock().await.iter_mut() {
            if let Some(listener) = worker.listener.take() {
                listener.abort();
                let _ = listener.await;
            }
        }
        self.close_links().await;
    }
    /// Serves the still running worker on its endpoint again after `stop_listening`.
    async fn listen_again(&self, index: usize) {
        let mut workers = self.workers.lock().await;
        let worker = &mut workers[index];
        std::fs::remove_file(&worker.endpoint).unwrap();
        worker.listener = Some(accept_links(
            UnixListener::bind(&worker.endpoint).unwrap(),
            worker.served.clone(),
            worker.epoch,
            worker.connections.clone(),
            worker.accepted.clone(),
        ));
    }
    async fn served(&self, index: usize) -> Arc<dyn AgentAccess> {
        self.workers.lock().await[index].served.clone()
    }
    async fn accepted(&self, index: usize) -> usize {
        self.workers.lock().await[index]
            .accepted
            .load(Ordering::SeqCst)
    }
    async fn exit(&self, index: usize) {
        let workers = self.workers.lock().await;
        workers[index].runtime.shutdown().await;
        workers[index].exited.store(true, Ordering::SeqCst);
    }
    async fn shutdown(&self) {
        self.model.release.notify_one();
        self.stop_listening().await;
        for worker in self.workers.lock().await.iter() {
            worker.runtime.shutdown().await;
        }
    }
}

#[async_trait]
impl WorkerFactory for LinkFixtureFactory {
    async fn start(
        &self,
        launch: &LaunchSpec,
        epoch: Uuid,
    ) -> Result<Arc<dyn AgentAccess>, AccessError> {
        let index = self.started.fetch_add(1, Ordering::SeqCst);
        let runtime = Agent::open_embedded(
            launch.config_root.clone().unwrap(),
            support::config(),
            self.model.clone(),
        )
        .unwrap();
        let served: Arc<dyn AgentAccess> =
            Arc::new(EmbeddedAgentAccess::for_worker(runtime.clone(), epoch));
        let endpoint = self.directory.join(format!("w-{index}.sock"));
        let connections = Arc::new(Mutex::new(Vec::new()));
        let accepted = Arc::new(AtomicUsize::new(0));
        let listener = accept_links(
            UnixListener::bind(&endpoint).unwrap(),
            served.clone(),
            epoch,
            connections.clone(),
            accepted.clone(),
        );
        let exited = Arc::new(AtomicBool::new(false));
        let client = LocalAgentClient::connect(&endpoint).await?;
        let running = exited.clone();
        let access: Arc<dyn AgentAccess> =
            Arc::new(worker_link(client, endpoint.clone(), move || {
                !running.load(Ordering::SeqCst)
            }));
        self.workers.lock().await.push(LinkWorker {
            access: access.clone(),
            served,
            epoch,
            endpoint,
            runtime,
            listener: Some(listener),
            connections,
            accepted,
            exited,
        });
        Ok(access)
    }
    async fn exited(&self, worker: &Arc<dyn AgentAccess>) -> bool {
        self.workers
            .lock()
            .await
            .iter()
            .find(|known| Arc::ptr_eq(&known.access, worker))
            .is_some_and(|known| known.exited.load(Ordering::SeqCst))
    }
}

fn accept_links(
    listener: UnixListener,
    served: Arc<dyn AgentAccess>,
    epoch: Uuid,
    connections: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    accepted: Arc<AtomicUsize>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            accepted.fetch_add(1, Ordering::SeqCst);
            let served = served.clone();
            connections.lock().await.push(tokio::spawn(async move {
                let _ = serve_connection(stream, served, epoch).await;
            }));
        }
    })
}

fn record_launch(workdir: &Path) -> LaunchSpec {
    let mut launch = LaunchSpec::embedded(workdir.to_path_buf());
    launch.config_root = Some(workdir.join("state"));
    launch
}

async fn create_running(
    owner: &OwnerServer,
    model: &HeldModel,
    launch: &LaunchSpec,
) -> ExecutionRef {
    let target = execution(
        owner
            .execute(envelope(
                owner.owner_epoch(),
                AgentCommand::Create {
                    launch: launch.clone(),
                    initial: Some(UserInput::Text {
                        text: "Keep working while the link drops".into(),
                        contexts: vec![],
                        paths: vec![],
                    }),
                    origin: None,
                },
            ))
            .await
            .unwrap(),
    );
    tokio::time::timeout(Duration::from_secs(3), model.entered.notified())
        .await
        .unwrap();
    target
}

async fn wait_until_completed(access: &dyn AgentAccess, target: ExecutionRef) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let observed = access
                .observe(ObservationRequest::Agent {
                    execution: target,
                    detail: AgentDetailQuery::Snapshot,
                    after: None,
                })
                .await
                .unwrap();
            if matches!(observed.initial, ObservationValue::Agent(snapshot) if snapshot.conversation.loops.iter().any(|goal| goal.state == tickloop_agent::runtime::LoopState::Completed))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_live_worker_whose_owner_link_closes_keeps_its_execution_and_is_never_restarted() {
    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let factory = Arc::new(LinkFixtureFactory::new(
        temporary.path(),
        vec![support::text("Finished after the link closed.")],
    ));
    let owner = OwnerServer::new(Uuid::new_v4(), factory.clone());
    let launch = record_launch(temporary.path());
    let target = create_running(&owner, &factory.model, &launch).await;

    factory.close_links().await;
    let relinked = agents(&owner).await;
    assert_eq!(relinked.len(), 1);
    assert_eq!(relinked[0].execution, target);
    assert_eq!(relinked[0].availability, ExecutionAvailability::Live);
    assert_eq!(factory.accepted(0).await, 2);

    factory.model.release.notify_one();
    wait_until_completed(&owner, target).await;
    assert_eq!(factory.model.script.requests.lock().unwrap().len(), 1);
    let record = agents(&owner).await[0].record.clone().unwrap();
    let loaded = execution(
        owner
            .execute(envelope(
                owner.owner_epoch(),
                AgentCommand::LoadRecord {
                    record,
                    as_new: false,
                    launch: Some(launch),
                },
            ))
            .await
            .unwrap(),
    );
    assert_eq!(loaded, target);
    assert_eq!(factory.started.load(Ordering::SeqCst), 1);
    factory.shutdown().await;
}

#[tokio::test]
async fn a_lost_execution_whose_worker_is_still_running_cannot_be_resumed_twice() {
    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let factory = Arc::new(LinkFixtureFactory::new(
        temporary.path(),
        vec![support::text("Finished without its owner.")],
    ));
    let owner = OwnerServer::new(Uuid::new_v4(), factory.clone());
    let launch = record_launch(temporary.path());
    let target = create_running(&owner, &factory.model, &launch).await;

    factory.stop_listening().await;
    let lost = agents(&owner).await;
    assert_eq!(lost.len(), 1);
    assert_eq!(lost[0].execution, target);
    assert_eq!(lost[0].availability, ExecutionAvailability::Lost);
    let record = lost[0].record.clone().unwrap();
    let submitted = owner
        .execute(envelope(
            owner.owner_epoch(),
            AgentCommand::Submit {
                execution: target,
                target_loop: None,
                input: UserInput::Text {
                    text: "Must not reach a second runtime".into(),
                    contexts: vec![],
                    paths: vec![],
                },
                origin: None,
            },
        ))
        .await
        .unwrap();
    assert_eq!(
        submitted.error.unwrap().code,
        AccessErrorCode::StaleExecution
    );
    let resume = |record: TranscriptRef| {
        envelope(
            owner.owner_epoch(),
            AgentCommand::LoadRecord {
                record,
                as_new: false,
                launch: Some(launch.clone()),
            },
        )
    };
    let refused = owner
        .execute(resume(record.clone()))
        .await
        .unwrap()
        .error
        .expect("a worker that may still be running must not get a second execution");
    assert_eq!(factory.started.load(Ordering::SeqCst), 1);
    assert!(
        refused.message.contains("outcome is unknown"),
        "{refused:?}"
    );
    assert!(!refused.message.contains("cancel"), "{refused:?}");

    factory.model.release.notify_one();
    wait_until_completed(factory.served(0).await.as_ref(), target).await;
    factory.exit(0).await;
    let restored = execution(owner.execute(resume(record)).await.unwrap());
    assert_ne!(restored, target);
    assert_eq!(factory.started.load(Ordering::SeqCst), 2);
    assert_eq!(factory.model.script.requests.lock().unwrap().len(), 1);
    factory.shutdown().await;
}

#[tokio::test]
async fn a_lost_execution_stays_lost_when_its_still_running_worker_answers_again() {
    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let factory = Arc::new(LinkFixtureFactory::new(
        temporary.path(),
        vec![support::text("Finished without its owner.")],
    ));
    let owner = OwnerServer::new(Uuid::new_v4(), factory.clone());
    let launch = record_launch(temporary.path());
    let target = create_running(&owner, &factory.model, &launch).await;
    owner
        .show_view(DisplayUpdate::Displayed {
            view: ViewRef {
                session_id: Uuid::new_v4(),
                view_id: Uuid::new_v4(),
            },
            expected_version: 0,
            execution: target,
            host: None,
        })
        .await
        .unwrap();
    factory.stop_listening().await;
    let lost = agents(&owner).await;
    assert_eq!(lost[0].availability, ExecutionAvailability::Lost);
    let record = lost[0].record.clone().unwrap();

    factory.listen_again(0).await;
    let answered = agents(&owner).await;
    assert_eq!(
        factory.accepted(0).await,
        2,
        "the owner reached the worker again"
    );
    assert_eq!(answered.len(), 1);
    assert_eq!(answered[0].execution, target);
    assert_eq!(answered[0].availability, ExecutionAvailability::Lost);
    assert!(answered[0].views.is_empty());
    let submitted = owner
        .execute(envelope(
            owner.owner_epoch(),
            AgentCommand::Submit {
                execution: target,
                target_loop: None,
                input: UserInput::Text {
                    text: "Must stay fenced".into(),
                    contexts: vec![],
                    paths: vec![],
                },
                origin: None,
            },
        ))
        .await
        .unwrap();
    assert_eq!(
        submitted.error.unwrap().code,
        AccessErrorCode::StaleExecution
    );
    let resume = |record: TranscriptRef| {
        envelope(
            owner.owner_epoch(),
            AgentCommand::LoadRecord {
                record,
                as_new: false,
                launch: Some(launch.clone()),
            },
        )
    };
    let refused = owner.execute(resume(record.clone())).await.unwrap();
    assert!(refused.error.is_some(), "{refused:?}");
    assert_eq!(factory.started.load(Ordering::SeqCst), 1);

    factory.model.release.notify_one();
    wait_until_completed(factory.served(0).await.as_ref(), target).await;
    factory.exit(0).await;
    let restored = execution(owner.execute(resume(record)).await.unwrap());
    assert_ne!(restored, target);
    assert_eq!(factory.started.load(Ordering::SeqCst), 2);
    factory.shutdown().await;
}

#[tokio::test]
async fn an_execution_feed_can_close_while_the_owner_connection_stays_available() {
    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let runtime = Agent::new_embedded(
        support::config(),
        Arc::new(support::ScriptedModel::new(vec![])),
    );
    let access = Arc::new(EmbeddedAgentAccess::new(runtime));
    let socket = temporary.path().join("feed.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let epoch = access.owner_epoch();
    let service = access.clone();
    let server = tokio::spawn(serve_listener(listener, service, epoch));
    let client = LocalAgentClient::connect(socket).await.unwrap();
    let command = create(epoch, temporary.path());
    let target = execution(client.execute(command.clone()).await.unwrap());
    let mut observation = client
        .observe(ObservationRequest::Agent {
            execution: target,
            detail: AgentDetailQuery::Snapshot,
            after: None,
        })
        .await
        .unwrap();
    access.shutdown().await;
    let error = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Err(error) = observation.recv().await {
                break error;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(error.code, AccessErrorCode::Unavailable);
    assert!(client.is_connected());
    let receipt = client
        .observe(ObservationRequest::Receipt {
            epoch,
            command_id: command.command_id,
        })
        .await
        .unwrap();
    assert!(
        matches!(receipt.initial, ObservationValue::Receipt(Some(receipt)) if matches!(receipt.state, ReceiptState::Completed))
    );
    server.abort();
}

#[tokio::test]
async fn a_directory_change_during_snapshot_handoff_is_delivered_after_the_snapshot_cursor() {
    let temporary = tempfile::tempdir().unwrap();
    let gate = Arc::new(SnapshotGate::default());
    let factory = Arc::new(FixtureFactory {
        snapshot_gate: Some(gate.clone()),
        ..FixtureFactory::new(vec![])
    });
    let owner = OwnerServer::new(Uuid::new_v4(), factory.clone());
    owner
        .execute(create(owner.owner_epoch(), temporary.path()))
        .await
        .unwrap()
        .into_result()
        .unwrap();
    gate.next.store(true, Ordering::SeqCst);
    let (observation, second) = tokio::time::timeout(Duration::from_secs(3), async {
        tokio::join!(
            owner.observe(ObservationRequest::Overview {
                filter: AgentFilter::default(),
                page: Page {
                    offset: 0,
                    limit: 256
                },
                after: None
            }),
            async {
                gate.entered.notified().await;
                let second = owner
                    .execute(create(owner.owner_epoch(), temporary.path()))
                    .await;
                gate.release.notify_one();
                second
            }
        )
    })
    .await
    .unwrap();
    let mut observation = observation.unwrap();
    let second = execution(second.unwrap());
    assert!(
        matches!(&observation.initial, ObservationValue::Overview(entries) if entries.len() == 1 && entries[0].execution != second)
    );
    let update = tokio::time::timeout(Duration::from_secs(3), observation.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(update.cursor.sequence > observation.cursor.sequence);
    assert!(
        agents(&owner)
            .await
            .iter()
            .any(|entry| entry.execution == second)
    );
    factory.shutdown().await;
}

struct OversizedResponseAccess {
    inner: EmbeddedAgentAccess,
    text: String,
    enlarge_receipts: AtomicBool,
    executions: AtomicUsize,
}

const OVERSIZED_RESPONSE_TIMEOUT: Duration = Duration::from_secs(180);

#[async_trait]
impl AgentAccess for OversizedResponseAccess {
    async fn observe(&self, query: ObservationRequest) -> Result<Observation, AccessError> {
        let warnings = matches!(
            &query,
            ObservationRequest::Agent {
                detail: AgentDetailQuery::Warnings,
                ..
            }
        );
        let mut observation = self.inner.observe(query).await?;
        if warnings {
            observation.initial = ObservationValue::Warnings(vec![self.text.clone()]);
        } else if self.enlarge_receipts.load(Ordering::SeqCst)
            && let ObservationValue::Receipt(Some(receipt)) = &mut observation.initial
        {
            receipt.result = Some(CommandResult::Text(self.text.clone()));
        }
        Ok(observation)
    }
    async fn execute(&self, command: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        let mut receipt = self.inner.execute(command).await?;
        receipt.result = Some(CommandResult::Text(self.text.clone()));
        Ok(receipt)
    }
    async fn show_view(&self, update: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
        self.inner.show_view(update).await
    }
}

async fn oversized_response_fixture(
    directory: &Path,
) -> (
    LocalAgentClient,
    Arc<OversizedResponseAccess>,
    ExecutionRef,
    tokio::task::JoinHandle<Result<(), AccessError>>,
) {
    let runtime = Agent::new_embedded(
        support::config(),
        Arc::new(support::ScriptedModel::new(vec![])),
    );
    let inner = EmbeddedAgentAccess::new(runtime);
    let epoch = inner.owner_epoch();
    let target = execution(inner.execute(create(epoch, directory)).await.unwrap());
    let access = Arc::new(OversizedResponseAccess {
        inner,
        text: "x".repeat(MAX_FRAME_BYTES),
        enlarge_receipts: AtomicBool::new(true),
        executions: AtomicUsize::new(0),
    });
    let socket = directory.join("oversized.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(serve_listener(listener, access.clone(), epoch));
    let client = LocalAgentClient::connect(socket).await.unwrap();
    (client, access, target, server)
}

#[tokio::test]
async fn an_oversized_initial_observation_returns_a_typed_error_and_keeps_ipc_available() {
    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let (client, access, target, server) = oversized_response_fixture(temporary.path()).await;
    let error = tokio::time::timeout(
        OVERSIZED_RESPONSE_TIMEOUT,
        client.observe(ObservationRequest::Agent {
            execution: target,
            detail: AgentDetailQuery::Warnings,
            after: None,
        }),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(error.code, AccessErrorCode::PayloadTooLarge);
    assert!(client.is_connected());
    let small = client
        .observe(ObservationRequest::Agent {
            execution: target,
            detail: AgentDetailQuery::Snapshot,
            after: None,
        })
        .await
        .unwrap();
    assert!(
        matches!(small.initial, ObservationValue::Agent(snapshot) if snapshot.execution == target)
    );
    assert_eq!(access.executions.load(Ordering::SeqCst), 0);
    access.inner.shutdown().await;
    server.abort();
}

#[tokio::test]
async fn an_oversized_accepted_receipt_stays_queryable_without_replaying_the_command() {
    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let (client, access, target, server) = oversized_response_fixture(temporary.path()).await;
    let command = envelope(
        client.owner_epoch(),
        AgentCommand::Configure {
            provider: None,
            execution: target,
            change: ConfigurationChange::Directories {
                workdir: temporary.path().to_path_buf(),
                additional: vec![],
            },
        },
    );
    let error = tokio::time::timeout(OVERSIZED_RESPONSE_TIMEOUT, client.execute(command.clone()))
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(error.code, AccessErrorCode::OutcomeUnknown);
    assert!(client.is_connected());
    let query = ObservationRequest::Receipt {
        epoch: command.owner_epoch,
        command_id: command.command_id,
    };
    let error = tokio::time::timeout(OVERSIZED_RESPONSE_TIMEOUT, client.observe(query.clone()))
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(error.code, AccessErrorCode::PayloadTooLarge);
    access.enlarge_receipts.store(false, Ordering::SeqCst);
    let known = client.observe(query).await.unwrap();
    assert!(
        matches!(known.initial, ObservationValue::Receipt(Some(receipt)) if receipt.command_id == command.command_id && receipt.target == Some(target) && matches!(receipt.state, ReceiptState::Completed))
    );
    assert_eq!(access.executions.load(Ordering::SeqCst), 1);
    let snapshot = client
        .observe(ObservationRequest::Agent {
            execution: target,
            detail: AgentDetailQuery::Snapshot,
            after: None,
        })
        .await
        .unwrap();
    assert!(
        matches!(snapshot.initial, ObservationValue::Agent(snapshot) if snapshot.conversation.workdir == temporary.path().canonicalize().unwrap())
    );
    access.inner.shutdown().await;
    server.abort();
}

fn host_location(socket: &Path, pane: &tick_connect::herdr::Pane) -> HostLocation {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(socket).unwrap();
    HostLocation {
        host_instance: format!("herdr:{}:{}", metadata.dev(), metadata.ino()),
        socket: socket.into(),
        workspace: Some(pane.workspace_id.clone()),
        tab: Some(pane.tab_id.clone()),
        pane: Some(pane.pane_id.clone()),
        terminal: Some(pane.terminal_id.clone()),
        client: Some("fixture-client".into()),
    }
}

fn serve_host_socket(
    listener: UnixListener,
    pane: Arc<Mutex<tick_connect::herdr::Pane>>,
    requests: Arc<AtomicUsize>,
) -> tokio::task::JoinHandle<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let pane = pane.clone();
            let requests = requests.clone();
            tokio::spawn(async move {
                let (reader, mut writer) = stream.into_split();
                let mut line = String::new();
                BufReader::new(reader).read_line(&mut line).await.unwrap();
                let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                assert_eq!(request["method"], "pane.current");
                requests.fetch_add(1, Ordering::SeqCst);
                let mut response = serde_json::to_vec(&serde_json::json!({"id":request["id"],"result":{"pane":pane.lock().await.clone()}})).unwrap();
                response.push(b'\n');
                writer.write_all(&response).await.unwrap();
            });
        }
    })
}

fn fixture_host_pane() -> tick_connect::herdr::Pane {
    tick_connect::herdr::Pane {
        pane_id: "pane-a".into(),
        terminal_id: "terminal-a".into(),
        workspace_id: "workspace-a".into(),
        tab_id: "tab-a".into(),
        cwd: None,
        foreground_cwd: None,
        title: None,
        tokens: Default::default(),
    }
}

#[tokio::test]
async fn replacing_the_host_socket_rejects_a_frozen_view_even_with_identical_pane_ids() {
    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let socket = temporary.path().join("herdr.sock");
    let pane = Arc::new(Mutex::new(fixture_host_pane()));
    let listener = UnixListener::bind(&socket).unwrap();
    let host = serve_host_socket(listener, pane.clone(), Arc::new(AtomicUsize::new(0)));
    let frozen = host_location(&socket, &*pane.lock().await);
    let factory = Arc::new(FixtureFactory::new(vec![]));
    let owner = Arc::new(OwnerServer::new(Uuid::new_v4(), factory.clone()));
    let (client, server) = serve(temporary.path(), owner).await;
    let target = execution(
        client
            .execute(create(client.owner_epoch(), temporary.path()))
            .await
            .unwrap(),
    );
    let view = ViewRef {
        session_id: client.session_id(),
        view_id: Uuid::new_v4(),
    };
    let displayed = client
        .show_view(DisplayUpdate::Displayed {
            view,
            expected_version: 0,
            execution: target,
            host: Some(frozen.clone()),
        })
        .await
        .unwrap();
    std::fs::rename(&socket, temporary.path().join("original.sock")).unwrap();
    let listener = UnixListener::bind(&socket).unwrap();
    let requests = Arc::new(AtomicUsize::new(0));
    let replacement = serve_host_socket(listener, pane, requests.clone());
    let rejected = client
        .show_view(DisplayUpdate::Displayed {
            view,
            expected_version: displayed.version,
            execution: target,
            host: Some(frozen),
        })
        .await
        .unwrap_err();
    assert_eq!(rejected.code, AccessErrorCode::VersionConflict);
    assert_eq!(requests.load(Ordering::SeqCst), 0);
    assert_eq!(agents(&client).await[0].views[0].version, displayed.version);
    host.abort();
    replacement.abort();
    server.abort();
    factory.shutdown().await;
}

#[tokio::test]
async fn moving_a_source_pane_requires_a_new_display_version_before_the_next_input() {
    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let socket = temporary.path().join("herdr.sock");
    let pane = Arc::new(Mutex::new(fixture_host_pane()));
    let listener = UnixListener::bind(&socket).unwrap();
    let host = serve_host_socket(listener, pane.clone(), Arc::new(AtomicUsize::new(0)));
    let first = host_location(&socket, &*pane.lock().await);
    let factory = Arc::new(FixtureFactory::new(vec![support::text(
        "The moved source was accepted.",
    )]));
    let owner = Arc::new(OwnerServer::new(Uuid::new_v4(), factory.clone()));
    let (client, server) = serve(temporary.path(), owner).await;
    let target = execution(
        client
            .execute(create(client.owner_epoch(), temporary.path()))
            .await
            .unwrap(),
    );
    let view = ViewRef {
        session_id: client.session_id(),
        view_id: Uuid::new_v4(),
    };
    let displayed = client
        .show_view(DisplayUpdate::Displayed {
            view,
            expected_version: 0,
            execution: target,
            host: Some(first.clone()),
        })
        .await
        .unwrap();
    {
        let mut current = pane.lock().await;
        current.workspace_id = "workspace-b".into();
        current.tab_id = "tab-b".into();
    }
    let submit = |version, source: HostLocation| {
        envelope(
            client.owner_epoch(),
            AgentCommand::Submit {
                execution: target,
                target_loop: None,
                input: UserInput::Text {
                    text: "Accept this fixed source".into(),
                    contexts: vec![],
                    paths: vec![],
                },
                origin: Some(InteractionSource {
                    view,
                    display_version: version,
                    host: Some(source),
                }),
            },
        )
    };
    let rejected = client
        .execute(submit(displayed.version, first.clone()))
        .await
        .unwrap();
    assert_eq!(
        rejected.error.unwrap().code,
        AccessErrorCode::VersionConflict
    );
    let current = host_location(&socket, &*pane.lock().await);
    let refreshed = client
        .show_view(DisplayUpdate::Displayed {
            view,
            expected_version: displayed.version,
            execution: target,
            host: Some(current.clone()),
        })
        .await
        .unwrap();
    assert_eq!(refreshed.version, displayed.version + 1);
    assert_eq!(current.client, first.client);
    let old_version = client
        .execute(submit(displayed.version, first))
        .await
        .unwrap();
    assert_eq!(
        old_version.error.unwrap().code,
        AccessErrorCode::VersionConflict
    );
    client
        .execute(submit(refreshed.version, current.clone()))
        .await
        .unwrap()
        .into_result()
        .unwrap();
    assert_eq!(
        agents(&client).await[0].views[0].host.as_ref(),
        Some(&current)
    );
    tokio::time::timeout(Duration::from_secs(3), async {
        while factory.model.requests.lock().unwrap().len() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    host.abort();
    server.abort();
    factory.shutdown().await;
}

#[tokio::test]
async fn a_nonterminal_creation_receipt_keeps_the_console_dispatch_gate_closed() {
    use tickloop_agent::agent_console::AgentConsole;
    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let gate = Arc::new(Notify::new());
    let factory = Arc::new(FixtureFactory {
        gate: Some(gate.clone()),
        ..FixtureFactory::new(vec![support::text("The original dispatch finished.")])
    });
    let owner = Arc::new(OwnerServer::new(Uuid::new_v4(), factory.clone()));
    let (client, connections, listener) = controlled_connections(temporary.path(), owner).await;
    let socket = temporary.path().join("c.sock");
    let reconnect: ReconnectFactory = Arc::new(move || {
        let socket = socket.clone();
        Box::pin(async move { LocalAgentClient::connect(socket).await })
    });
    let access = Arc::new(RecoveringAgentClient::new(client, reconnect));
    let mut console = AgentConsole::new(
        access.clone(),
        access.owner_epoch(),
        temporary.path().into(),
        None,
    );
    let launch = LaunchSpec::embedded(temporary.path().into());
    let input = UserInput::Text {
        text: "Dispatch exactly once".into(),
        contexts: vec![],
        paths: vec![],
    };
    let sent_launch = launch.clone();
    let sent_input = input.clone();
    let executing = tokio::spawn(async move {
        let result = console.dispatch(sent_launch, sent_input, None).await;
        (console, result)
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        while factory.started.load(Ordering::SeqCst) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    connections.lock().await[0].abort();
    let (mut console, result) = tokio::time::timeout(Duration::from_secs(3), executing)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.unwrap_err().code, AccessErrorCode::OutcomeUnknown);
    let command_id = console.projection().unconfirmed_dispatch.unwrap();
    assert!(console.reconcile_dispatch().await.unwrap().is_none());
    assert_eq!(
        console
            .dispatch(launch, input, None)
            .await
            .unwrap_err()
            .code,
        AccessErrorCode::OutcomeUnknown
    );
    assert_eq!(factory.started.load(Ordering::SeqCst), 1);
    gate.notify_one();
    let receipt = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(receipt) = console.reconcile_dispatch().await.unwrap() {
                break receipt;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(receipt.command_id, command_id);
    assert!(matches!(receipt.state, ReceiptState::Completed));
    assert!(console.projection().unconfirmed_dispatch.is_none());
    assert_eq!(agents(access.as_ref()).await.len(), 1);
    assert_eq!(factory.started.load(Ordering::SeqCst), 1);
    listener.abort();
    for connection in connections.lock().await.iter() {
        connection.abort();
    }
    factory.shutdown().await;
}

#[tokio::test]
async fn failed_first_input_reports_the_created_agent_without_retrying_or_losing_it() {
    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let factory = Arc::new(FixtureFactory::new(vec![support::text(
        "Explicit correction worked.",
    )]));
    let owner = Arc::new(OwnerServer::new(Uuid::new_v4(), factory.clone()));
    let (client, server) = serve(temporary.path(), owner).await;
    // The runtime registers the Agent before it checks the first input's file references, so a
    // reference outside the workspace fails only after registration.
    let outside = tempfile::tempdir_in("/tmp").unwrap();
    let outside_file = outside.path().join("outside.txt");
    std::fs::write(&outside_file, "outside").unwrap();
    let command = envelope(
        client.owner_epoch(),
        AgentCommand::Create {
            launch: LaunchSpec::embedded(temporary.path().into()),
            initial: Some(UserInput::Text {
                text: "Read the referenced file".into(),
                contexts: vec![],
                paths: vec![outside_file],
            }),
            origin: None,
        },
    );
    let failed = client.execute(command.clone()).await.unwrap();
    assert!(matches!(failed.state, ReceiptState::Failed));
    let target = failed.target.expect("a created Agent remains addressable");
    let error = failed.error.as_ref().unwrap();
    assert!(
        error.message.contains(&format!(
            "Agent {} was created, but its first input was not submitted",
            target.agent_id
        )),
        "{error:?}"
    );
    assert!(
        error.message.contains("outside the configured workspace"),
        "{error:?}"
    );
    let duplicate = client.execute(command.clone()).await.unwrap();
    assert_eq!(duplicate.target, Some(target));
    assert!(matches!(duplicate.state, ReceiptState::Failed));
    assert_eq!(factory.started.load(Ordering::SeqCst), 1);
    assert!(factory.model.requests.lock().unwrap().is_empty());
    let observed = client
        .observe(ObservationRequest::Agent {
            execution: target,
            detail: AgentDetailQuery::Snapshot,
            after: None,
        })
        .await
        .unwrap();
    let ObservationValue::Agent(snapshot) = observed.initial else {
        panic!("expected snapshot");
    };
    assert!(snapshot.conversation.loops.is_empty());
    assert_eq!(agents(&client).await.len(), 1);
    let corrected = client
        .execute(envelope(
            client.owner_epoch(),
            AgentCommand::Submit {
                execution: target,
                target_loop: None,
                input: UserInput::Text {
                    text: "Submit the correction to the existing Agent".into(),
                    contexts: vec![],
                    paths: vec![],
                },
                origin: None,
            },
        ))
        .await
        .unwrap();
    assert!(matches!(corrected.state, ReceiptState::Completed));
    tokio::time::timeout(Duration::from_secs(3), async {
        while factory.model.requests.lock().unwrap().len() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(factory.started.load(Ordering::SeqCst), 1);
    factory.shutdown().await;
    server.abort();
}

#[tokio::test]
async fn an_unknown_first_skill_registers_no_agent_runs_no_model_and_deduplicates() {
    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let factory = Arc::new(FixtureFactory::new(vec![]));
    let owner = Arc::new(OwnerServer::new(Uuid::new_v4(), factory.clone()));
    let (client, server) = serve(temporary.path(), owner).await;
    let command = envelope(
        client.owner_epoch(),
        AgentCommand::Create {
            launch: LaunchSpec::embedded(temporary.path().into()),
            initial: Some(UserInput::Skill {
                name: "fixture-no-such-skill".into(),
                arguments: String::new(),
            }),
            origin: None,
        },
    );
    let failed = client.execute(command.clone()).await.unwrap();
    assert!(matches!(failed.state, ReceiptState::Failed));
    assert_eq!(failed.target, None);
    let message = failed.error.as_ref().unwrap().message.clone();
    assert!(message.contains("fixture-no-such-skill"), "{message}");
    assert!(!message.contains("was created"), "{message}");
    let duplicate = client.execute(command).await.unwrap();
    assert!(matches!(duplicate.state, ReceiptState::Failed));
    assert_eq!(duplicate.target, None);
    assert_eq!(duplicate.error.unwrap().message, message);
    assert!(agents(&client).await.is_empty());
    assert!(factory.model.requests.lock().unwrap().is_empty());
    // The owner starts one worker before that worker checks the Skill; the duplicate is answered
    // from the receipt and starts nothing.
    assert_eq!(factory.started.load(Ordering::SeqCst), 1);
    factory.shutdown().await;
    server.abort();
}

#[tokio::test]
async fn default_history_pages_return_multiple_records_and_transcript_messages() {
    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let factory = Arc::new(FixtureFactory::new(vec![
        support::text("One"),
        support::text("Two"),
        support::text("Three"),
    ]));
    let owner = Arc::new(OwnerServer::new(Uuid::new_v4(), factory.clone()));
    let (client, server) = serve(temporary.path(), owner).await;
    let mut ids = Vec::new();
    for prompt in ["First", "Second", "Third"] {
        let mut launch = LaunchSpec::embedded(temporary.path().into());
        launch.config_root = Some(temporary.path().join("state"));
        let receipt = client
            .execute(envelope(
                client.owner_epoch(),
                AgentCommand::Create {
                    launch,
                    initial: Some(UserInput::Text {
                        text: prompt.into(),
                        contexts: vec![],
                        paths: vec![],
                    }),
                    origin: None,
                },
            ))
            .await
            .unwrap();
        ids.push(execution(receipt));
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let observed = client.observe(ObservationRequest::Agent { execution: *ids.last().unwrap(), detail: AgentDetailQuery::Snapshot, after: None }).await.unwrap();
                if matches!(observed.initial, ObservationValue::Agent(snapshot) if snapshot.conversation.loops.iter().any(|goal| matches!(goal.state, tickloop_agent::runtime::LoopState::Completed))) { break; }
                tokio::task::yield_now().await;
            }
        }).await.unwrap();
    }
    client
        .execute(envelope(
            client.owner_epoch(),
            AgentCommand::FlushTranscripts,
        ))
        .await
        .unwrap()
        .into_result()
        .unwrap();
    let observed = client
        .observe(ObservationRequest::SavedRecords {
            store_root: Some(temporary.path().join("state")),
            workdir: temporary.path().into(),
            page: Page::default(),
        })
        .await
        .unwrap();
    let ObservationValue::Records(records) = observed.initial else {
        panic!("expected records");
    };
    assert_eq!(records.len(), 3);
    let record = TranscriptRef {
        store_root: temporary.path().join("state"),
        chat_id: records[0].id,
    };
    let observed = client
        .observe(ObservationRequest::Transcript {
            record: record.clone(),
            page: Page::default(),
        })
        .await
        .unwrap();
    let ObservationValue::Transcript(history) = observed.initial else {
        panic!("expected history");
    };
    assert!(history.messages.len() > 1);
    assert_eq!(history.messages.len(), history.total_messages);
    let observed = client
        .observe(ObservationRequest::Transcript {
            record,
            page: Page {
                offset: 1,
                limit: 1,
            },
        })
        .await
        .unwrap();
    let ObservationValue::Transcript(next) = observed.initial else {
        panic!("expected next page");
    };
    assert_eq!(next.messages.len(), 1);
    assert_eq!(next.messages[0].id, history.messages[1].id);
    assert_eq!(client.protocol().major, PROTOCOL_MAJOR);
    assert!(
        client
            .protocol()
            .capabilities
            .contains(&AccessCapability::EditHistory)
    );
    assert!(
        client
            .protocol()
            .capabilities
            .contains(&AccessCapability::DisplayRegistration)
    );
    let limits = client
        .protocol()
        .limits
        .as_ref()
        .expect("negotiated finite limits");
    assert_eq!(limits.frame_bytes, MAX_FRAME_BYTES);
    assert_eq!(limits.view_lease_seconds, VIEW_LEASE_SECONDS);
    assert!(limits.feed_bytes < limits.frame_bytes);
    factory.shutdown().await;
    server.abort();
}

#[tokio::test]
async fn the_owner_discovers_and_routes_more_than_one_page_in_one_worker() {
    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let factory = Arc::new(FixtureFactory::new(vec![]));
    let owner = Arc::new(OwnerServer::new(Uuid::new_v4(), factory.clone()));
    let (client, server) = serve(temporary.path(), owner).await;
    let root = execution(
        client
            .execute(create(client.owner_epoch(), temporary.path()))
            .await
            .unwrap(),
    );
    let worker = factory.accesses.lock().await[0].clone();
    for _ in 0..MAX_PAGE_LIMIT {
        worker
            .execute(create(client.owner_epoch(), temporary.path()))
            .await
            .unwrap()
            .into_result()
            .unwrap();
    }
    let observed = client
        .observe(ObservationRequest::Overview {
            filter: AgentFilter::default(),
            page: Page {
                offset: 0,
                limit: MAX_PAGE_LIMIT,
            },
            after: None,
        })
        .await
        .unwrap();
    let ObservationValue::Overview(first) = observed.initial else {
        panic!("expected first page");
    };
    assert_eq!(first.len(), MAX_PAGE_LIMIT);
    let observed = client
        .observe(ObservationRequest::Overview {
            filter: AgentFilter::default(),
            page: Page {
                offset: MAX_PAGE_LIMIT,
                limit: MAX_PAGE_LIMIT,
            },
            after: None,
        })
        .await
        .unwrap();
    let ObservationValue::Overview(tail) = observed.initial else {
        panic!("expected second page");
    };
    assert_eq!(tail.len(), 1);
    assert!(!first.iter().any(|row| row.execution == tail[0].execution));
    assert!(first.iter().chain(&tail).any(|row| row.execution == root));
    let observed = client
        .observe(ObservationRequest::Agent {
            execution: tail[0].execution,
            detail: AgentDetailQuery::Snapshot,
            after: None,
        })
        .await
        .unwrap();
    let ObservationValue::Agent(snapshot) = observed.initial else {
        panic!("expected routed tail Agent");
    };
    assert_eq!(snapshot.execution, tail[0].execution);
    assert_eq!(snapshot.conversation.id, tail[0].chat_id);
    assert_eq!(factory.started.load(Ordering::SeqCst), 1);
    assert!(factory.model.requests.lock().unwrap().is_empty());
    factory.shutdown().await;
    server.abort();
}

#[tokio::test]
async fn a_client_rejected_oversized_create_does_not_mark_acceptance_unknown() {
    use tickloop_agent::agent_console::AgentConsole;
    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let factory = Arc::new(FixtureFactory::new(vec![]));
    let owner = Arc::new(OwnerServer::new(Uuid::new_v4(), factory.clone()));
    let (client, server) = serve(temporary.path(), owner).await;
    let access = Arc::new(client.clone());
    let mut console =
        AgentConsole::new(access, client.owner_epoch(), temporary.path().into(), None);
    let error = console
        .dispatch(
            LaunchSpec::embedded(temporary.path().into()),
            UserInput::Text {
                text: "x".repeat(MAX_FRAME_BYTES),
                contexts: vec![],
                paths: vec![],
            },
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, AccessErrorCode::PayloadTooLarge);
    assert!(console.projection().unconfirmed_dispatch.is_none());
    assert_eq!(factory.started.load(Ordering::SeqCst), 0);
    assert!(agents(&client).await.is_empty());
    let accepted = console
        .dispatch(
            LaunchSpec::embedded(temporary.path().into()),
            UserInput::Shell {
                command: "printf explicitly-corrected".into(),
                background: false,
            },
            None,
        )
        .await
        .unwrap()
        .into_result()
        .unwrap();
    assert!(matches!(accepted, CommandResult::Created { .. }));
    assert_eq!(factory.started.load(Ordering::SeqCst), 1);
    assert_eq!(agents(&client).await.len(), 1);
    factory.shutdown().await;
    server.abort();
}
