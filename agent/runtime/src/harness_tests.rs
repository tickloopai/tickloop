use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use tickloop_agent_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_agent_protocol::{AgentHostOptions, ChatId, ModelIdentity};
use tickloop_agent_test_support::{ScriptedModel, config, harness::ChosenModels, text};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::*;
use crate::{environment::LocalEnvironment, transcript::SettledOutcome};

const LEAK_CYCLES: usize = 200;

fn harness(
    transcripts: Option<Arc<dyn TranscriptStore>>,
    environment: Option<Arc<dyn ExecutionEnvironment>>,
    model: Arc<dyn ModelClient>,
) -> Agent {
    Agent::with_seams(
        HarnessSeams {
            transcripts,
            environment,
            models: ChosenModels::new(model),
        },
        HarnessOptions {
            model: config(),
            settings: None,
            host: AgentHostOptions::embedded_defaults(),
            tools: ToolPolicy::local(),
            host_rules: Vec::new(),
        },
    )
    .unwrap()
}

async fn settle_one_turn(agent: &Agent, chat: ChatId) {
    let input = Uuid::new_v4();
    agent
        .submit_with_input_id(chat, input, None, "prompt".into(), vec![], vec![], None)
        .await
        .unwrap();
    assert!(matches!(
        agent.settled_input(chat, input).await.unwrap(),
        SettledOutcome::Completed { .. }
    ));
}

async fn wait_for_alive_tasks(expected: usize, cycle: usize) {
    let metrics = tokio::runtime::Handle::current().metrics();
    tokio::time::timeout(Duration::from_secs(5), async {
        while metrics.num_alive_tasks() > expected {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "cycle {cycle}: {} tasks alive, {expected} before",
            metrics.num_alive_tasks()
        )
    });
}

/// Each cycle is one leased cloud turn: a Chat with a Folder (command watcher, JSONL flush loop)
/// alternates with a Chat without one. After `shutdown` nothing of the instance stays alive.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_turns_leave_no_instance_or_task_behind() {
    let folder = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let baseline = tokio::runtime::Handle::current()
        .metrics()
        .num_alive_tasks();
    for cycle in 0..LEAK_CYCLES {
        let has_folder = cycle % 2 == 0;
        let store: Arc<dyn TranscriptStore> = Arc::new(
            crate::transcript::JsonlTranscriptStore::open(state.path().join("transcripts"))
                .unwrap(),
        );
        let environment: Option<Arc<dyn ExecutionEnvironment>> =
            has_folder.then(|| Arc::new(LocalEnvironment::bare()) as _);
        let agent = harness(
            Some(store),
            environment,
            Arc::new(ScriptedModel::new(vec![text("answer")])),
        );
        let chat = if has_folder {
            agent.create_chat(folder.path().into()).await.unwrap()
        } else {
            agent.create_chat_without_folder().await.unwrap()
        };
        settle_one_turn(&agent, chat).await;
        assert_eq!(agent.quiesce(chat).await, Quiescence::Idle);
        agent.shutdown().await;
        let instance = Arc::downgrade(&agent.inner);
        drop(agent);
        assert!(instance.upgrade().is_none(), "cycle {cycle}");
        wait_for_alive_tasks(baseline, cycle).await;
    }
}

/// Answers every turn and never finishes a title until cancelled.
struct EndlessTitles {
    title_cancelled: AtomicBool,
}

#[async_trait]
impl ModelClient for EndlessTitles {
    fn identity(&self) -> ModelIdentity {
        ScriptedModel::new(Vec::new()).identity()
    }

    async fn complete(
        &self,
        _: ModelRequest,
        _: UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        Ok(ModelOutput {
            private_source: None,
            content: text("answer"),
            usage: Default::default(),
            stop_reason: "end_turn".into(),
        })
    }

    async fn generate_title(
        &self,
        _: ModelRequest,
        _: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        cancel.cancelled().await;
        self.title_cancelled.store(true, Ordering::SeqCst);
        Err(ModelError::Cancelled)
    }
}

#[tokio::test(start_paused = true)]
async fn quiesce_cancels_a_title_request_still_running_after_ten_seconds() {
    let folder = tempfile::tempdir().unwrap();
    let model = Arc::new(EndlessTitles {
        title_cancelled: AtomicBool::new(false),
    });
    let agent = harness(
        None,
        Some(Arc::new(LocalEnvironment::bare())),
        model.clone(),
    );
    let chat = agent.create_chat(folder.path().into()).await.unwrap();
    settle_one_turn(&agent, chat).await;
    assert!(agent.inner.title_requests.lock().await.contains_key(&chat));

    let started = tokio::time::Instant::now();
    assert_eq!(agent.quiesce(chat).await, Quiescence::Idle);
    let waited = started.elapsed();
    assert!(
        (TITLE_SETTLE_LIMIT..TITLE_SETTLE_LIMIT + Duration::from_secs(1)).contains(&waited),
        "{waited:?}"
    );
    assert!(model.title_cancelled.load(Ordering::SeqCst));
    assert!(agent.inner.title_requests.lock().await.is_empty());
    assert!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .generated_title
            .is_none()
    );
    agent.shutdown().await;
}
