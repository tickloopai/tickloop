//! The whole stateless-host contract through a reference node: a holding claims the family's
//! tasks while it runs and acknowledges each on its own condition; it hands the family over at
//! its holding-time cap or on SIGTERM, where each Run stops at a safe point and the next holder
//! goes on with the same attempt; a Run still going at the ceiling ends as `Interrupted{Handover}`
//! and is continued without counting as a lost host; the scheduler's sweep takes over a family
//! whose holder crashed after every task was acknowledged, and gives up on a family that keeps
//! crashing until the user acts.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use serde_json::json;
use tickloop_agent_artifacts::ClaimedFeedback;
use tickloop_agent_base::network::Reach;
use tickloop_agent_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_agent_protocol::{
    AgentHostOptions, Content, Decision, EventSourceBinding, ExecutionMode, PermissionRule,
    PromptPolicy, QuestionAnswer,
};
use tickloop_agent_runtime::transcript::{LoadRange, TranscriptStore};
use tickloop_agent_runtime::{
    Agent, Delivery, ExternalDelivery, HandOver, HarnessOptions, HarnessSeams, ParentSelection,
    QuestionReply, ToolPolicy, TurnOp, TurnRequest, UserMessage, WaitHosting, Waits,
};
use tickloop_agent_test_support::event_stream::MemoryEventStream;
use tickloop_agent_test_support::harness::ChosenModels;
use tickloop_agent_test_support::routed::has_tool_result;
use tickloop_agent_test_support::stateless_host::{
    CLAIM_VISIBILITY, DurableChats, Halt, HoldingEnd, HoldingReport, HostConfig, LeaseError,
    ManualClock, NodeConfig, ReferenceHost, ReferenceNode, ReferenceScheduler, TaskKind, TaskState,
    UNCLEAN_HOLDINGS_LIMIT,
};
use tickloop_agent_test_support::{
    ScriptedModel, call, config, single_question, text, title_output,
};
use tickloop_agent_transcript::{
    InputState, InterruptedBy, Line, ResponseEnding, ResponseEntry, ResponseState, ResumePoint,
    SettledOutcome, WaitEntry, WaitKind, WaitState,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::durable_family::{last_completions, messages_saying, task_of};

const LEASE_TTL: Duration = Duration::from_secs(30);
/// The clock's step: shorter than the lease's TTL minus its margin, so heartbeats keep up.
const TICK: Duration = Duration::from_secs(5);
const PROMPT: &str = "Prepare the report.";
const DONE: &str = "Report ready.";
const FEEDBACK_HEADING: &str = "@tickloop feedback request-1 on artifact doc";
const UNKNOWN_OUTCOME: &str = "outcome is unknown";

/// How the model answers a request.
enum Reply {
    Now(Vec<Content>),
    /// Streams until `ready` holds, then gives the content.
    When(Arc<dyn Fn() -> bool + Send + Sync>, Vec<Content>),
    /// Streams until the request is cancelled.
    Never,
    /// The vendor asks to retry in an hour.
    RateLimited,
}

type Respond = dyn Fn(&ModelRequest) -> Reply + Send + Sync;

/// A model the test holds back: it answers each request from its content, when the test lets
/// it, and honours cancellation while it streams.
struct ControlledModel {
    requests: std::sync::Mutex<Vec<ModelRequest>>,
    respond: Box<Respond>,
}

impl ControlledModel {
    fn new(respond: impl Fn(&ModelRequest) -> Reply + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            requests: std::sync::Mutex::default(),
            respond: Box::new(respond),
        })
    }

    /// The conversation requests made so far: those that offer tools.
    fn conversation(&self) -> Vec<ModelRequest> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| !request.tools.is_empty())
            .cloned()
            .collect()
    }

    fn conversation_of(&self, task: Option<&str>) -> usize {
        self.conversation()
            .iter()
            .filter(|request| task_of(request).as_deref() == task)
            .count()
    }
}

#[async_trait]
impl ModelClient for ControlledModel {
    async fn generate_title(
        &self,
        request: ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        Ok(title_output(request))
    }

    fn identity(&self) -> tickloop_agent_protocol::ModelIdentity {
        ScriptedModel::new(vec![]).identity()
    }

