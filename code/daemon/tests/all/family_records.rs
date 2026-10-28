use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use serde_json::json;
use tickloop_code_access::{
    AccessError, AgentAccess, AgentCommand, AgentFilter, AgentSummary, CommandEnvelope,
    CommandResult, ExecutionRef, LaunchSpec, ObservationRequest, ObservationValue, Page,
    TranscriptRef,
};
use tickloop_code_daemon::{DaemonServer, EmbeddedAgentAccess, WorkerFactory};
use tickloop_code_settings::local_agent::LocalAgent;
use tickloop_harness::protocol::{ChatId, Content, EventKind};
use tickloop_harness::runtime::Agent;
use tickloop_harness::test_support::routed::{RoutedModel, has_tool_result, state};
use tickloop_harness::test_support::{ScriptedModel, call, config, next_event, text};
use uuid::Uuid;

/// Starts every Worker on one configuration root, counting them.
struct SavedRootWorkers {
    root: PathBuf,
    started: AtomicUsize,
}

#[async_trait]
impl WorkerFactory for SavedRootWorkers {
    async fn start(
        &self,
        _: &LaunchSpec,
        epoch: Uuid,
    ) -> Result<Arc<dyn AgentAccess>, AccessError> {
        self.started.fetch_add(1, Ordering::SeqCst);
        let runtime = Agent::open_embedded(
            self.root.clone(),
            config(),
            Arc::new(ScriptedModel::new(Vec::<Vec<Content>>::new())),
        )
        .unwrap();
        Ok(Arc::new(EmbeddedAgentAccess::for_worker(runtime, epoch)))
    }

    async fn exited(&self, _: &Arc<dyn AgentAccess>) -> bool {
        false
    }
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
    let mut events = agent.subscribe();
    let receipt = agent
        .submit(chat, "Delegate the inspection".into())
        .await
        .unwrap();
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
    let child = agent.subagents(chat).await.unwrap()[0].child_chat_id;
    agent.shutdown().await;
    (chat, child)
}

async fn load(daemon: &DaemonServer, root: &Path, folder: &Path, chat: ChatId) -> ExecutionRef {
    let mut launch = LaunchSpec::embedded(folder.into());
    launch.config_root = Some(root.into());
    let result = daemon
        .execute(CommandEnvelope {
            daemon_epoch: daemon.daemon_epoch(),
            command_id: Uuid::new_v4(),
            command: AgentCommand::LoadRecord {
                record: TranscriptRef {
                    store_root: root.into(),
                    chat_id: chat.uuid(),
                },
                as_new: false,
                launch: Some(launch),
            },
        })
        .await
        .unwrap()
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

#[tokio::test]
async fn loading_a_subagent_record_joins_its_family_already_running_in_a_worker() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let folder = directory.path();
    let (root_chat, child_chat) = saved_family(&root, folder).await;
    let factory = Arc::new(SavedRootWorkers {
        root: root.clone(),
        started: AtomicUsize::new(0),
    });
    let daemon = DaemonServer::new(Uuid::new_v4(), factory.clone());

    let root_execution = load(&daemon, &root, folder, root_chat).await;
    let child_execution = load(&daemon, &root, folder, child_chat).await;

    assert_eq!(factory.started.load(Ordering::SeqCst), 1);
    let rows = overview(&daemon).await;
    let roots = rows
        .iter()
        .filter(|row| row.parent.is_none() && row.subagent.is_none())
        .map(|row| (row.chat_id, row.execution))
        .collect::<Vec<_>>();
    assert_eq!(roots, [(root_chat.uuid(), root_execution)]);
    let child = rows
        .iter()
        .find(|row| row.execution == child_execution)
        .unwrap();
    assert_eq!(child.chat_id, child_chat.uuid());
    assert_eq!(child.parent, Some(root_execution));
    assert_eq!(child.subagent.as_ref().unwrap().handle, "inspector");
}
