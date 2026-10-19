//! Scheduled goals through the reference host: each holding acknowledges with the time the
//! family is next due, and the scheduler's wake for that time is a holding of its own on a
//! fresh Harness. Every scheduled time fires once, however often or late the scheduler wakes
//! the family, and a crash between registering a scheduled goal and its call's result neither
//! loses the goal nor registers it twice.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tickloop_agent_model::ModelRequest;
use tickloop_agent_protocol::{AgentHostOptions, Content, PromptPolicy};
use tickloop_agent_runtime::transcript::TranscriptStore;
use tickloop_agent_runtime::{
    Agent, Delivery, HarnessOptions, HarnessSeams, ParentSelection, ToolPolicy, TurnOp,
    TurnRequest, UserMessage, WaitHosting,
};
use tickloop_agent_test_support::event_stream::MemoryEventStream;
use tickloop_agent_test_support::harness::ChosenModels;
use tickloop_agent_test_support::routed::{RoutedModel, has_tool_result};
use tickloop_agent_test_support::stateless_host::{
    CrashPoint, DurableChats, Halt, HostConfig, ManualClock, ReferenceHost, TurnReport,
};
use tickloop_agent_test_support::{call, config, text};
use tickloop_agent_transcript::{InputOp, Line};
use uuid::Uuid;

const LEASE_TTL: Duration = Duration::from_secs(30);
const HOUR: Duration = Duration::from_secs(3_600);
/// The cloud's floor for schedules.
const CLOUD_MIN_SCHEDULE_INTERVAL: Duration = Duration::from_secs(60);
const ASKS_FOR_A_SCHEDULE: &str = "Every hour, summarize the news.";
const SCHEDULED_GOAL: &str = "Summarize the news.";

fn last_message(request: &ModelRequest) -> String {
    request
        .messages
        .last()
        .map(|message| format!("{:?}", message.content))
        .unwrap_or_default()
}

/// Registers an hourly goal when asked to, and answers each of its runs.
fn scheduling_model() -> Arc<RoutedModel> {
    Arc::new(RoutedModel::new(|request| {
        let last = last_message(request);
        if last.contains(ASKS_FOR_A_SCHEDULE) && !has_tool_result(request, "goal") {
            call(
                "goal",
                "StartGoal",
                json!({"goal": SCHEDULED_GOAL, "schedule": "every 1h"}),
            )
        } else if last.contains(SCHEDULED_GOAL) {
            text("Here is the summary.")
        } else {
            text("Registered.")
        }
    }))
}