    async fn complete(
        &self,
        request: ModelRequest,
        events: tokio::sync::mpsc::UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        self.requests.lock().unwrap().push(request.clone());
        let content = match (self.respond)(&request) {
            Reply::Now(content) => content,
            Reply::When(ready, content) => loop {
                if ready() {
                    break content;
                }
                let _ = events.send(ModelDelta::Text(".".into()));
                tokio::select! {
                    _ = cancel.cancelled() => return Err(ModelError::Cancelled),
                    _ = tokio::time::sleep(Duration::from_millis(5)) => {}
                }
            },
            Reply::Never => {
                let _ = events.send(ModelDelta::Text("…".into()));
                cancel.cancelled().await;
                return Err(ModelError::Cancelled);
            }
            Reply::RateLimited => {
                return Err(ModelError::HttpResponse {
                    status: 429,
                    description: "slow down".into(),
                    retry_after: Some(Duration::from_secs(3_600)),
                    should_retry: Some(true),
                });
            }
        };
        Ok(ModelOutput {
            private_source: None,
            content,
            usage: Default::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

fn flag() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

fn when(flag: &Arc<AtomicBool>, content: Vec<Content>) -> Reply {
    let flag = flag.clone();
    Reply::When(Arc::new(move || flag.load(Ordering::SeqCst)), content)
}

fn allow_web_fetch() -> PermissionRule {
    PermissionRule {
        id: Uuid::new_v4(),
        decision: Decision::Allow,
        tool: "WebFetch".into(),
        specifier: None,
        input: None,
        loop_id: None,
        persistent: false,
    }
}

/// A cloud Harness whose Chats fetch pages, ask questions and delegate, with durable waits.
fn harness(
    store: Arc<dyn TranscriptStore>,
    model: Arc<ControlledModel>,
    request_gates: Vec<Arc<tokio::sync::Semaphore>>,
) -> Agent {
    Agent::with_seams(
        HarnessSeams {
            transcripts: Some(store),
            environment: None,
            models: ChosenModels::new(model),
        },
        HarnessOptions {
            host: AgentHostOptions {
                prompt_policy: PromptPolicy::DenyOnPrompt,
                execution_mode_override: Some(ExecutionMode::FullAccess),
                ..AgentHostOptions::product_defaults()
            },
            tools: ToolPolicy {
                questions: true,
                delegation: true,
                family_messages: true,
                folderless_reach: Reach::Anywhere,
                ..ToolPolicy::web_fetch_only()
            },
            host_rules: vec![allow_web_fetch()],
            waits: WaitHosting::Durable,
            request_gates,
            ..HarnessOptions::local(config())
        },
    )
    .unwrap()
}

/// The durable side, the stream and the clock of the cloud, and its families.
struct Cloud {
    clock: Arc<ManualClock>,
    chats: Arc<DurableChats>,
    stream: Arc<MemoryEventStream>,
    model: Arc<ControlledModel>,
    gates: Vec<Arc<tokio::sync::Semaphore>>,
    root: Uuid,
}

impl Cloud {
    fn new(model: Arc<ControlledModel>) -> Self {
        let clock = ManualClock::new();
        Self {
            chats: DurableChats::new(clock.clone()),
            clock,
            stream: Arc::new(MemoryEventStream::new()),
            model,
            gates: Vec::new(),
            root: Uuid::new_v4(),
        }
    }

    fn node(&self, holder: &str, config: NodeConfig) -> Arc<ReferenceNode> {
        let model = self.model.clone();
        let gates = self.gates.clone();
        Arc::new(ReferenceNode::new(
            ReferenceHost::new(
                holder,
                self.chats.clone(),
                self.stream.clone(),
                self.clock.clone(),
                HostConfig::default(),
                move |store| harness(store, model.clone(), gates.clone()),
            ),
            config,
        ))
    }

    fn scheduler(&self) -> ReferenceScheduler {
        ReferenceScheduler::new(self.chats.clone())
    }

    /// The user's message to the root Chat of `root`, queued as a task of its own.
    fn user_turn(&self, root: Uuid, text: &str) -> (Uuid, TurnRequest) {
        let request = prompt(text);
        let task = self
            .chats
            .enqueue_by_user(
                root,
                &format!("turn:{}", request.input_id),
                TaskKind::Turn {
                    member: None,
                    request: Box::new(request.clone()),
                },
            )
            .unwrap();
        (task, request)
    }

    fn task_state(&self, root: Uuid, task: Uuid) -> TaskState {
        self.task(root, task).state
    }

    fn task(
        &self,
        root: Uuid,
        task: Uuid,
    ) -> tickloop_agent_test_support::stateless_host::TaskView {
        self.chats
            .tasks(root)
            .into_iter()
            .find(|view| view.id == task)
            .unwrap()
    }

    fn lines(&self) -> Vec<Line> {
        self.chats.history(self.root).lines
    }

    fn member_lines(&self) -> Vec<Line> {
        self.chats
            .members(self.root)
            .into_iter()
            .flat_map(|member| self.chats.member_lines(member))
            .collect()
    }

    /// Moves the host's clock on by `time`, a heartbeat's step at a time.
    async fn pass(&self, time: Duration) {
        let mut passed = Duration::ZERO;
        while passed < time {
            self.clock.advance(TICK);
            passed += TICK;
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
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

fn spawn_holding(node: &Arc<ReferenceNode>, root: Uuid) -> tokio::task::JoinHandle<HoldingReport> {
    let node = node.clone();
    tokio::spawn(async move { node.hold(root).await.unwrap() })
}

async fn eventually(what: &str, check: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while !check() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting until {what}"));
}

/// The latest record of each answer to `input`, in the order they started.
fn answers(lines: &[Line], input: Uuid) -> Vec<ResponseEntry> {
    let mut latest: Vec<ResponseEntry> = Vec::new();
    for line in lines {
        if let Line::Response(entry) = line
            && entry.input_id == Some(input)
        {
            match latest
                .iter_mut()
                .find(|known| known.response_id == entry.response_id)
            {
                Some(known) => *known = entry.clone(),
                None => latest.push(entry.clone()),
            }
        }
    }
    latest
}

/// The latest record of each wait the Lines hold.
fn waits(lines: &[Line]) -> Vec<WaitEntry> {
    let mut latest: Vec<WaitEntry> = Vec::new();
    for line in lines {
        if let Line::Wait(entry) = line {
            match latest
                .iter_mut()
                .find(|known| known.wait_id == entry.wait_id)
            {
                Some(known) => *known = entry.clone(),
                None => latest.push(entry.clone()),
            }
        }
    }
    latest
}

fn handover_waits(lines: &[Line]) -> Vec<WaitEntry> {
    waits(lines)
        .into_iter()
        .filter(|wait| matches!(wait.kind, WaitKind::Handover))
        .collect()
}

fn ended(answer: &ResponseEntry) -> Option<&ResponseEnding> {
    match &answer.state {
        ResponseState::Ended { ending } => Some(ending),
        _ => None,
    }
}

fn input_state(lines: &[Line], input: Uuid) -> Option<InputState> {
    lines.iter().rev().find_map(|line| match line {
        Line::Input(entry) if entry.input_id == input => Some(entry.state.clone()),
        _ => None,
    })
}

/// How many times the Lines carry the call `call_id`, and its result.
fn calls_and_results(lines: &[Line], call_id: &str) -> (usize, usize) {
    let contents = || {
        lines
            .iter()
            .filter_map(|line| match line {
                Line::User(entry) | Line::Assistant(entry) => Some(&entry.message.content),
                _ => None,
            })
            .flatten()
    };
    (
        contents()
            .filter(|content| matches!(content, Content::ToolUse { id, .. } if id == call_id))
            .count(),
        contents()
            .filter(|content| {
                matches!(content, Content::ToolResult { tool_use_id, .. } if tool_use_id == call_id)
            })
            .count(),
    )
}

fn step_results(request: &ModelRequest) -> usize {
    request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter(|content| {
            matches!(content, Content::ToolResult { tool_use_id, .. } if tool_use_id.starts_with("step_"))
        })
        .count()
}

fn mentions(request: &ModelRequest, text: &str) -> bool {
    serde_json::to_string(&request.messages)
        .unwrap()
        .contains(text)
}

fn conversation_mentions(request: &ModelRequest, text: &str) -> bool {
    serde_json::to_string(
        &request
            .messages
            .iter()
            .filter(|message| message.role != tickloop_agent_protocol::Role::System)
            .collect::<Vec<_>>(),
    )
    .unwrap()
    .contains(text)
}

fn completed(lines: &[Line], input: Uuid) {
    assert!(
        matches!(
            input_state(lines, input),
            Some(InputState::Settled {
                outcome: SettledOutcome::Completed { .. }
            })
        ),
        "{:?}",
        input_state(lines, input)
    );
}

/// A Run that makes four calls, one model request at a time as the test allows, outlives three
/// holding caps. Each holding hands it over at the safe point before its next call, and the
/// next goes on with the same attempt: no answer is interrupted, no call is repeated and the
/// model never reads an unknown outcome. The user's task goes back to the queue each time
/// without spending an attempt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_longer_than_three_holding_caps_finishes_in_one_attempt() {
    const STEPS: usize = 4;
    let allowed = Arc::new(AtomicUsize::new(0));
    let model = ControlledModel::new({
        let allowed = allowed.clone();
        move |request| {
            let step = step_results(request);
            if step >= STEPS {
                return Reply::Now(text(DONE));
            }
            let allowed = allowed.clone();
            Reply::When(
                Arc::new(move || allowed.load(Ordering::SeqCst) > step),
                call(&format!("step_{step}"), "ListSubagents", json!({})),
            )
        }
    });
    let cloud = Cloud::new(model.clone());
    let config = NodeConfig {
        holding_cap: Duration::from_secs(60),
        handover_ceiling: Duration::from_secs(30),
    };
    let (task, request) = cloud.user_turn(cloud.root, PROMPT);
    for holding in 0..3 {
        let node = cloud.node(&format!("node-{holding}"), config);
        let running = spawn_holding(&node, cloud.root);
        eventually("the Run streams its next request", || {
            model.conversation().len() == holding + 1
        })
        .await;
        cloud.pass(config.holding_cap + TICK).await;
        eventually("the holding hands over", || {
            node.is_handing_over(cloud.root)
        })
        .await;
        allowed.fetch_add(1, Ordering::SeqCst);
        let report = running.await.unwrap();
        assert_eq!(
            report.end,
            HoldingEnd::HandedOver(HandOver {
                drained: true,
                interrupted: Vec::new(),
            }),
            "{report:?}"
        );
        assert_eq!(report.returned, vec![task]);
        let stopped = handover_waits(&cloud.lines());
        assert_eq!(stopped.len(), holding + 1);
        assert!(matches!(
            &stopped.last().unwrap().resume,
            ResumePoint::InBatch { call_id, .. } if *call_id == format!("step_{holding}")
        ));
    }
    allowed.store(STEPS, Ordering::SeqCst);
    let report = cloud
        .node("node-last", config)
        .hold(cloud.root)
        .await
        .unwrap();
    assert_eq!(report.end, HoldingEnd::Quiet, "{report:?}");

    let lines = cloud.lines();
    completed(&lines, request.input_id);
    let attempts = answers(&lines, request.input_id);
    assert_eq!(attempts.len(), 1, "{attempts:?}");
    assert!(matches!(
        ended(&attempts[0]),
        Some(ResponseEnding::Completed)
    ));
    assert!(
        handover_waits(&lines)
            .iter()
            .all(|wait| matches!(wait.state, WaitState::Appended))
    );
    let requests = model.conversation();
    assert_eq!(requests.len(), STEPS + 1);
    for (index, request) in requests.iter().enumerate() {
        assert_eq!(step_results(request), index);
        assert!(!mentions(request, UNKNOWN_OUTCOME));
    }
    for step in 0..STEPS {
        assert_eq!(calls_and_results(&lines, &format!("step_{step}")), (1, 1));
    }
    let view = cloud.task(cloud.root, task);
    assert_eq!(view.state, TaskState::Acknowledged);
    assert_eq!((view.attempts, view.deliveries), (1, 4));
    let handovers = cloud
        .chats
        .tasks(cloud.root)
        .into_iter()
        .filter(|view| view.key.starts_with("handover:"))
        .collect::<Vec<_>>();
    assert_eq!(handovers.len(), 3);
    assert!(
        handovers
            .iter()
            .all(|view| view.state == TaskState::Acknowledged)
    );
}

/// SIGTERM while the model streams: the stream finishes, the Run stops before the call it
/// made, and the next holder makes that call and the next request; the model is asked once for
/// each.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigterm_mid_model_request_lets_the_stream_finish_and_the_next_holder_makes_the_call() {
    let released = flag();
    let model = ControlledModel::new({
        let released = released.clone();
        move |request| {
            if has_tool_result(request, "list") {
                Reply::Now(text(DONE))
            } else {
                when(&released, call("list", "ListSubagents", json!({})))
            }
        }
    });
    let cloud = Cloud::new(model.clone());
    let (task, request) = cloud.user_turn(cloud.root, PROMPT);
    let node = cloud.node("terminated", NodeConfig::default());
    let running = spawn_holding(&node, cloud.root);
    eventually("the model streams", || model.conversation().len() == 1).await;
    node.sigterm();
    eventually("the holding hands over", || {
        node.is_handing_over(cloud.root)
    })
    .await;
    released.store(true, Ordering::SeqCst);
    let report = running.await.unwrap();
    assert!(
        matches!(
            &report.end,
            HoldingEnd::HandedOver(HandOver { drained: true, .. })
        ),
        "{report:?}"
    );
    let lines = cloud.lines();
    let [stopped] = handover_waits(&lines).try_into().unwrap();
    assert!(matches!(stopped.state, WaitState::Open));
    assert!(matches!(&stopped.resume, ResumePoint::InBatch { call_id, .. } if call_id == "list"));
    assert_eq!(
        answers(&lines, request.input_id)[0].state,
        ResponseState::Suspended
    );
    assert_eq!(calls_and_results(&lines, "list"), (1, 0));
    assert_eq!(cloud.chats.waiting(cloud.root), None);

    let report = cloud
        .node("next", NodeConfig::default())
        .hold(cloud.root)
        .await
        .unwrap();
    assert_eq!(report.end, HoldingEnd::Quiet, "{report:?}");
    let lines = cloud.lines();
    completed(&lines, request.input_id);
    assert_eq!(answers(&lines, request.input_id).len(), 1);
    assert_eq!(calls_and_results(&lines, "list"), (1, 1));
    assert_eq!(model.conversation().len(), 2);
    let view = cloud.task(cloud.root, task);
    assert_eq!(view.state, TaskState::Acknowledged);
    assert_eq!(view.attempts, 1);
}

/// A page server whose single page answers only once the test opens it; it counts requests.
async fn held_page(open: Arc<AtomicBool>, hits: Arc<AtomicUsize>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        while let Ok((mut socket, _)) = listener.accept().await {
            let open = open.clone();
            let hits = hits.clone();
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buffer = [0; 1024];
                while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let Ok(length) = socket.read(&mut buffer).await else {
                        return;
                    };
                    if length == 0 {
                        return;
                    }
                    request.extend_from_slice(&buffer[..length]);
                }
                hits.fetch_add(1, Ordering::SeqCst);
                while !open.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                let body = "<p>Delivery is on Friday.</p>";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    format!("http://localhost:{port}/delivery")
}

/// SIGTERM while a tool runs: the fetch finishes and its result is kept, the Run stops before
/// the next call of the batch, and the next holder makes only that one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigterm_mid_tool_call_finishes_the_call_and_stops_before_the_next_one() {
    let open = flag();
    let hits = Arc::new(AtomicUsize::new(0));
    let url = held_page(open.clone(), hits.clone()).await;
    let model = ControlledModel::new(move |request| {
        if request.tools.is_empty() {
            Reply::Now(text("Delivery is on Friday."))
        } else if has_tool_result(request, "list") {
            Reply::Now(text(DONE))
        } else {
            Reply::Now(vec![
                Content::ToolUse {
                    id: "page".into(),
                    name: "WebFetch".into(),
                    input: json!({"url": url, "prompt": "When?"}),
                },
                Content::ToolUse {
                    id: "list".into(),
                    name: "ListSubagents".into(),
                    input: json!({}),
                },
            ])
        }
    });
    let cloud = Cloud::new(model.clone());
    let (_, request) = cloud.user_turn(cloud.root, PROMPT);
    let node = cloud.node("terminated", NodeConfig::default());
    let running = spawn_holding(&node, cloud.root);
    eventually("the page is requested", || hits.load(Ordering::SeqCst) == 1).await;
    node.sigterm();
    eventually("the holding hands over", || {
        node.is_handing_over(cloud.root)
    })
    .await;
    open.store(true, Ordering::SeqCst);
    let report = running.await.unwrap();
    assert!(
        matches!(
            &report.end,
            HoldingEnd::HandedOver(HandOver { drained: true, .. })
        ),
        "{report:?}"
    );
    let lines = cloud.lines();
    assert_eq!(calls_and_results(&lines, "page"), (1, 1));
    assert_eq!(calls_and_results(&lines, "list"), (1, 0));
    let [stopped] = handover_waits(&lines).try_into().unwrap();
    assert!(matches!(&stopped.resume, ResumePoint::InBatch { call_id, .. } if call_id == "list"));

    let report = cloud
        .node("next", NodeConfig::default())
        .hold(cloud.root)
        .await
        .unwrap();
    assert_eq!(report.end, HoldingEnd::Quiet, "{report:?}");
    let lines = cloud.lines();
    completed(&lines, request.input_id);
    assert_eq!(answers(&lines, request.input_id).len(), 1);
    assert_eq!(calls_and_results(&lines, "page"), (1, 1));
    assert_eq!(calls_and_results(&lines, "list"), (1, 1));
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert_eq!(model.conversation().len(), 2);
}

/// A Run waiting for a request gate, and one in a retry backoff, stop there at once: neither is
/// a request in flight. The next holder makes the request with a fresh retry budget.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_gate_wait_and_a_retry_backoff_are_safe_points() {
    for in_backoff in [false, true] {
        let may_answer = flag();
        let model = ControlledModel::new({
            let may_answer = may_answer.clone();
            move |request| {
                if !has_tool_result(request, "list") {
                    Reply::Now(call("list", "ListSubagents", json!({})))
                } else if in_backoff && !may_answer.load(Ordering::SeqCst) {
                    Reply::RateLimited
                } else {
                    Reply::Now(text(DONE))
                }
            }
        });
        let mut cloud = Cloud::new(model.clone());
        let gate = Arc::new(tokio::sync::Semaphore::new(100));
        cloud.gates = vec![gate.clone()];
        // Another holder of the family's gate takes every permit: the first request waits.
        let taken = (!in_backoff).then(|| gate.clone().try_acquire_many_owned(100).unwrap());
        let (_, request) = cloud.user_turn(cloud.root, PROMPT);
        let node = cloud.node("terminated", NodeConfig::default());
        let running = spawn_holding(&node, cloud.root);
        if in_backoff {
            eventually("the second request is refused", || {
                model.conversation().len() == 2
            })
            .await;
        } else {
            eventually("the answer started", || {
                !answers(&cloud.lines(), request.input_id).is_empty()
            })
            .await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        node.sigterm();
        let report = tokio::time::timeout(Duration::from_secs(10), running)
            .await
            .expect("the Run stops at once")
            .unwrap();
        assert!(
            matches!(
                &report.end,
                HoldingEnd::HandedOver(HandOver { drained: true, .. })
            ),
            "{report:?}"
        );
        let [stopped] = handover_waits(&cloud.lines()).try_into().unwrap();
        assert_eq!(stopped.resume, ResumePoint::BeforeModelRequest);

        may_answer.store(true, Ordering::SeqCst);
        drop(taken);
        let report = cloud
            .node("next", NodeConfig::default())
            .hold(cloud.root)
            .await
            .unwrap();
        assert_eq!(report.end, HoldingEnd::Quiet, "{report:?}");
        let lines = cloud.lines();
        completed(&lines, request.input_id);
        assert_eq!(answers(&lines, request.input_id).len(), 1);
        assert_eq!(
            model.conversation().len(),
            if in_backoff { 3 } else { 2 },
            "in backoff: {in_backoff}"
        );
        assert_eq!(calls_and_results(&lines, "list"), (1, 1));
    }
}

/// A child's stream outlasts the ceiling while its parent waits in the foreground. The child's
/// answer ends as `Interrupted{Handover}`, the parent is released with its wait open, and after
/// the handover the child goes on with a second attempt that counts as no lost host; the parent
/// receives the child's final report exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_child_stream_past_the_ceiling_reports_once_to_its_waiting_parent_after_the_handover() {
    let may_finish = flag();
    let model = ControlledModel::new({
        let may_finish = may_finish.clone();
        move |request| match task_of(request).as_deref() {
            Some("child") if may_finish.load(Ordering::SeqCst) => {
                Reply::Now(text("Child report: weekly."))
            }
            Some("child") => Reply::Never,
            _ if !has_tool_result(request, "delegate") => Reply::Now(call(
                "delegate",
                "Subagent",
                json!({"task": "child", "name": "writer"}),
            )),
            _ => Reply::Now(text(DONE)),
        }
    });
    let cloud = Cloud::new(model.clone());
    let config = NodeConfig {
        handover_ceiling: Duration::from_millis(300),
        ..NodeConfig::default()
    };
    let (task, request) = cloud.user_turn(cloud.root, PROMPT);
    let node = cloud.node("terminated", config);
    let running = spawn_holding(&node, cloud.root);
    eventually("the child streams", || {
        model.conversation_of(Some("child")) == 1
    })
    .await;
    node.sigterm();
    let report = running.await.unwrap();
    let HoldingEnd::HandedOver(HandOver {
        drained: false,
        interrupted,
    }) = &report.end
    else {
        panic!("the child is interrupted at the ceiling: {report:?}");
    };
    assert_eq!(interrupted.len(), 1);
    assert_eq!(report.returned, vec![task]);
    let parent_waits = waits(&cloud.lines());
    assert!(
        parent_waits
            .iter()
            .any(|wait| matches!(wait.kind, WaitKind::Subagent { .. })
                && matches!(wait.state, WaitState::Open))
    );
    assert!(last_completions(&cloud.lines()).is_empty());

    may_finish.store(true, Ordering::SeqCst);
    let report = cloud.node("next", config).hold(cloud.root).await.unwrap();
    assert_eq!(report.end, HoldingEnd::Quiet, "{report:?}");
    let lines = cloud.lines();
    completed(&lines, request.input_id);
    let parent = answers(&lines, request.input_id);
    assert_eq!(parent.len(), 1, "{parent:?}");
    let child_lines = cloud.member_lines();
    let child_input = child_lines
        .iter()
        .find_map(|line| match line {
            Line::Response(entry) => entry.input_id,
            _ => None,
        })
        .unwrap();
    let child = answers(&child_lines, child_input);
    assert_eq!(child.len(), 2, "{child:?}");
    assert_eq!(
        ended(&child[0]),
        Some(&ResponseEnding::Interrupted {
            by: InterruptedBy::Handover
        })
    );
    assert_eq!(ended(&child[1]), Some(&ResponseEnding::Completed));
    assert_eq!(last_completions(&lines).len(), 1);
    assert_eq!(messages_saying(&lines, "Child report: weekly."), 1);
    let last = model
        .conversation()
        .into_iter()
        .rfind(|request| task_of(request).is_none())
        .unwrap();
    assert_eq!(
        serde_json::to_string(&last.messages)
            .unwrap()
            .matches("Child report: weekly.")
            .count(),
        1
    );
}

/// The delivery of an @tickloop request for the Loop `target`.
fn deliver(target: Uuid) -> TurnRequest {
    TurnRequest::deliver(
        target,
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
        .unwrap(),
    )
}

/// Links the Loop `target` to the document's @tickloop requests, as the user's earlier link
/// recorded it in the Transcript.
async fn link_to_the_document(cloud: &Cloud, target: Uuid) {
    let chat = cloud.chats.harness_chat(cloud.root).unwrap();
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
}

fn loop_of(lines: &[Line], input: Uuid) -> Uuid {
    lines
        .iter()
        .find_map(|line| match line {
            Line::Input(entry) if entry.input_id == input => Some(entry.loop_id),
            _ => None,
        })
        .unwrap()
}

/// Every task is acknowledged: the user's turn once it ended while its background child runs,
/// an @tickloop request once it was accepted while its Run streams. Then the holder crashes.
/// Nothing is queued, so the scheduler's sweep takes the family over once the lease expired, and
/// the child and the request each go on to their end exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crash_after_every_task_was_acknowledged_is_taken_over_by_the_sweep() {
    let may_finish = flag();
    let model = ControlledModel::new({
        let may_finish = may_finish.clone();
        move |request| {
            let goes_on = may_finish.load(Ordering::SeqCst);
            match task_of(request).as_deref() {
                Some("watch") if goes_on => Reply::Now(text("Watcher report.")),
                Some("watch") => Reply::Never,
                _ if mentions(request, FEEDBACK_HEADING)
                    && !mentions(request, "Title tightened.") =>
                {
                    if goes_on {
                        Reply::Now(text("Title tightened."))
                    } else {
                        Reply::Never
                    }
                }
                _ if mentions(request, "Delegate the watch.")
                    && !has_tool_result(request, "watch") =>
                {
                    Reply::Now(call(
                        "watch",
                        "Subagent",
                        json!({"task": "watch", "name": "watcher", "run_in_background": true}),
                    ))
                }
                _ => Reply::Now(text("Watching.")),
            }
        }
    });
    let cloud = Cloud::new(model.clone());
    let (_, linked) = cloud.user_turn(cloud.root, "Watch the document.");
    let report = cloud
        .node("linking", NodeConfig::default())
        .hold(cloud.root)
        .await
        .unwrap();
    assert_eq!(report.end, HoldingEnd::Quiet, "{report:?}");
    let target = loop_of(&cloud.lines(), linked.input_id);
    link_to_the_document(&cloud, target).await;
    let (turn_task, _) = cloud.user_turn(cloud.root, "Delegate the watch.");
    let node = cloud.node("crashing", NodeConfig::default());
    let running = spawn_holding(&node, cloud.root);
    eventually("the turn is acknowledged", || {
        cloud.task_state(cloud.root, turn_task) == TaskState::Acknowledged
    })
    .await;
    let request = deliver(target);
    let delivery_task = cloud
        .chats
        .enqueue(
            cloud.root,
            &format!("feedback:{}", request.input_id),
            TaskKind::Turn {
                member: None,
                request: Box::new(request.clone()),
            },
        )
        .unwrap();
    eventually("the request is acknowledged and streams", || {
        cloud.task_state(cloud.root, delivery_task) == TaskState::Acknowledged
            && model
                .conversation()
                .iter()
                .any(|request| mentions(request, FEEDBACK_HEADING))
            && model.conversation_of(Some("watch")) == 1
    })
    .await;
    assert!(node.crash(cloud.root));
    let report = running.await.unwrap();
    assert_eq!(report.end, HoldingEnd::Halted(Halt::Crashed), "{report:?}");
    assert!(report.refused.is_empty(), "{report:?}");
    assert!(!cloud.chats.has_open_tasks(cloud.root));

    let scheduler = cloud.scheduler();
    assert!(scheduler.tick(SystemTime::now()).is_empty());
    cloud.pass(LEASE_TTL + TICK).await;
    assert_eq!(
        scheduler.tick(SystemTime::now()),
        vec![format!("takeover:{}:{}", cloud.root, report.epoch)]
    );
    assert!(scheduler.tick(SystemTime::now()).is_empty());
    may_finish.store(true, Ordering::SeqCst);
    let report = cloud
        .node("surviving", NodeConfig::default())
        .hold(cloud.root)
        .await
        .unwrap();
    assert_eq!(report.end, HoldingEnd::Quiet, "{report:?}");
    assert!(!cloud.chats.has_open_tasks(cloud.root));
    assert_eq!(cloud.chats.consecutive_unclean(cloud.root), 0);

    let lines = cloud.lines();
    completed(&lines, request.input_id);
    assert_eq!(messages_saying(&lines, "Watcher report."), 1);
    assert_eq!(messages_saying(&lines, FEEDBACK_HEADING), 1);
    let feedback = answers(&lines, request.input_id);
    assert_eq!(feedback.len(), 2, "{feedback:?}");
    assert_eq!(
        ended(&feedback[0]),
        Some(&ResponseEnding::Interrupted {
            by: InterruptedBy::Host
        })
    );
    assert_eq!(last_completions(&lines).len(), 1);
    for task in [turn_task, delivery_task] {
        assert_eq!(cloud.task(cloud.root, task).deliveries, 1);
    }
}

/// The answer is acknowledged once accepted, while the Run it resumed streams on; the holder
/// crashes. The sweep takes the family over, and the Run goes on without asking again or
/// repeating the answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crash_after_an_answer_was_acknowledged_is_taken_over_by_the_sweep() {
    let may_finish = flag();
    let model = ControlledModel::new({
        let may_finish = may_finish.clone();
        move |request| {
            if !has_tool_result(request, "question") {
                Reply::Now(call(
                    "question",
                    "AskUserQuestion",
                    single_question("Which format?"),
                ))
            } else if may_finish.load(Ordering::SeqCst) {
                Reply::Now(text(DONE))
            } else {
                Reply::Never
            }
        }
    });
    let cloud = Cloud::new(model.clone());
    let (_, turn) = cloud.user_turn(cloud.root, PROMPT);
    let asked = cloud
        .node("asking", NodeConfig::default())
        .hold(cloud.root)
        .await
        .unwrap();
    assert_eq!(asked.end, HoldingEnd::Quiet, "{asked:?}");
    let Some(Waits { people, .. }) = &asked.waiting else {
        panic!("the family waits for the user: {asked:?}");
    };
    let answer = TurnRequest {
        input_id: Uuid::new_v4(),
        op: TurnOp::Answer {
            request: people[0].request,
            reply: QuestionReply::Answered(vec![QuestionAnswer::other("Weekly")]),
        },
        delivery: Delivery::Queue,
        host_context: None,
    };
    let answer_task = cloud
        .chats
        .enqueue_by_user(
            cloud.root,
            &format!("answer:{}", answer.input_id),
            TaskKind::Turn {
                member: None,
                request: Box::new(answer),
            },
        )
        .unwrap();
    let node = cloud.node("crashing", NodeConfig::default());
    let running = spawn_holding(&node, cloud.root);
    eventually("the answer is acknowledged and the Run streams", || {
        cloud.task_state(cloud.root, answer_task) == TaskState::Acknowledged
            && model.conversation().len() == 2
    })
    .await;
    assert!(node.crash(cloud.root));
    let crashed = running.await.unwrap();
    assert_eq!(crashed.end, HoldingEnd::Halted(Halt::Crashed));
    assert!(!cloud.chats.has_open_tasks(cloud.root));

    cloud.pass(LEASE_TTL + TICK).await;
    assert_eq!(
        cloud.scheduler().tick(SystemTime::now()),
        vec![format!("takeover:{}:{}", cloud.root, crashed.epoch)]
    );
    may_finish.store(true, Ordering::SeqCst);
    let report = cloud
        .node("surviving", NodeConfig::default())
        .hold(cloud.root)
        .await
        .unwrap();
    assert_eq!(report.end, HoldingEnd::Quiet, "{report:?}");
    let lines = cloud.lines();
    completed(&lines, turn.input_id);
    assert_eq!(calls_and_results(&lines, "question"), (1, 1));
    assert_eq!(cloud.task(cloud.root, answer_task).deliveries, 1);
}

/// Each of five holdings ends at its cap with the Run still streaming past the ceiling. Every
/// one gives the user's task back without spending an attempt and continues the input with a
/// new attempt that counts as no lost host, so the sixth finishes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn five_ceiling_handovers_neither_dead_letter_the_task_nor_fail_the_input() {
    const HANDOVERS: usize = 5;
    let model = ControlledModel::new(|_| Reply::Never);
    let finishing = ControlledModel::new(|_| Reply::Now(text(DONE)));
    let mut cloud = Cloud::new(model.clone());
    let config = NodeConfig {
        holding_cap: Duration::from_secs(60),
        handover_ceiling: Duration::from_millis(100),
    };
    let (task, request) = cloud.user_turn(cloud.root, PROMPT);
    for holding in 0..HANDOVERS {
        let node = cloud.node(&format!("node-{holding}"), config);
        let running = spawn_holding(&node, cloud.root);
        eventually("the Run streams", || {
            model.conversation().len() == holding + 1
        })
        .await;
        cloud.pass(config.holding_cap + TICK).await;
        let report = running.await.unwrap();
        assert!(
            matches!(&report.end, HoldingEnd::HandedOver(HandOver { drained: false, interrupted }) if interrupted.len() == 1),
            "{report:?}"
        );
        assert_eq!(report.returned, vec![task]);
        assert_eq!(cloud.task(cloud.root, task).attempts, 0);
    }
    cloud.model = finishing;
    let report = cloud
        .node("node-last", config)
        .hold(cloud.root)
        .await
        .unwrap();
    assert_eq!(report.end, HoldingEnd::Quiet, "{report:?}");
    let lines = cloud.lines();
    completed(&lines, request.input_id);
    let attempts = answers(&lines, request.input_id);
    assert_eq!(attempts.len(), HANDOVERS + 1);
    assert!(attempts[..HANDOVERS].iter().all(|attempt| ended(attempt)
        == Some(&ResponseEnding::Interrupted {
            by: InterruptedBy::Handover
        })));
    let view = cloud.task(cloud.root, task);
    assert_eq!(view.state, TaskState::Acknowledged);
    assert_eq!((view.attempts, view.deliveries), (1, HANDOVERS as u32 + 1));
}

