//! @tickloop requests through the reference host: a holding delivers each with
//! `TurnOp::Deliver` and acknowledges it once synced and published. A holder that dies before
//! acknowledging, whether before the delivery committed or after it ran, leaves the redelivery
//! to take it into the conversation exactly once, and a delivery handed out again after its
//! acknowledgement was lost runs nothing.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tickloop_agent_artifacts::ClaimedFeedback;
use tickloop_agent_protocol::{AgentHostOptions, Content, EventSourceBinding, PromptPolicy};
use tickloop_agent_runtime::transcript::{LoadRange, TranscriptStore};
use tickloop_agent_runtime::{
    Agent, Delivery, ExternalDelivery, HarnessOptions, HarnessSeams, ParentSelection, ToolPolicy,
    TurnOp, TurnOutcome, TurnRequest, UserMessage, WaitHosting,
};
use tickloop_agent_test_support::event_stream::MemoryEventStream;
use tickloop_agent_test_support::harness::ChosenModels;
use tickloop_agent_test_support::routed::RoutedModel;
use tickloop_agent_test_support::stateless_host::{
    CrashPoint, DurableChats, Halt, HostConfig, ManualClock, ReferenceHost,
};
use tickloop_agent_test_support::{config, text};
use tickloop_agent_transcript::{InputOp, Line};
use uuid::Uuid;

const LEASE_TTL: Duration = Duration::from_secs(30);
const HEADING: &str = "@tickloop feedback request-1 on artifact doc";

fn cloud_harness(store: Arc<dyn TranscriptStore>, model: Arc<RoutedModel>) -> Agent {
    Agent::with_seams(
        HarnessSeams {
            transcripts: Some(store),
            environment: None,
            models: ChosenModels::new(model),
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

struct Cloud {
    clock: Arc<ManualClock>,
    chats: Arc<DurableChats>,
    stream: Arc<MemoryEventStream>,
    model: Arc<RoutedModel>,
    chat: Uuid,
}

impl Cloud {
    fn new() -> Self {
        let clock = ManualClock::new();
        Self {
            chats: DurableChats::new(clock.clone()),
            clock,
            stream: Arc::new(MemoryEventStream::new()),
            model: Arc::new(RoutedModel::new(|_| text("Handled."))),
            chat: Uuid::new_v4(),
        }
    }

    fn host(&self, holder: &str) -> ReferenceHost {
        let model = self.model.clone();
        ReferenceHost::new(
            holder,
            self.chats.clone(),
            self.stream.clone(),
            self.clock.clone(),
            HostConfig::default(),
            move |store| cloud_harness(store, model.clone()),
        )
    }

    fn lines(&self) -> Vec<Line> {
        self.chats.history(self.chat).lines
    }

    /// How often the request's prompt is in the conversation.
    fn prompts(&self) -> usize {
        self.lines()
            .iter()
            .filter_map(|line| match line {
                Line::User(entry) => Some(&entry.message.content),
                _ => None,
            })
            .flatten()
            .filter(
                |content| matches!(content, Content::Text { text } if text.starts_with(HEADING)),
            )
            .count()
    }
}

fn prompt(text: &str) -> TurnRequest {
    TurnRequest {
        input_id: Uuid::new_v4(),
        op: TurnOp::Continue {
            after: ParentSelection::CurrentLeaf,
            target_loop: None,
            message: UserMessage {
                text: text.into(),
                ..Default::default()
            },
        },
        delivery: Delivery::Queue,
        host_context: None,
    }
}

/// The delivery of the request the service handed the Chat's Loop, as the host claimed it.
fn delivery() -> ExternalDelivery {
    ExternalDelivery::from_claim(
        "https://tickloop.ai/artifact",
        "owner",
        "doc",
        &ClaimedFeedback {
            delivery_id: Uuid::new_v4().to_string(),
            feedback_id: "request-1".into(),
            claim_id: "claim".into(),
            fencing_token: "fence-secret".into(),
            lease_expires_at: "2026-10-20T00:00:00Z".into(),
            input: json!({"messages": [{"text": "Tighten the title"}]}),
        },
    )
    .unwrap()
}

/// The first append that records the delivery in the ledger, in whatever state it reached.
fn records_the_delivery(lines: &[Line]) -> bool {
    lines
        .iter()
        .any(|line| matches!(line, Line::Input(entry) if entry.op == InputOp::Feedback))
}

#[tokio::test]
async fn a_delivery_whose_holder_died_before_acknowledging_enters_the_conversation_once() {
    for crash in [
        CrashPoint::BeforeCommit(records_the_delivery),
        CrashPoint::BeforeAck,
    ] {
        let cloud = Cloud::new();
        let ready = prompt("Watch the document.");
        let report = cloud
            .host("first")
            .deliver(cloud.chat, ready.clone())
            .await
            .unwrap();
        assert!(report.acknowledged, "{report:?}");
        let target = cloud
            .lines()
            .iter()
            .find_map(|line| match line {
                Line::Input(entry) if entry.input_id == ready.input_id => Some(entry.loop_id),
                _ => None,
            })
            .expect("the prompt is a ledger input");

        let chat = cloud.chats.harness_chat(cloud.chat).unwrap();
        let transcripts = cloud.chats.transcripts();
        let stored = transcripts
            .load(chat, LoadRange::Full)
            .await
            .unwrap()
            .unwrap();
        let mut lines = cloud.lines();
        let record = lines
            .iter_mut()
            .rev()
            .find_map(|line| match line {
                Line::System(entry) => entry
                    .loop_record
                    .as_mut()
                    .filter(|record| record.loop_id == target),
                _ => None,
            })
            .unwrap();
        record.event_sources = vec![
            serde_json::to_value(EventSourceBinding {
                origin: "https://tickloop.ai/artifact".into(),
                account_id: "owner".into(),
                artifact_id: "doc".into(),
                binding_id: Uuid::new_v4(),
                client_instance_id: Uuid::new_v4(),
                epoch: 1,
                resume_required: false,
                last_request_at_ms: None,
            })
            .unwrap(),
        ];
        transcripts.seed(chat, lines, stored.position);

        let request = TurnRequest::deliver(target, delivery());
        let holding = cloud.host("crashing").acquire(cloud.chat).unwrap();
        holding.crash_at(crash);
        let crashed = holding.run(request.clone()).await;
        assert_eq!(crashed.halt, Some(Halt::Crashed), "{crashed:?}");
        assert_eq!(
            cloud.chats.acknowledgements(cloud.chat, request.input_id),
            0
        );

        cloud.clock.advance(LEASE_TTL + Duration::from_secs(1));
        let redelivered = cloud
            .host("surviving")
            .deliver(cloud.chat, request.clone())
            .await
            .unwrap();
        assert!(redelivered.acknowledged, "{redelivered:?}");
        assert!(
            matches!(redelivered.outcome, Some(TurnOutcome::Completed { .. })),
            "{redelivered:?}"
        );
        assert_eq!(cloud.prompts(), 1);

        // The service hands it out again: the acknowledgement was lost on its way.
        let requests = cloud.model.requests.lock().unwrap().len();
        let again = cloud
            .host("again")
            .deliver(cloud.chat, request.clone())
            .await
            .unwrap();
        assert!(again.acknowledged, "{again:?}");
        assert!(matches!(again.outcome, Some(TurnOutcome::Completed { .. })));
        assert_eq!(cloud.model.requests.lock().unwrap().len(), requests);
        assert_eq!(cloud.prompts(), 1);
    }
}
