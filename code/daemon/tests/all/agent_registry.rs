use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use serde_json::json;
use tickloop_code_access::{
    AccessError, AccessErrorCode, AgentAccess, AgentCommand, AgentFilter, AgentSummary,
    CommandEnvelope, CommandReceipt, CommandResult, EndReason, EndedAgent, EndedOutcome,
    ExecutionRef, LaunchSpec, ObservationRequest, ObservationValue, Page, TranscriptRef, UserInput,
};
use tickloop_code_daemon::agent_registry::REGISTRY_DIRECTORY;
use tickloop_code_daemon::{
    AgentRegistries, AgentRegistry, DaemonServer, EmbeddedAgentAccess, WorkerFactory,
};
use tickloop_code_settings::local_agent::LocalAgent;
use tickloop_harness::protocol::{ChatId, EventKind, LoopState};
use tickloop_harness::runtime::Agent;
use tickloop_harness::test_support::routed::{RoutedModel, has_tool_result, state};
use tickloop_harness::test_support::{ScriptedModel, call, config, next_event, text};
use uuid::Uuid;

const REGISTRY_WAIT: Duration = Duration::from_secs(5);

/// Starts every Worker on one configuration root with one shared scripted model, counting the
/// Workers; a daemon that loaded or ran an Agent on its own would start one or ask the model.
struct CountingWorkers {
    root: PathBuf,
    started: AtomicUsize,
    model: Arc<ScriptedModel>,
}

impl CountingWorkers {
    fn new(root: &Path) -> Arc<Self> {
        Arc::new(Self {
            root: root.into(),
            started: AtomicUsize::new(0),
            model: Arc::new(ScriptedModel::new(vec![text("Continued the module")])),
        })
    }

    fn model_requests(&self) -> usize {
        self.model.requests.lock().unwrap().len()
    }
}

#[async_trait]
impl WorkerFactory for CountingWorkers {
    async fn start(
        &self,
        _: &LaunchSpec,
        epoch: Uuid,
    ) -> Result<Arc<dyn AgentAccess>, AccessError> {
        self.started.fetch_add(1, Ordering::SeqCst);
        let runtime =
            Agent::open_embedded(self.root.clone(), config(), self.model.clone()).unwrap();
        Ok(Arc::new(EmbeddedAgentAccess::for_worker(runtime, epoch)))
    }

    async fn exited(&self, _: &Arc<dyn AgentAccess>) -> bool {
        false
    }
}

async fn wait_for_run(agent: &Agent, chat: ChatId, prompt: &str) {
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, prompt.into()).await.unwrap();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != receipt.run_id {
            continue;
        }
        match event.kind {
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
}

/// Saves a root Chat with one finished turn.
async fn saved_chat(root: &Path, folder: &Path) -> ChatId {
    let agent = Agent::open_embedded(
        root.into(),
        config(),
        Arc::new(ScriptedModel::new(vec![text("Summarized the module")])),
    )
    .unwrap();
    let chat = agent.create_chat(folder.into()).await.unwrap();
    wait_for_run(&agent, chat, "Summarize the module").await;
    agent.shutdown().await;
    chat
}

/// Saves a root Chat that delegated one Subagent; returns the root and the Subagent.
async fn saved_family(root: &Path, folder: &Path) -> (ChatId, ChatId) {
    let model = Arc::new(RoutedModel::new(|request| {
        if !state(request)["delegation"].is_null() {
            return text("child report");
        }
        if !has_tool_result(request, "delegate") {
            return call(
                "delegate",
                "Subagent",
                json!({"task":"Inspect the module","name":"inspector"}),
            );
        }
        text("root done")
    }));
    let agent = Agent::open_embedded(root.into(), config(), model).unwrap();
    let chat = agent.create_chat(folder.into()).await.unwrap();
    wait_for_run(&agent, chat, "Delegate the inspection").await;
    let child = agent.subagents(chat).await.unwrap()[0].child_chat_id;
    agent.shutdown().await;
    (chat, child)
}

fn started_daemon(factory: Arc<CountingWorkers>) -> DaemonServer {
    DaemonServer::with_agent_registries(
        Uuid::new_v4(),
        factory,
        AgentRegistries::new(|root| AgentRegistry::new(root, None)),
    )
}

async fn execute(daemon: &DaemonServer, command: AgentCommand) -> CommandReceipt {
    daemon
        .execute(CommandEnvelope {
            daemon_epoch: daemon.daemon_epoch(),
            command_id: Uuid::new_v4(),
            command,
        })
        .await
        .unwrap()
}