/// A holding lasts thirty minutes while its family runs: every heartbeat extends the claim of
/// the wake it took, so the wake is never handed out again, and it is acknowledged once when
/// the holding ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wake_claimed_for_a_thirty_minute_holding_is_not_delivered_again() {
    let may_finish = flag();
    let model = ControlledModel::new({
        let may_finish = may_finish.clone();
        move |_| when(&may_finish, text(DONE))
    });
    let cloud = Cloud::new(model.clone());
    let config = NodeConfig {
        holding_cap: Duration::from_secs(60 * 60),
        ..NodeConfig::default()
    };
    let (turn_task, _) = cloud.user_turn(cloud.root, PROMPT);
    let wake = cloud
        .chats
        .enqueue(cloud.root, "wake:test", TaskKind::Wake { due_at: None })
        .unwrap();
    let node = cloud.node("long", config);
    let running = spawn_holding(&node, cloud.root);
    eventually("the Run streams", || model.conversation().len() == 1).await;
    let mut passed = Duration::ZERO;
    while passed < Duration::from_secs(30 * 60) {
        cloud.pass(TICK).await;
        passed += TICK;
        cloud.chats.reclaim_expired_tasks();
        assert_eq!(cloud.task_state(cloud.root, wake), TaskState::Claimed);
    }
    assert!(passed > CLAIM_VISIBILITY * 100);
    may_finish.store(true, Ordering::SeqCst);
    let report = running.await.unwrap();
    assert_eq!(report.end, HoldingEnd::Quiet, "{report:?}");
    for task in [wake, turn_task] {
        let view = cloud.task(cloud.root, task);
        assert_eq!(view.state, TaskState::Acknowledged);
        assert_eq!(view.deliveries, 1);
    }
}

