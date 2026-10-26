//! A Loop that takes thousands of Trigger deliveries leaves a Transcript whose load stays bounded:
//! the chain is read from the latest compact boundary and the retained Lines from the latest
//! snapshot.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tickloop_agent_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_agent_protocol::{AgentHostOptions, EventKind, PromptPolicy, Usage};
use tickloop_agent_runtime::transcript::{LoadRange, StorePosition, TranscriptStore};
use tickloop_agent_runtime::{
    Agent, Delivery, HarnessOptions, HarnessSeams, ParentSelection, ToolPolicy, TurnOp,
    TurnRequest, UserMessage, WaitHosting,
};
use tickloop_agent_test_support::harness::ChosenModels;
use tickloop_agent_test_support::transcripts::InMemoryTranscriptStore;
use tickloop_agent_test_support::{config, next_event, text};
use tickloop_agent_transcript::Line;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// The conversation is compacted after this many fires, as a long-running Chat's would be.
const FIRES_PER_COMPACTION: usize = 100;
/// What a load may read of the retained Lines, whatever the number of fires. Each retained
/// request's ledger record names the delivery it came from, about 170 bytes of it.
const RETAINED_BYTES_LIMIT: usize = 640 * 1024;
const LOADED_LINES_LIMIT: usize = 2_000;
const LOAD_TIME_LIMIT: Duration = Duration::from_secs(2);

/// Answers every request, compaction summaries included, at once.
struct Answers;

#[async_trait]
impl ModelClient for Answers {
    async fn generate_title(
        &self,
        request: ModelRequest,
        _: UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        Ok(tickloop_agent_test_support::title_output(request))
    }

    fn identity(&self) -> tickloop_agent_protocol::ModelIdentity {
        tickloop_agent_protocol::ModelIdentity {
            provider: tickloop_agent_protocol::ModelProvider::Api,
            profile: "fixture".into(),
            protocol: tickloop_agent_protocol::Protocol::Messages,
            requested_model: "fixture".into(),
        }
    }

    async fn complete(
        &self,
        _: ModelRequest,
        _: UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        Ok(ModelOutput {
            private_source: None,
            content: text("checked"),
            usage: Usage {
                input_tokens: Some(100),
                output_tokens: Some(10),
                cache_read_input_tokens: Some(0),
                cache_write_input_tokens: Some(0),
            },
            stop_reason: "end_turn".into(),
        })
    }
}

fn harness(store: Arc<dyn TranscriptStore>) -> Agent {
    Agent::with_seams(
        HarnessSeams {
            search: None,
            transcripts: Some(store),
            environment: None,
            models: ChosenModels::new(Arc::new(Answers)),
        },
        HarnessOptions {
            host: AgentHostOptions {
                prompt_policy: PromptPolicy::DenyOnPrompt,
                ..AgentHostOptions::product_defaults()
            },
            tools: ToolPolicy::web_fetch_only(),
            waits: WaitHosting::Durable,
            ..HarnessOptions::local(config())
        },
    )
    .unwrap()
}

async fn until_completed(
    events: &mut tokio::sync::broadcast::Receiver<tickloop_agent_protocol::Event>,
    run: Uuid,
) {
    loop {
        let event = next_event(events).await;
        match event.kind {
            EventKind::Completed { .. } if event.run_id == run => return,
            EventKind::Failed(error) if event.run_id == run => panic!("{error}"),
            _ => {}
        }
    }
}

/// A Loop taking `fires` @tickloop requests a durable host delivers over holdings of
/// `fires_per_holding` each, every one on a fresh instance as a host gives each holding, and the
/// conversation compacted as a long-running Chat's is. Every load reads a bounded part of the
/// Transcript.
async fn fire_over_holdings(fires: usize, fires_per_holding: usize) {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let agent = harness(store.clone());
    let chat = agent.create_chat_without_folder().await.unwrap();
    let mut events = agent.subscribe();
    let turn = agent.advance(chat, check_feed()).await.unwrap();
    until_completed(&mut events, (turn.receipt().run_id).uuid()).await;
    agent.sync().await.unwrap();
    agent.shutdown().await;
    store.seed(
        chat,
        super::durable_deliveries::linked(store.lines(chat), turn.receipt().loop_id),
        StorePosition(1),
    );
    for holding in 0..fires / fires_per_holding {
        assert_bounded_load(&store, chat).await;
        let started = Instant::now();
        let agent = harness(store.clone());
        agent.resume_chat(chat).await.unwrap();
        assert!(
            started.elapsed() < LOAD_TIME_LIMIT,
            "loading took {:?}",
            started.elapsed()
        );
        // A durable load keeps the Loop receiving across holdings.
        let mut events = agent.subscribe();
        for fire in holding * fires_per_holding..(holding + 1) * fires_per_holding {
            let delivery = super::durable_deliveries::delivery(&format!("feedback-{fire}"));
            let turn = agent
                .advance(chat, TurnRequest::deliver(delivery))
                .await
                .unwrap();
            until_completed(&mut events, (turn.receipt().run_id).uuid()).await;
            if fire % FIRES_PER_COMPACTION == FIRES_PER_COMPACTION - 1 {
                agent.compact(chat).await.unwrap();
            }
        }
        agent.sync().await.unwrap();
        agent.shutdown().await;
    }
    assert_bounded_load(&store, chat).await;
    assert!(
        store
            .lines(chat)
            .iter()
            .any(|line| matches!(line, Line::RetainedSnapshot(_)))
    );
}

fn check_feed() -> TurnRequest {
    TurnRequest {
        input_id: tickloop_ids::Id::from(Uuid::new_v4()),
        op: TurnOp::Continue {
            after: ParentSelection::CurrentLeaf,
            message: UserMessage {
                text: "Check the feed whenever it changes.".into(),
                ..Default::default()
            },
        },
        delivery: Delivery::Queue,
        host_context: None,
    }
}

/// What a load from the latest boundary reads stays within the limits, however many fires
/// the Transcript holds.
async fn assert_bounded_load(
    store: &InMemoryTranscriptStore,
    chat: tickloop_agent_protocol::ChatId,
) {
    let loaded = store
        .load(chat, LoadRange::FromLatestBoundary)
        .await
        .unwrap()
        .unwrap();
    let retained_bytes = loaded
        .retained
        .iter()
        .chain(loaded.lines.iter().filter(|line| line.is_retained()))
        .map(|line| serde_json::to_vec(line).unwrap().len())
        .sum::<usize>();
    assert!(
        retained_bytes < RETAINED_BYTES_LIMIT,
        "a load reads {retained_bytes} bytes of retained Lines"
    );
    let read = loaded.retained.len() + loaded.lines.len();
    assert!(read < LOADED_LINES_LIMIT, "a load reads {read} Lines");
}

#[tokio::test(start_paused = true)]
async fn a_thousand_fires_over_several_holdings_leave_bounded_loads() {
    fire_over_holdings(1_000, 250).await;
}

// Paused time lets the recorder's flush interval and the other timers pass at once.
#[tokio::test(start_paused = true)]
#[ignore = "production-scale run; cargo nextest run --release --run-ignored only"]
async fn ten_thousand_fires_leave_bounded_loads_at_production_limits() {
    fire_over_holdings(10_000, 500).await;
}