async fn load(daemon: &DaemonServer, root: &Path, folder: &Path, chat: ChatId) -> ExecutionRef {
    let mut launch = LaunchSpec::embedded(folder.into());
    launch.config_root = Some(root.into());
    let result = execute(
        daemon,
        AgentCommand::LoadRecord {
            record: TranscriptRef {
                store_root: root.into(),
                chat_id: chat.uuid(),
            },
            as_new: false,
            launch: Some(launch),
        },
    )
    .await
    .into_result()
    .unwrap();
    let CommandResult::Loaded { execution, .. } = result else {
        panic!("expected a loaded record, got {result:?}");
    };
    execution
}

async fn overview(daemon: &DaemonServer) -> Vec<AgentSummary> {
    let observation = daemon
        .observe(ObservationRequest::Overview {
            filter: AgentFilter::default(),
            page: Page {
                offset: 0,
                limit: 256,
            },
            after: None,
        })
        .await
        .unwrap();
    let ObservationValue::Overview(rows) = observation.initial else {
        panic!("expected the overview");
    };
    rows
}

async fn ended(daemon: &DaemonServer, root: &Path) -> Vec<EndedAgent> {
    let observation = daemon
        .observe(ObservationRequest::EndedAgents {
            store_root: Some(root.into()),
        })
        .await
        .unwrap();
    let ObservationValue::EndedAgents(ended) = observation.initial else {
        panic!("expected the ended Agents");
    };
    ended
}

fn record_path(root: &Path, chat: ChatId) -> PathBuf {
    root.join(REGISTRY_DIRECTORY)
        .join(format!("{}.json", chat.uuid().hyphenated()))
}