/// The scheduler's wake key carries the epoch of the release that wrote `wake_at`: the same
/// time written back by a later release is queued again, while one release's time is queued
/// once however often the scheduler polls.
#[tokio::test]
async fn the_same_wake_at_from_another_release_is_queued_again() {
    let cloud = Cloud::new(ControlledModel::new(|_| Reply::Now(text(DONE))));
    let scheduler = cloud.scheduler();
    let wake_at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let waiting = Waits {
        wake_at: Some(wake_at),
        ..Waits::default()
    };
    let micros = wake_at
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_micros();
    let first = cloud
        .chats
        .acquire_lease(cloud.root, "first", LEASE_TTL)
        .unwrap();
    cloud.chats.release_waiting(&first, Some(waiting.clone()));
    assert!(scheduler.tick(wake_at - Duration::from_secs(1)).is_empty());
    assert_eq!(
        scheduler.tick(wake_at),
        vec![format!("wake:{}:1:{micros}", cloud.root)]
    );
    assert!(scheduler.tick(wake_at).is_empty());

    let second = cloud
        .chats
        .acquire_lease(cloud.root, "second", LEASE_TTL)
        .unwrap();
    cloud.chats.release_waiting(&second, Some(waiting));
    assert_eq!(
        scheduler.tick(wake_at),
        vec![format!("wake:{}:2:{micros}", cloud.root)]
    );
    let wakes = cloud.chats.tasks(cloud.root);
    assert_eq!(wakes.len(), 2);
    assert!(
        wakes
            .iter()
            .all(|task| matches!(task.state, TaskState::Queued))
    );
}