fn cloud_harness(store: Arc<dyn TranscriptStore>, model: Arc<RoutedModel>) -> Agent {
    Agent::with_seams(
        HarnessSeams {
            transcripts: Some(store),
            environment: None,
            models: ChosenModels::new(model),
        },
        HarnessOptions {
            model: config(),
            settings: None,
            host: AgentHostOptions {
                prompt_policy: PromptPolicy::DenyOnPrompt,
                ..AgentHostOptions::product_defaults()
            },
            tools: ToolPolicy {
                goals: true,
                ..ToolPolicy::web_fetch_only()
            },
            host_rules: Vec::new(),
            min_schedule_interval: CLOUD_MIN_SCHEDULE_INTERVAL,
            waits: WaitHosting::Durable,
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
            model: scheduling_model(),
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

    /// The scheduler's wake of the family for `due_at`, on the host `holder`.
    async fn wake(&self, holder: &str, due_at: SystemTime) -> TurnReport {
        let report = self
            .host(holder)
            .acquire(self.chat)
            .unwrap()
            .wake(Uuid::new_v4(), Some(due_at))
            .await;
        assert!(report.acknowledged, "{report:?}");
        report
    }

    /// How many runs of the scheduled goal the model was asked for.
    fn runs(&self) -> usize {
        self.model
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| last_message(request).contains(SCHEDULED_GOAL))
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

fn wake_at(report: &TurnReport) -> SystemTime {
    report
        .waiting
        .as_ref()
        .and_then(|waits| waits.wake_at)
        .unwrap_or_else(|| panic!("the family is due again: {report:?}"))
}

/// The scheduled times the ledger holds a fire for, by input id.
fn fires(lines: &[Line]) -> HashSet<Uuid> {
    lines
        .iter()
        .filter_map(|line| match line {
            Line::Input(entry) if entry.op == InputOp::Trigger => Some(entry.input_id),
            _ => None,
        })
        .collect()
}

fn results_of(lines: &[Line], call_id: &str) -> Vec<Value> {
    lines
        .iter()
        .filter_map(|line| match line {
            Line::User(entry) => Some(&entry.message.content),
            _ => None,
        })
        .flatten()
        .filter_map(|content| match content {
            Content::ToolResult {
                tool_use_id,
                content,
                ..
            } if tool_use_id == call_id => serde_json::from_str(&content.text()).ok(),
            _ => None,
        })
        .collect()
}

/// The Loops the chain records with a schedule.
fn scheduled_loops(lines: &[Line]) -> HashSet<Uuid> {
    lines
        .iter()
        .filter_map(|line| match line {
            Line::System(entry) => entry.loop_record.as_ref(),
            _ => None,
        })
        .filter(|record| {
            record
                .trigger
                .as_ref()
                .is_some_and(|trigger| !trigger["schedule"].is_null())
        })
        .map(|record| record.loop_id)
        .collect()
}

fn puts_the_goal_result_on_the_chain(lines: &[Line]) -> bool {
    !results_of(lines, "goal").is_empty()
}

/// An hourly goal over several holdings: each wake at the reported time runs the goal once and
/// reports the next hour; a repeated wake for the same time runs nothing; a wake two hours late
/// runs once, for the latest time, and the hours go on from there.
#[tokio::test]
async fn an_hourly_goal_runs_once_per_scheduled_time_across_holdings() {
    let cloud = Cloud::new();
    let registered = cloud
        .host("registering")
        .deliver(cloud.chat, prompt(ASKS_FOR_A_SCHEDULE))
        .await
        .unwrap();
    assert!(registered.acknowledged, "{registered:?}");
    let results = results_of(&cloud.lines(), "goal");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["state"], "scheduled");
    assert_eq!(cloud.runs(), 0);
    let mut due = wake_at(&registered);

    for holding in 1..=3 {
        let report = cloud.wake(&format!("hourly {holding}"), due).await;
        assert_eq!(cloud.runs(), holding);
        assert_eq!(fires(&cloud.lines()).len(), holding);
        assert_eq!(wake_at(&report), due + HOUR);
        // The scheduler's wake for the same time again, as a redelivery would bring it.
        let repeated = cloud.wake(&format!("repeated {holding}"), due).await;
        assert_eq!(cloud.runs(), holding);
        assert_eq!(wake_at(&repeated), due + HOUR);
        due += HOUR;
    }

    let late = cloud.wake("late", due + 2 * HOUR).await;
    assert_eq!(cloud.runs(), 4);
    assert_eq!(fires(&cloud.lines()).len(), 4);
    assert_eq!(wake_at(&late), due + 3 * HOUR);
    assert!(
        due.duration_since(UNIX_EPOCH).unwrap()
            > SystemTime::now().duration_since(UNIX_EPOCH).unwrap(),
        "the host's due times, not the nodes' clocks, decided every fire"
    );
}

/// The holder dies as the call's result is about to commit: whether the registered Loop
/// committed before it, and the redelivered turn settles the call by that Loop, or went down
/// with it, and the call is made again, one goal is scheduled, once.
#[tokio::test]
async fn a_crash_before_a_scheduled_goals_result_neither_loses_nor_repeats_the_goal() {
    let cloud = Cloud::new();
    let task = prompt(ASKS_FOR_A_SCHEDULE);
    let holding = cloud.host("crashing").acquire(cloud.chat).unwrap();
    holding.crash_at(CrashPoint::BeforeCommit(puts_the_goal_result_on_the_chain));
    let crashed = holding.run(task.clone()).await;
    assert_eq!(crashed.halt, Some(Halt::Crashed), "{crashed:?}");
    assert!(results_of(&cloud.lines(), "goal").is_empty());

    cloud.clock.advance(LEASE_TTL + Duration::from_secs(1));
    let redelivered = cloud
        .host("surviving")
        .deliver(cloud.chat, task)
        .await
        .unwrap();
    assert!(redelivered.acknowledged, "{redelivered:?}");
    let results = results_of(&cloud.lines(), "goal");
    assert_eq!(results.len(), 1, "{results:?}");
    assert_eq!(results[0]["state"], "scheduled");
    assert_eq!(scheduled_loops(&cloud.lines()).len(), 1);

    let due = wake_at(&redelivered);
    cloud.wake("hourly", due).await;
    assert_eq!(cloud.runs(), 1);
}