fn registry_files(root: &Path) -> Vec<String> {
    let mut names = std::fs::read_dir(root.join(REGISTRY_DIRECTORY))
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// Waits until the daemon's directory monitor wrote the record of `chat` with `status`.
async fn wait_for_record(root: &Path, chat: ChatId, status: &str) -> serde_json::Value {
    let path = record_path(root, chat);
    let deadline = tokio::time::Instant::now() + REGISTRY_WAIT;
    loop {
        if let Ok(bytes) = std::fs::read(&path) {
            let record: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            if record["status"] == json!(status) {
                return record;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the registry never recorded {status} for {path:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn a_running_root_is_recorded_in_a_private_file_written_whole() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let folder = directory.path();
    let chat = saved_chat(&root, folder).await;
    let daemon = started_daemon(CountingWorkers::new(&root));

    let execution = load(&daemon, &root, folder, chat).await;
    let record = wait_for_record(&root, chat, "live").await;

    assert_eq!(record["version"], 1);
    assert_eq!(record["chatId"], json!(chat.uuid()));
    assert_eq!(record["agentId"], json!(execution.agent_id));
    assert_eq!(record["folder"], json!(folder.canonicalize().unwrap()));
    assert_eq!(record["title"], "Summarize the module");
    assert_eq!(record["lastState"], json!(LoopState::Idle));
    assert!(record["createdAt"].as_u64().unwrap() > 0);
    assert!(record["updatedAt"].as_u64().unwrap() > 0);
    assert!(record.get("endedAt").is_none());
    let registry = root.join(REGISTRY_DIRECTORY);
    assert_eq!(
        std::fs::metadata(&registry).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(record_path(&root, chat))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    // Atomic replacement leaves no temporary file beside the record.
    assert_eq!(
        registry_files(&root),
        [format!("{}.json", chat.uuid().hyphenated())]
    );
    assert!(ended(&daemon, &root).await.is_empty());
}

#[tokio::test]
async fn after_a_daemon_restart_the_root_is_listed_as_failed_and_nothing_runs() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let folder = directory.path();
    let chat = saved_chat(&root, folder).await;
    let first = started_daemon(CountingWorkers::new(&root));
    let execution = load(&first, &root, folder, chat).await;
    wait_for_record(&root, chat, "live").await;
    drop(first);

    let workers = CountingWorkers::new(&root);
    let second = started_daemon(workers.clone());

    assert!(overview(&second).await.is_empty());
    let rows = ended(&second, &root).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    let row = &rows[0];
    assert_eq!(row.record.chat_id, chat.uuid());
    assert_eq!(row.record.store_root, root);
    assert_eq!(row.agent_id, execution.agent_id);
    assert_eq!(row.title, "Summarize the module");
    assert_eq!(row.outcome, EndedOutcome::Failed);
    assert_eq!(row.reason, EndReason::DaemonStopped);
    assert!(row.resumable);
    assert!(row.ended_at >= row.updated_at);
    let record = wait_for_record(&root, chat, "failed").await;
    assert!(record["endedAt"].as_u64().is_some());
    assert_eq!(record["endReason"], "daemon_stopped");
    // Listing the ended Agent loads nothing and starts no Worker, so no model can run.
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(workers.started.load(Ordering::SeqCst), 0);
    assert_eq!(workers.model_requests(), 0);
    assert!(overview(&second).await.is_empty());
}

#[tokio::test]
async fn loading_an_ended_root_starts_no_run_until_the_user_replies() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let folder = directory.path();
    let chat = saved_chat(&root, folder).await;
    let first = started_daemon(CountingWorkers::new(&root));
    load(&first, &root, folder, chat).await;
    wait_for_record(&root, chat, "live").await;
    drop(first);
    let workers = CountingWorkers::new(&root);
    let second = started_daemon(workers.clone());
    assert_eq!(ended(&second, &root).await.len(), 1);

    let execution = load(&second, &root, folder, chat).await;

    assert!(ended(&second, &root).await.is_empty());
    let rows = overview(&second).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].chat_id, chat.uuid());
    assert_eq!(rows[0].state, LoopState::Idle);
    let loaded_run = rows[0].run_id;
    let record = wait_for_record(&root, chat, "live").await;
    assert!(record.get("endedAt").is_none());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(workers.model_requests(), 0, "loading alone must not run");

    let receipt = execute(
        &second,
        AgentCommand::Submit {
            execution,
            input: UserInput::Text {
                text: "Continue".into(),
                contexts: Vec::new(),
                paths: Vec::new(),
            },
            origin: None,
        },
    )
    .await;
    receipt.into_result().unwrap();
    let deadline = tokio::time::Instant::now() + REGISTRY_WAIT;
    loop {
        let row = overview(&second).await.remove(0);
        if row.state == LoopState::Idle && row.run_id != loaded_run {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "{row:?}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(workers.model_requests(), 1);
}

#[tokio::test]
async fn forgetting_an_ended_root_removes_its_row_but_keeps_its_history() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let folder = directory.path();
    let chat = saved_chat(&root, folder).await;
    let first = started_daemon(CountingWorkers::new(&root));
    load(&first, &root, folder, chat).await;
    wait_for_record(&root, chat, "live").await;

    let refused = execute(
        &first,
        AgentCommand::ForgetEndedAgent {
            record: TranscriptRef {
                store_root: root.clone(),
                chat_id: chat.uuid(),
            },
        },
    )
    .await
    .into_result()
    .unwrap_err();
    assert_eq!(refused.code, AccessErrorCode::Busy);
    drop(first);

    let second = started_daemon(CountingWorkers::new(&root));
    let receipt = execute(
        &second,
        AgentCommand::ForgetEndedAgent {
            record: TranscriptRef {
                store_root: root.clone(),
                chat_id: chat.uuid(),
            },
        },
    )
    .await;
    assert!(matches!(receipt.into_result(), Ok(CommandResult::Done)));

    assert!(ended(&second, &root).await.is_empty());
    assert!(!record_path(&root, chat).exists());
    assert!(
        tickloop_harness::runtime::transcript::records::saved_record_exists(
            root.clone(),
            chat.uuid()
        )
        .await
        .unwrap()
    );
    let third = started_daemon(CountingWorkers::new(&root));
    assert!(ended(&third, &root).await.is_empty());
}

#[tokio::test]
async fn a_subagent_has_no_row_of_its_own() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let folder = directory.path();
    let (root_chat, child_chat) = saved_family(&root, folder).await;
    let first = started_daemon(CountingWorkers::new(&root));
    load(&first, &root, folder, root_chat).await;
    load(&first, &root, folder, child_chat).await;
    assert_eq!(overview(&first).await.len(), 2);
    wait_for_record(&root, root_chat, "live").await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        registry_files(&root),
        [format!("{}.json", root_chat.uuid().hyphenated())]
    );
    drop(first);

    let second = started_daemon(CountingWorkers::new(&root));

    let rows = ended(&second, &root).await;
    assert_eq!(
        rows.iter()
            .map(|row| row.record.chat_id)
            .collect::<Vec<_>>(),
        [root_chat.uuid()]
    );
}

#[tokio::test]
async fn an_ended_root_whose_history_was_deleted_is_not_resumable() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let folder = directory.path();
    let chat = saved_chat(&root, folder).await;
    let first = started_daemon(CountingWorkers::new(&root));
    load(&first, &root, folder, chat).await;
    wait_for_record(&root, chat, "live").await;
    drop(first);
    std::fs::remove_dir_all(root.join("projects")).unwrap();

    let second = started_daemon(CountingWorkers::new(&root));

    let rows = ended(&second, &root).await;
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].resumable);
}

#[tokio::test]
async fn an_unreadable_record_is_skipped_and_left_in_place() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let registry = root.join(REGISTRY_DIRECTORY);
    std::fs::create_dir_all(&registry).unwrap();
    std::fs::set_permissions(&registry, std::fs::Permissions::from_mode(0o700)).unwrap();
    let damaged = registry.join(format!("{}.json", Uuid::new_v4().hyphenated()));
    std::fs::write(&damaged, "{\"version\":").unwrap();

    let daemon = started_daemon(CountingWorkers::new(&root));

    assert!(ended(&daemon, &root).await.is_empty());
    assert!(damaged.exists());
}