/// One holding acknowledges a turn, then dies during the next one. The acknowledged turn is
/// never delivered again; only the turn the dead holding claimed spends an attempt, and the
/// next holding finishes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_holding_spends_no_attempt_of_the_tasks_it_finished() {
    let may_finish = flag();
    let model = ControlledModel::new({
        let may_finish = may_finish.clone();
        move |request| {
            if conversation_mentions(request, "Second") {
                when(&may_finish, text(DONE))
            } else {
                Reply::Now(text(DONE))
            }
        }
    });
    let cloud = Cloud::new(model.clone());
    let (first, _) = cloud.user_turn(cloud.root, "First");
    let (second, request) = cloud.user_turn(cloud.root, "Second");
    let node = cloud.node("crashing", NodeConfig::default());
    let running = spawn_holding(&node, cloud.root);
    eventually("the first turn is acknowledged", || {
        cloud.task_state(cloud.root, first) == TaskState::Acknowledged
    })
    .await;
    eventually("the second turn streams", || {
        model
            .conversation()
            .iter()
            .any(|request| conversation_mentions(request, "Second"))
    })
    .await;
    assert!(node.crash(cloud.root));
    assert_eq!(
        running.await.unwrap().end,
        HoldingEnd::Halted(Halt::Crashed)
    );
    cloud.pass(LEASE_TTL + TICK).await;
    // The claim expired: the turn is delivered again, and no sweep is needed.
    assert!(cloud.scheduler().tick(SystemTime::now()).is_empty());
    assert_eq!(cloud.task_state(cloud.root, second), TaskState::Queued);
    may_finish.store(true, Ordering::SeqCst);
    let report = cloud
        .node("surviving", NodeConfig::default())
        .hold(cloud.root)
        .await
        .unwrap();
    assert_eq!(report.end, HoldingEnd::Quiet, "{report:?}");
    completed(&cloud.lines(), request.input_id);
    let first = cloud.task(cloud.root, first);
    assert_eq!(
        (first.state, first.attempts, first.deliveries),
        (TaskState::Acknowledged, 1, 1)
    );
    let second = cloud.task(cloud.root, second);
    assert_eq!(
        (second.state, second.attempts, second.deliveries),
        (TaskState::Acknowledged, 2, 2)
    );
}

