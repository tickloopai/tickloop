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
use crate::{
    Delivery, ParentSelection, TurnOp, TurnOutcome, TurnRequest, UserMessage,
    environment::LocalEnvironment,
};
use tickloop_agent_transcript::{InterruptedBy, ResponseEnding, ResponseState};

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

fn prompt_request(input: Uuid, after: ParentSelection, delivery: Delivery) -> TurnRequest {
    TurnRequest {
        input_id: input,
        op: TurnOp::Continue {
            after,
            target_loop: None,
            message: UserMessage {
                text: "prompt".into(),
                ..Default::default()
            },
        },
        delivery,
        host_context: None,
    }
}

async fn settle_one_turn(agent: &Agent, chat: ChatId) {
    let turn = agent
        .advance(
            chat,
            prompt_request(
                Uuid::new_v4(),
                ParentSelection::CurrentLeaf,
                Delivery::Queue,
            ),
        )
        .await
        .unwrap();
    assert!(matches!(
        turn.outcome().await.unwrap(),
        TurnOutcome::Completed { .. }
    ));
}

/// One leased cloud turn's calls: the first prompt starts a root, a repeated advance only
/// attaches, and the next prompt follows the first answer explicitly.
async fn settle_leased_turns(agent: &Agent, chat: ChatId) {
    let first = prompt_request(Uuid::new_v4(), ParentSelection::Root, Delivery::SendNow);
    let turn = agent.advance(chat, first.clone()).await.unwrap();
    assert!(matches!(
        turn.outcome().await.unwrap(),
        TurnOutcome::Completed { .. }
    ));
    let repeated = agent.advance(chat, first).await.unwrap();
    assert_eq!(repeated.receipt().run_id, turn.receipt().run_id);
    let answer = agent
        .input_state(chat, turn.input_id())
        .await
        .unwrap()
        .unwrap()
        .responses[0]
        .response_id;
    let next = agent
        .advance(
            chat,
            prompt_request(
                Uuid::new_v4(),
                ParentSelection::Message(answer),
                Delivery::SendNow,
            ),
        )
        .await
        .unwrap();
    assert!(matches!(
        next.outcome().await.unwrap(),
        TurnOutcome::Completed { .. }
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

/// Each cycle is one leased cloud turn through `advance`: a Chat with a Folder (command watcher,
/// JSONL flush loop) alternates with a Chat without one. After `shutdown` and `sync` nothing of
/// the instance stays alive.
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
            Arc::new(ScriptedModel::new(vec![text("answer"), text("answer")])),
        );
        let chat = if has_folder {
            agent.create_chat(folder.path().into()).await.unwrap()
        } else {
            agent.create_chat_without_folder().await.unwrap()
        };
        settle_leased_turns(&agent, chat).await;
        assert_eq!(agent.quiesce(chat).await, Quiescence::Idle);
        agent.shutdown().await;
        agent.sync().await.unwrap();
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

/// A submission admitted before shutdown that has selected its Run but not yet registered its
/// task: shutdown waits for it, and for the Run it then starts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_waits_for_a_submission_admitted_before_it_registered_its_run() {
    let folder = tempfile::tempdir().unwrap();
    let agent = harness(
        None,
        Some(Arc::new(LocalEnvironment::bare())),
        Arc::new(ScriptedModel::new(vec![text("answer")])),
    );
    let chat = agent.create_chat(folder.path().into()).await.unwrap();
    let input = Uuid::new_v4();
    let pause = crate::turn::admission_pause::pause_next(chat);
    let submitting = tokio::spawn({
        let agent = agent.clone();
        async move {
            agent
                .advance(
                    chat,
                    prompt_request(input, ParentSelection::CurrentLeaf, Delivery::Queue),
                )
                .await
        }
    });
    pause.entered.await.unwrap();
    let shutting_down = tokio::spawn({
        let agent = agent.clone();
        async move { agent.shutdown().await }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !shutting_down.is_finished(),
        "shutdown waits for the admitted submission"
    );
    pause.release.send(()).unwrap();
    submitting.await.unwrap().unwrap();
    shutting_down.await.unwrap();

    let state = agent.input_state(chat, input).await.unwrap().unwrap();
    assert!(!state.is_live, "the Run ended before shutdown returned");
    assert_eq!(state.responses.len(), 1);
    assert!(matches!(
        state.responses[0].state,
        ResponseState::Ended {
            ending: ResponseEnding::Interrupted {
                by: InterruptedBy::Host
            }
        } | ResponseState::Ended {
            ending: ResponseEnding::Completed
        }
    ));
}