/// A family whose every load panics: after its holder crashed with every task acknowledged, the
/// sweep takes it over a bounded number of times, each takeover tried once, and then marks it as
/// needing attention. Another family held by the same node meanwhile finishes every turn. The
/// user's next message clears the mark, and the family goes on once it loads again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_family_whose_load_panics_is_held_a_bounded_number_of_times_until_the_user_acts() {
    let may_finish = flag();
    let model = ControlledModel::new({
        let may_finish = may_finish.clone();
        move |request| match task_of(request).as_deref() {
            Some("watch") => when(&may_finish, text("Watcher report.")),
            _ if mentions(request, "Delegate") && !has_tool_result(request, "watch") => {
                Reply::Now(call(
                    "watch",
                    "Subagent",
                    json!({"task": "watch", "name": "watcher", "run_in_background": true}),
                ))
            }
            _ => Reply::Now(text(DONE)),
        }
    });
    let cloud = Cloud::new(model.clone());
    let poisoned = cloud.root;
    let healthy = Uuid::new_v4();
    let node = cloud.node("node", NodeConfig::default());
    let (turn, _) = cloud.user_turn(poisoned, "Delegate the watch.");
    let running = spawn_holding(&node, poisoned);
    eventually("the turn is acknowledged", || {
        cloud.task_state(poisoned, turn) == TaskState::Acknowledged
    })
    .await;
    assert!(node.crash(poisoned));
    assert_eq!(
        running.await.unwrap().end,
        HoldingEnd::Halted(Halt::Crashed)
    );
    cloud.chats.poison(poisoned, true);
    let scheduler = cloud.scheduler();
    let mut healthy_turns = Vec::new();
    for round in 0..60 {
        cloud.pass(TICK).await;
        scheduler.tick(SystemTime::now());
        if round % 10 == 0 {
            healthy_turns.push(cloud.user_turn(healthy, "Hello.").0);
        }
        for root in [poisoned, healthy] {
            if !cloud
                .chats
                .tasks(root)
                .iter()
                .any(|task| task.state == TaskState::Queued)
            {
                continue;
            }
            match node.hold(root).await {
                Ok(report) if root == poisoned => {
                    assert_eq!(report.end, HoldingEnd::Halted(Halt::Crashed), "{report:?}");
                }
                Ok(report) => assert_eq!(report.end, HoldingEnd::Quiet, "{report:?}"),
                Err(LeaseError::Held { .. }) => {}
            }
        }
    }
    // The crashed holding, then one takeover for each further unclean holding below the limit.
    assert_eq!(
        cloud.chats.acquisitions(poisoned),
        UNCLEAN_HOLDINGS_LIMIT as usize
    );
    assert!(cloud.chats.needs_attention(poisoned));
    assert_eq!(
        cloud.chats.consecutive_unclean(poisoned),
        UNCLEAN_HOLDINGS_LIMIT
    );
    let takeovers = cloud.chats.tasks(poisoned);
    let takeovers = takeovers
        .iter()
        .filter(|task| task.key.starts_with("takeover:"))
        .collect::<Vec<_>>();
    assert_eq!(takeovers.len(), UNCLEAN_HOLDINGS_LIMIT as usize - 1);
    assert!(
        takeovers
            .iter()
            .all(|task| task.state == TaskState::DeadLettered
                && task.attempts == 1
                && task.max_attempts == 1)
    );
    for turn in healthy_turns {
        assert_eq!(cloud.task_state(healthy, turn), TaskState::Acknowledged);
    }
    assert!(!cloud.chats.needs_attention(healthy));

    cloud.chats.poison(poisoned, false);
    may_finish.store(true, Ordering::SeqCst);
    let (retry, request) = cloud.user_turn(poisoned, "Are you there?");
    assert!(!cloud.chats.needs_attention(poisoned));
    let report = node.hold(poisoned).await.unwrap();
    assert_eq!(report.end, HoldingEnd::Quiet, "{report:?}");
    assert_eq!(cloud.task_state(poisoned, retry), TaskState::Acknowledged);
    let lines = cloud.lines();
    completed(&lines, request.input_id);
    assert_eq!(messages_saying(&lines, "Watcher report."), 1);
    assert_eq!(cloud.chats.consecutive_unclean(poisoned), 0);
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

/// Many holdings of a delegating family, every other one handed over on SIGTERM and finished by
/// a fresh node: afterwards no Harness, publisher, heartbeat or Run is left behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_family_holdings_and_handovers_leave_no_task_behind() {
    const CYCLES: usize = 12;
    let model = ControlledModel::new(|request| {
        let turns = serde_json::to_string(&request.messages)
            .unwrap()
            .matches("Turn ")
            .count();
        let delegate = format!("delegate_{turns}");
        match task_of(request).as_deref() {
            Some(_) => Reply::Now(text("Child report.")),
            None if !has_tool_result(request, &delegate) => Reply::Now(call(
                &delegate,
                "Subagent",
                json!({"task": format!("child {turns}"), "name": format!("writer{turns}")}),
            )),
            None => Reply::Now(text(DONE)),
        }
    });
    let cloud = Cloud::new(model);
    let baseline = tokio::runtime::Handle::current()
        .metrics()
        .num_alive_tasks();
    for cycle in 0..CYCLES {
        let (task, request) = cloud.user_turn(cloud.root, &format!("Turn {cycle}."));
        if cycle % 2 == 1 {
            let node = cloud.node(&format!("terminated-{cycle}"), NodeConfig::default());
            node.sigterm();
            let report = node.hold(cloud.root).await.unwrap();
            assert!(
                matches!(report.end, HoldingEnd::HandedOver(_)),
                "{report:?}"
            );
        }
        let report = cloud
            .node(&format!("node-{cycle}"), NodeConfig::default())
            .hold(cloud.root)
            .await
            .unwrap();
        assert_eq!(report.end, HoldingEnd::Quiet, "{report:?}");
        assert_eq!(cloud.task_state(cloud.root, task), TaskState::Acknowledged);
        completed(&cloud.lines(), request.input_id);
        wait_for_alive_tasks(baseline, cycle).await;
    }
}
