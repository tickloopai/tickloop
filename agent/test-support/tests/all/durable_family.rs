//! Families through the reference host: one lease holds a root Chat and every member it
//! delegated. A foreground child answered hours later in another holding gives the parent what
//! one process would have; background children finish after the parent's turn within the
//! holding; a crash between a family effect and its result neither loses nor repeats the
//! effect; and a stopped subtree stays stopped.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tickloop_agent_model::ModelRequest;
use tickloop_agent_protocol::{
    AgentHostOptions, Content, ExecutionMode, PromptPolicy, QuestionAnswer, QuestionRequest, Role,
    SubagentNotificationDelivery,
};
use tickloop_agent_runtime::transcript::TranscriptStore;
use tickloop_agent_runtime::{
    Agent, Delivery, HarnessOptions, HarnessSeams, ParentSelection, PersonWait, PersonWaitKind,
    QuestionReply, ToolPolicy, TurnOp, TurnOutcome, TurnRequest, TurnWait, UserMessage,
    WaitHosting, Waits,
};
use tickloop_agent_test_support::event_stream::MemoryEventStream;
use tickloop_agent_test_support::harness::ChosenModels;
use tickloop_agent_test_support::routed::{RoutedModel, has_tool_result};
use tickloop_agent_test_support::stateless_host::{
    CrashPoint, DurableChats, Halt, HostConfig, ManualClock, ReferenceHost, TurnReport,
};
use tickloop_agent_test_support::transcripts::InMemoryTranscriptStore;
use tickloop_agent_test_support::{call, config, single_question, text};
use tickloop_agent_transcript::{Line, WaitKind, WaitState};
use uuid::Uuid;

use super::durable_waits::normalized;

const ASK_USER_QUESTION_TOOL_NAME: &str = "AskUserQuestion";

const LEASE_TTL: Duration = Duration::from_secs(30);

/// The task the requesting member was delegated, from the Agent message that opens its
/// conversation; `None` for the root.
fn task_of(request: &ModelRequest) -> Option<String> {
    let first = request
        .messages
        .iter()
        .find(|message| message.role != Role::System)?;
    let Content::Text { text } = first.content.first()? else {
        return None;
    };
    let body = text
        .strip_prefix("<agent-message>")?
        .split_once("</agent-message>")?
        .0;
    serde_json::from_str::<Value>(body).ok()?["message"]
        .as_str()
        .map(str::to_owned)
}

/// The text of the request's last message.
fn last_message(request: &ModelRequest) -> String {
    request
        .messages
        .last()
        .map(|message| format!("{:?}", message.content))
        .unwrap_or_default()
}

/// A cloud Harness whose Chats ask questions and delegate, with durable waits; the host lets
/// every delegation through without review.
fn family_harness(store: Arc<dyn TranscriptStore>, model: Arc<RoutedModel>) -> Agent {
    family_harness_with_tasks(store, model, None)
}

fn family_harness_with_tasks(
    store: Arc<dyn TranscriptStore>,
    model: Arc<RoutedModel>,
    task_tools: Option<bool>,
) -> Agent {
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
                execution_mode_override: Some(ExecutionMode::FullAccess),
                ..AgentHostOptions::product_defaults()
            },
            tools: ToolPolicy {
                tasks: task_tools.is_some(),
                questions: true,
                delegation: true,
                family_messages: true,
                ..ToolPolicy::web_fetch_only()
            },
            host_rules: Vec::new(),
            task_tools,
            min_schedule_interval: tickloop_agent_runtime::LOCAL_MIN_SCHEDULE_INTERVAL,
            waits: WaitHosting::Durable,
        },
    )
    .unwrap()
}

/// The host side of one family: its durable rows, its stream, its clock and a host per name.
struct Cloud {
    clock: Arc<ManualClock>,
    chats: Arc<DurableChats>,
    stream: Arc<MemoryEventStream>,
    model: Arc<RoutedModel>,
    chat: Uuid,
    task_tools: Option<bool>,
}

impl Cloud {
    fn new(model: Arc<RoutedModel>) -> Self {
        let clock = ManualClock::new();
        Self {
            chats: DurableChats::new(clock.clone()),
            clock,
            stream: Arc::new(MemoryEventStream::new()),
            model,
            chat: Uuid::new_v4(),
            task_tools: None,
        }
    }

    fn host(&self, holder: &str) -> ReferenceHost {
        let model = self.model.clone();
        let task_tools = self.task_tools;
        ReferenceHost::new(
            holder,
            self.chats.clone(),
            self.stream.clone(),
            self.clock.clone(),
            HostConfig::default(),
            move |store| family_harness_with_tasks(store, model.clone(), task_tools),
        )
    }

    fn root_lines(&self) -> Vec<Line> {
        self.chats.history(self.chat).lines
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

fn answer(request: &QuestionRequest) -> TurnRequest {
    TurnRequest {
        input_id: Uuid::new_v4(),
        op: TurnOp::Answer {
            request: request.id,
            reply: QuestionReply::Answered(vec![QuestionAnswer::other("Weekly")]),
        },
        delivery: Delivery::Queue,
        host_context: None,
    }
}

fn acknowledged(report: &TurnReport) {
    assert!(report.acknowledged, "{report:?}");
}

/// The tool results of the call `call_id` the Lines hold, as JSON.
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

/// How many user messages of the Lines carry `text`.
fn messages_saying(lines: &[Line], text: &str) -> usize {
    lines
        .iter()
        .filter(|line| matches!(line, Line::User(entry) if format!("{:?}", entry.message.content).contains(text)))
        .count()
}

fn last_completions(lines: &[Line]) -> Vec<tickloop_agent_protocol::FamilyCompletion> {
    lines
        .iter()
        .rev()
        .find_map(|line| match line {
            Line::FamilyCompletions(entry) => Some(entry.completions.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// The model requests as text with ids replaced by their order of appearance and the times
/// they carry zeroed: two families making the same requests differ in nothing else.
fn requests(model: &RoutedModel) -> Vec<String> {
    model
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|request| {
            let text = normalized(&serde_json::to_string(&request.messages).unwrap());
            zeroed(&text, "\\\"created_at\\\":")
        })
        .collect()
}

/// `text` with the number after each `field` replaced by 0.
fn zeroed(text: &str, field: &str) -> String {
    let mut rest = text;
    let mut zeroed = String::new();
    while let Some(position) = rest.find(field) {
        let value_start = position + field.len();
        zeroed.push_str(&rest[..value_start]);
        zeroed.push('0');
        rest = rest[value_start..].trim_start_matches(|character: char| character.is_ascii_digit());
    }
    zeroed.push_str(rest);
    zeroed
}

/// A root that delegates in the foreground to a child that asks the user first.
fn foreground_family() -> Arc<RoutedModel> {
    Arc::new(RoutedModel::new(|request| {
        match task_of(request).as_deref() {
            Some("child") if !has_tool_result(request, "question") => call(
                "question",
                ASK_USER_QUESTION_TOOL_NAME,
                single_question("Which format?"),
            ),
            Some("child") => text("Weekly format chosen."),
            _ if !has_tool_result(request, "delegate") => call(
                "delegate",
                "Subagent",
                json!({"task": "child", "name": "writer"}),
            ),
            _ => text("Report ready."),
        }
    }))
}

/// The foreground family asked and answered within one Harness; gives its model requests.
async fn foreground_in_process() -> Vec<String> {
    let model = foreground_family();
    let agent = family_harness(Arc::new(InMemoryTranscriptStore::new()), model.clone());
    let root = agent.create_chat_without_folder().await.unwrap();
    let turn = agent
        .advance(root, prompt("Prepare the report."))
        .await
        .unwrap();
    let TurnOutcome::Waiting(TurnWait::Member { chat: child, wait }) =
        turn.outcome().await.unwrap()
    else {
        panic!("the root waits for its child");
    };
    let TurnWait::Question(request) = *wait else {
        panic!("the child waits for its question");
    };
    let reply = agent.advance(child, answer(&request)).await.unwrap();
    assert!(matches!(
        reply.outcome().await.unwrap(),
        TurnOutcome::Merged { .. }
    ));
    agent.settled_input(root, turn.input_id()).await.unwrap();
    agent.shutdown().await;
    requests(&model)
}

#[tokio::test]
async fn a_foreground_child_answered_hours_later_in_another_holding_goes_on_as_in_one_process() {
    let in_process = foreground_in_process().await;
    let cloud = Cloud::new(foreground_family());
    let asked = cloud
        .host("a")
        .deliver(cloud.chat, prompt("Prepare the report."))
        .await
        .unwrap();
    acknowledged(&asked);
    let Some(TurnOutcome::Waiting(TurnWait::Member { chat: child, wait })) = asked.outcome else {
        panic!("the root waits for its child: {asked:?}");
    };
    let TurnWait::Question(request) = *wait else {
        panic!("the child waits for its question");
    };
    assert_eq!(
        asked.waiting,
        Some(Waits {
            people: vec![PersonWait {
                chat: child,
                request: request.id,
                kind: PersonWaitKind::Question,
            }],
            wake_at: None,
            deliveries: Vec::new(),
        })
    );
    assert_eq!(cloud.chats.waiting(cloud.chat), asked.waiting);
    assert!(cloud.chats.members(cloud.chat).contains(&child));

    cloud.clock.advance(Duration::from_secs(3 * 60 * 60));
    let replied = cloud
        .host("b")
        .deliver_to(cloud.chat, child, answer(&request))
        .await
        .unwrap();
    acknowledged(&replied);
    assert!(matches!(replied.outcome, Some(TurnOutcome::Merged { .. })));
    assert_eq!(replied.waiting, None, "the whole family went on");
    assert_eq!(requests(&cloud.model), in_process);
    let lines = cloud.root_lines();
    let [result] = results_of(&lines, "delegate").try_into().unwrap();
    assert_eq!(result["status"], "completed");
    assert_eq!(result["result"], "Weekly format chosen.");
    let [completion] = last_completions(&lines).try_into().unwrap();
    assert!(matches!(
        completion.notification,
        SubagentNotificationDelivery::Foreground
    ));
    assert_eq!(messages_saying(&lines, "Weekly format chosen."), 1);
}

/// A root that delegates two children in the background and notes each report.
fn background_family() -> Arc<RoutedModel> {
    Arc::new(RoutedModel::new(|request| {
        match task_of(request).as_deref() {
            Some(task) => text(&format!("{task} done")),
            None if !has_tool_result(request, "first") => [
                call(
                    "first",
                    "Subagent",
                    json!({"task": "fetch", "name": "fetcher", "run_in_background": true}),
                ),
                call(
                    "second",
                    "Subagent",
                    json!({"task": "check", "name": "checker", "run_in_background": true}),
                ),
            ]
            .concat(),
            None if last_message(request).contains("ToolResult") => text("Delegated."),
            None => text("Noted."),
        }
    }))
}

#[tokio::test]
async fn background_children_finishing_after_the_parents_turn_report_once_within_the_holding() {
    let cloud = Cloud::new(background_family());
    let delegated = cloud
        .host("a")
        .deliver(cloud.chat, prompt("Gather the data."))
        .await
        .unwrap();
    acknowledged(&delegated);
    assert!(matches!(
        delegated.outcome,
        Some(TurnOutcome::Completed { .. })
    ));
    // The holding lasted until the children reported and the root read their reports.
    assert_eq!(delegated.waiting, None);
    let lines = cloud.root_lines();
    let completions = last_completions(&lines);
    assert_eq!(completions.len(), 2);
    assert!(completions.iter().all(|completion| matches!(
        completion.notification,
        SubagentNotificationDelivery::Queued
    )));
    assert_eq!(messages_saying(&lines, "fetch done"), 1);
    assert_eq!(messages_saying(&lines, "check done"), 1);

    let later = cloud
        .host("b")
        .deliver(cloud.chat, prompt("Anything else?"))
        .await
        .unwrap();
    acknowledged(&later);
    let lines = cloud.root_lines();
    assert_eq!(last_completions(&lines).len(), 2);
    assert_eq!(messages_saying(&lines, "fetch done"), 1);
    assert_eq!(messages_saying(&lines, "check done"), 1);
}

/// A root that delegates one child in the background, then answers.
fn launching_family() -> Arc<RoutedModel> {
    Arc::new(RoutedModel::new(|request| {
        match task_of(request).as_deref() {
            Some(_) => text("watching"),
            None if !has_tool_result(request, "delegate") => call(
                "delegate",
                "Subagent",
                json!({"task": "watch", "name": "watcher", "run_in_background": true}),
            ),
            None if last_message(request).contains("ping round") => call(
                "send",
                "SendMessage",
                json!({"to": "watcher", "message": "ping"}),
            ),
            None => text("Done."),
        }
    }))
}

fn puts_the_launch_result_on_the_chain(lines: &[Line]) -> bool {
    !results_of(lines, "delegate").is_empty()
}

fn puts_the_send_result_on_the_chain(lines: &[Line]) -> bool {
    !results_of(lines, "send").is_empty()
}

/// The holder dies once its child is durable and before its call's result commits: the next
/// holder finds the child the records name, so the child is neither lost nor launched again.
#[tokio::test]
async fn a_crash_after_a_child_launch_before_its_result_neither_loses_nor_repeats_the_child() {
    let cloud = Cloud::new(launching_family());
    let task = prompt("Watch the feed.");
    let holding = cloud.host("crashing").acquire(cloud.chat).unwrap();
    holding.crash_at(CrashPoint::BeforeCommit(
        puts_the_launch_result_on_the_chain,
    ));
    let crashed = holding.run(task.clone()).await;
    assert_eq!(crashed.halt, Some(Halt::Crashed), "{crashed:?}");
    let [child] = cloud
        .chats
        .members(cloud.chat)
        .into_iter()
        .collect::<Vec<_>>()[..]
    else {
        panic!("the child is durable before the crash");
    };
    assert!(results_of(&cloud.root_lines(), "delegate").is_empty());

    cloud.clock.advance(LEASE_TTL + Duration::from_secs(1));
    let redelivered = cloud
        .host("surviving")
        .deliver(cloud.chat, task)
        .await
        .unwrap();
    acknowledged(&redelivered);
    assert!(matches!(
        redelivered.outcome,
        Some(TurnOutcome::Completed { .. })
    ));
    assert_eq!(redelivered.waiting, None);
    assert_eq!(
        cloud
            .chats
            .members(cloud.chat)
            .into_iter()
            .collect::<Vec<_>>(),
        vec![child],
        "no second child"
    );
    let lines = cloud.root_lines();
    let [launched] = results_of(&lines, "delegate").try_into().unwrap();
    assert_eq!(launched["status"], "async_launched");
    assert_eq!(launched["child_chat_id"], child.to_string());
    let child_tasks = cloud
        .chats
        .member_lines(child)
        .iter()
        .filter(|line| matches!(line, Line::Input(entry) if entry.op == tickloop_agent_transcript::InputOp::AgentMessage))
        .map(|line| match line {
            Line::Input(entry) => entry.input_id,
            _ => unreachable!(),
        })
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(child_tasks.len(), 1, "the child got its task once");
    assert_eq!(messages_saying(&lines, "watching"), 1);
}

/// The holder dies once the receiver holds the message and before the sender's result commits:
/// the next holder neither loses the message nor sends it again.
#[tokio::test]
async fn a_family_message_is_delivered_once_across_a_crash_before_its_result() {
    let cloud = Cloud::new(launching_family());
    let launched = cloud
        .host("a")
        .deliver(cloud.chat, prompt("Watch the feed."))
        .await
        .unwrap();
    acknowledged(&launched);
    let [child] = cloud
        .chats
        .members(cloud.chat)
        .into_iter()
        .collect::<Vec<_>>()[..]
    else {
        panic!("one child");
    };
    let task = prompt("ping round 1");
    let holding = cloud.host("crashing").acquire(cloud.chat).unwrap();
    holding.crash_at(CrashPoint::BeforeCommit(puts_the_send_result_on_the_chain));
    let crashed = holding.run(task.clone()).await;
    assert_eq!(crashed.halt, Some(Halt::Crashed), "{crashed:?}");
    assert!(results_of(&cloud.root_lines(), "send").is_empty());

    cloud.clock.advance(LEASE_TTL + Duration::from_secs(1));
    let redelivered = cloud
        .host("surviving")
        .deliver(cloud.chat, task)
        .await
        .unwrap();
    acknowledged(&redelivered);
    assert_eq!(redelivered.waiting, None);
    let lines = cloud.root_lines();
    let [sent] = results_of(&lines, "send").try_into().unwrap();
    assert_eq!(sent["status"], "accepted");
    let child_lines = cloud.chats.member_lines(child);
    assert_eq!(
        messages_saying(&child_lines, "\\\"message\\\":\\\"ping\\\""),
        1,
        "the message reached the child once"
    );
    // The child answered it once: its report reached the root once.
    assert_eq!(last_completions(&lines).len(), 2);
}

/// A root with a background member that delegates a leaf and waits for it; the leaf asks the
/// user. Told to stop, the root stops the member's subtree.
fn stoppable_family() -> Arc<RoutedModel> {
    Arc::new(RoutedModel::new(|request| {
        match task_of(request).as_deref() {
            Some("member") if !has_tool_result(request, "leaf") => call(
                "leaf",
                "Subagent",
                json!({"task": "leaf", "name": "leaf", "run_in_background": true}),
            ),
            Some("member") => text("Waiting for the leaf."),
            Some("leaf") if !has_tool_result(request, "question") => call(
                "question",
                ASK_USER_QUESTION_TOOL_NAME,
                single_question("Which source?"),
            ),
            Some(_) => text("leaf report"),
            None if !has_tool_result(request, "delegate") => call(
                "delegate",
                "Subagent",
                json!({"task": "member", "name": "member", "run_in_background": true}),
            ),
            None if last_message(request).contains("Stop the member")
                && !has_tool_result(request, "stop") =>
            {
                call("stop", "StopSubagent", json!({"to": "member"}))
            }
            None => text("Noted."),
        }
    }))
}

#[tokio::test]
async fn a_stopped_subtree_stays_stopped_in_later_holdings() {
    let cloud = Cloud::new(stoppable_family());
    let started = cloud
        .host("a")
        .deliver(cloud.chat, prompt("Research the topic."))
        .await
        .unwrap();
    acknowledged(&started);
    // The leaf waits for the user, and the member for the leaf.
    let waits = started.waiting.clone().expect("the family waits");
    let [person] = waits.people.as_slice() else {
        panic!("{waits:?}");
    };
    let leaf = person.chat;
    assert_eq!(person.kind, PersonWaitKind::Question);
    let members = cloud.chats.members(cloud.chat);
    assert_eq!(members.len(), 2);
    let member = *members.iter().find(|id| **id != leaf).unwrap();
    assert!(cloud.chats.member_lines(member).iter().any(|line| matches!(
        line,
        Line::Wait(entry) if matches!(entry.kind, WaitKind::Children { .. })
            && matches!(entry.state, WaitState::Open)
    )));

    let stopped = cloud
        .host("b")
        .deliver(cloud.chat, prompt("Stop the member."))
        .await
        .unwrap();
    acknowledged(&stopped);
    assert_eq!(
        stopped.waiting, None,
        "nothing of the subtree waits any more"
    );
    let [result] = results_of(&cloud.root_lines(), "stop").try_into().unwrap();
    assert_eq!(result["status"], "stopped");
    let leaf_requests = cloud.model.requests.lock().unwrap().len();

    let later = cloud
        .host("a")
        .deliver(cloud.chat, prompt("Anything else?"))
        .await
        .unwrap();
    acknowledged(&later);
    assert_eq!(later.waiting, None);
    // Only the root answered: the stopped subtree did not come back.
    assert_eq!(
        cloud.model.requests.lock().unwrap().len(),
        leaf_requests + 1
    );
    assert!(cloud.chats.member_lines(leaf).iter().any(|line| matches!(
        line,
        Line::Wait(entry) if matches!(entry.kind, WaitKind::Question { .. })
            && matches!(entry.state, WaitState::Abandoned { .. })
    )));
}

fn makes_the_child_durable(lines: &[Line]) -> bool {
    lines.iter().any(|line| matches!(line, Line::Delegation(_)))
}

/// The holder dies before its child is durable: the next holder's load does not fail on a
/// child its parent may have registered, and the parent learns the launch's outcome is unknown
/// instead of launching again unawares.
#[tokio::test]
async fn a_crash_before_a_child_is_durable_leaves_the_launch_unknown_without_failing_the_load() {
    let cloud = Cloud::new(launching_family());
    let task = prompt("Watch the feed.");
    let holding = cloud.host("crashing").acquire(cloud.chat).unwrap();
    holding.crash_at(CrashPoint::BeforeCommit(makes_the_child_durable));
    let crashed = holding.run(task.clone()).await;
    assert_eq!(crashed.halt, Some(Halt::Crashed), "{crashed:?}");
    assert!(cloud.chats.members(cloud.chat).is_empty());

    cloud.clock.advance(LEASE_TTL + Duration::from_secs(1));
    let redelivered = cloud
        .host("surviving")
        .deliver(cloud.chat, task)
        .await
        .unwrap();
    acknowledged(&redelivered);
    assert_eq!(redelivered.waiting, None);
    let lines = cloud.root_lines();
    let results = results_of(&lines, "delegate");
    assert!(results.len() <= 1, "{results:?}");
    assert!(
        cloud.chats.members(cloud.chat).len() <= 1,
        "at most the one launch the parent decided on again"
    );
}

fn accepts_the_message(lines: &[Line]) -> bool {
    lines.iter().any(|line| {
        matches!(line, Line::Input(entry) if entry.op == tickloop_agent_transcript::InputOp::AgentMessage
            && entry.state == tickloop_agent_transcript::InputState::Accepted
            && format!("{:?}", entry.queued).contains("ping"))
    })
}

/// The holder dies before the receiver holds the message: the message is not delivered, and
/// the sender learns its outcome is unknown rather than that it was sent.
#[tokio::test]
async fn a_crash_before_the_receiver_holds_a_message_leaves_its_send_unknown() {
    let cloud = Cloud::new(launching_family());
    let launched = cloud
        .host("a")
        .deliver(cloud.chat, prompt("Watch the feed."))
        .await
        .unwrap();
    acknowledged(&launched);
    let [child] = cloud
        .chats
        .members(cloud.chat)
        .into_iter()
        .collect::<Vec<_>>()[..]
    else {
        panic!("one child");
    };
    let task = prompt("ping round 1");
    let holding = cloud.host("crashing").acquire(cloud.chat).unwrap();
    holding.crash_at(CrashPoint::BeforeCommit(accepts_the_message));
    let crashed = holding.run(task.clone()).await;
    assert_eq!(crashed.halt, Some(Halt::Crashed), "{crashed:?}");

    cloud.clock.advance(LEASE_TTL + Duration::from_secs(1));
    let redelivered = cloud
        .host("surviving")
        .deliver(cloud.chat, task)
        .await
        .unwrap();
    acknowledged(&redelivered);
    assert_eq!(redelivered.waiting, None);
    let child_lines = cloud.chats.member_lines(child);
    assert_eq!(
        messages_saying(&child_lines, "\\\"message\\\":\\\"ping\\\""),
        0,
        "the message the receiver never held is not delivered"
    );
    let results = results_of(&cloud.root_lines(), "send");
    assert!(
        results.iter().all(|result| result["status"] != "accepted"),
        "{results:?}"
    );
}

fn claiming_task() -> Arc<RoutedModel> {
    use tickloop_agent_test_support::routed::tool_result;
    Arc::new(RoutedModel::new(|request| {
        if !has_tool_result(request, "create") {
            return call(
                "create",
                "TaskCreate",
                json!({"mutation_id":Uuid::from_u128(1),
                "title":"Report", "description":"Prepare the authorized report"}),
            );
        }
        let task = &tool_result(&request.messages, "create").unwrap()["task"];
        if !has_tool_result(request, "claim") {
            return call(
                "claim",
                "TaskUpdate",
                json!({"mutation_id":Uuid::from_u128(2),
                "task_id":task["id"], "expected_revision":1, "action":"claim"}),
            );
        }
        if !has_tool_result(request, "question") {
            return call(
                "question",
                "AskUserQuestion",
                single_question("Which format?"),
            );
        }
        if !has_tool_result(request, "complete") {
            return call(
                "complete",
                "TaskUpdate",
                json!({"mutation_id":Uuid::from_u128(3),
                "task_id":task["id"], "expected_revision":2, "action":"complete", "result":"Report prepared."}),
            );
        }
        text("Report prepared.")
    }))
}

fn task_boards(
    lines: &[Line],
) -> std::collections::BTreeMap<Uuid, tickloop_agent_protocol::TaskBoard> {
    lines
        .iter()
        .rev()
        .find_map(|line| match line {
            Line::TaskBoards(entry) => Some(entry.boards.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn a_claimed_task_waits_and_finishes_on_another_host_like_one_process() {
    let model = claiming_task();
    let agent = family_harness_with_tasks(
        Arc::new(InMemoryTranscriptStore::new()),
        model.clone(),
        Some(true),
    );
    let root = agent.create_chat_without_folder().await.unwrap();
    let turn = agent
        .advance(root, prompt("Prepare the report."))
        .await
        .unwrap();
    let TurnOutcome::Waiting(TurnWait::Question(question)) = turn.outcome().await.unwrap() else {
        panic!("the Task owner waits");
    };
    agent
        .advance(root, answer(&question))
        .await
        .unwrap()
        .outcome()
        .await
        .unwrap();
    agent.settled_input(root, turn.input_id()).await.unwrap();
    agent.shutdown().await;
    let in_process = requests(&model);

    let mut cloud = Cloud::new(claiming_task());
    cloud.task_tools = Some(true);
    let asked = cloud
        .host("a")
        .deliver(cloud.chat, prompt("Prepare the report."))
        .await
        .unwrap();
    acknowledged(&asked);
    let Some(TurnOutcome::Waiting(TurnWait::Question(question))) = asked.outcome else {
        panic!("the Task owner waits: {asked:?}");
    };
    let before = task_boards(&cloud.root_lines());
    let board = before.values().next().unwrap();
    assert_eq!(
        board.tasks[0].status,
        tickloop_agent_protocol::SharedTaskStatus::InProgress
    );
    assert_eq!(board.mutations.len(), 2);
    cloud.clock.advance(Duration::from_secs(3 * 60 * 60));
    let replied = cloud
        .host("b")
        .deliver(cloud.chat, answer(&question))
        .await
        .unwrap();
    acknowledged(&replied);
    assert_eq!(replied.waiting, None);
    let after = task_boards(&cloud.root_lines());
    let completed = &after.values().next().unwrap().tasks[0];
    assert_eq!(completed.id, board.tasks[0].id);
    assert_eq!(
        completed.status,
        tickloop_agent_protocol::SharedTaskStatus::Completed
    );
    assert_eq!(after.values().next().unwrap().mutations.len(), 3);
    assert_eq!(requests(&cloud.model), in_process);
}

fn puts_the_create_result_on_the_chain(lines: &[Line]) -> bool {
    !results_of(lines, "create").is_empty()
}

fn puts_the_claim_result_on_the_chain(lines: &[Line]) -> bool {
    !results_of(lines, "claim").is_empty()
}

#[tokio::test]
async fn task_mutation_receipts_reconcile_crashes_before_create_or_claim_results() {
    for point in [
        puts_the_create_result_on_the_chain as fn(&[Line]) -> bool,
        puts_the_claim_result_on_the_chain,
    ] {
        let mut cloud = Cloud::new(claiming_task());
        cloud.task_tools = Some(true);
        let task = prompt("Prepare the report.");
        let holding = cloud.host("crashing").acquire(cloud.chat).unwrap();
        holding.crash_at(CrashPoint::BeforeCommit(point));
        let crashed = holding.run(task.clone()).await;
        assert_eq!(crashed.halt, Some(Halt::Crashed), "{crashed:?}");
        let before = task_boards(&cloud.root_lines());
        let board = before.values().next().unwrap();
        assert_eq!(board.tasks.len(), 1);
        let id = board.tasks[0].id;
        cloud.clock.advance(LEASE_TTL + Duration::from_secs(1));
        let resumed = cloud
            .host("surviving")
            .deliver(cloud.chat, task)
            .await
            .unwrap();
        acknowledged(&resumed);
        let Some(TurnOutcome::Waiting(TurnWait::Question(question))) = resumed.outcome else {
            panic!("the original Task owner continued: {resumed:?}");
        };
        let boards = task_boards(&cloud.root_lines());
        let board = boards.values().next().unwrap();
        assert_eq!(board.tasks.len(), 1);
        assert_eq!(board.tasks[0].id, id);
        assert_eq!(
            board.tasks[0].status,
            tickloop_agent_protocol::SharedTaskStatus::InProgress
        );
        assert_eq!(board.mutations.len(), 2);
        let lines = cloud.root_lines();
        assert_eq!(results_of(&lines, "create").len(), 1);
        assert_eq!(results_of(&lines, "claim").len(), 1);
        assert!(
            results_of(&lines, "create")
                .iter()
                .chain(results_of(&lines, "claim").iter())
                .any(|result| result["status"] == "already_applied")
        );
        let replied = cloud
            .host("later")
            .deliver(cloud.chat, answer(&question))
            .await
            .unwrap();
        acknowledged(&replied);
        assert_eq!(
            task_boards(&cloud.root_lines())
                .values()
                .next()
                .unwrap()
                .tasks[0]
                .status,
            tickloop_agent_protocol::SharedTaskStatus::Completed
        );
    }
}

fn assigning_task() -> Arc<RoutedModel> {
    use tickloop_agent_test_support::routed::{history_text, tool_result};
    Arc::new(RoutedModel::new(|request| {
        if task_of(request).is_some() {
            return text("Ready.");
        }
        if !has_tool_result(request, "create") {
            return call(
                "create",
                "TaskCreate",
                json!({"mutation_id":Uuid::from_u128(11),
                "title":"Report", "description":"Prepare the authorized report"}),
            );
        }
        if !has_tool_result(request, "delegate") {
            return call(
                "delegate",
                "Subagent",
                json!({"task":"stand by", "name":"writer", "run_in_background":true}),
            );
        }
        let retry = history_text(request).contains("Assign the report again");
        let call_id = if retry { "retry_assign" } else { "assign" };
        if history_text(request).contains("Assign the report") && !has_tool_result(request, call_id)
        {
            let task = &tool_result(&request.messages, "create").unwrap()["task"];
            return call(
                call_id,
                "TaskUpdate",
                json!({"mutation_id":Uuid::from_u128(12),
                "task_id":task["id"], "expected_revision":1, "action":"assign", "owner":"writer"}),
            );
        }
        text("Noted.")
    }))
}

fn accepts_the_assignment(lines: &[Line]) -> bool {
    lines.iter().any(|line| {
        matches!(line, Line::Input(entry)
        if entry.input_id == Uuid::from_u128(12))
    })
}

#[tokio::test]
async fn task_assignment_is_delivered_once_when_the_receiver_commits_before_the_sender_result() {
    let mut cloud = Cloud::new(assigning_task());
    cloud.task_tools = Some(true);
    acknowledged(
        &cloud
            .host("a")
            .deliver(cloud.chat, prompt("Prepare the report."))
            .await
            .unwrap(),
    );
    let [child] = cloud
        .chats
        .members(cloud.chat)
        .into_iter()
        .collect::<Vec<_>>()[..]
    else {
        panic!("one member");
    };
    let loop_id = *task_boards(&cloud.root_lines()).keys().next().unwrap();
    let mut task = prompt("Assign the report");
    if let TurnOp::Continue { target_loop, .. } = &mut task.op {
        *target_loop = Some(loop_id);
    }
    let holding = cloud.host("crashing").acquire(cloud.chat).unwrap();
    holding.crash_at(CrashPoint::AfterCommit(accepts_the_assignment));
    let crashed = holding.run(task.clone()).await;
    assert_eq!(crashed.halt, Some(Halt::Crashed), "{crashed:?}");
    cloud.clock.advance(LEASE_TTL + Duration::from_secs(1));
    acknowledged(&cloud.host("b").deliver(cloud.chat, task).await.unwrap());
    let mut retry = prompt("Assign the report again");
    if let TurnOp::Continue { target_loop, .. } = &mut retry.op {
        *target_loop = Some(loop_id);
    }
    acknowledged(&cloud.host("c").deliver(cloud.chat, retry).await.unwrap());
    let inputs = cloud
        .chats
        .member_lines(child)
        .iter()
        .filter_map(|line| match line {
            Line::Input(entry) if entry.input_id == Uuid::from_u128(12) => Some(entry.run_id),
            _ => None,
        })
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(inputs.len(), 1);
    assert_eq!(
        messages_saying(&cloud.chats.member_lines(child), "Shared task assignment:"),
        1
    );
    let boards = task_boards(&cloud.root_lines());
    assert_eq!(boards.values().next().unwrap().mutations.len(), 2);
}

fn member_claiming_task() -> Arc<RoutedModel> {
    use tickloop_agent_test_support::routed::tool_result;
    Arc::new(RoutedModel::new(|request| {
        if task_of(request).is_some() {
            if !has_tool_result(request, "list") {
                return call("list", "TaskList", json!({}));
            }
            let task = &tool_result(&request.messages, "list").unwrap()["board"]["tasks"][0];
            if !has_tool_result(request, "claim") {
                return call(
                    "claim",
                    "TaskUpdate",
                    json!({"mutation_id":Uuid::from_u128(22),
                    "task_id":task["id"], "expected_revision":1, "action":"claim"}),
                );
            }
            if !has_tool_result(request, "question") {
                return call(
                    "question",
                    "AskUserQuestion",
                    single_question("Which format?"),
                );
            }
            if !has_tool_result(request, "complete") {
                return call(
                    "complete",
                    "TaskUpdate",
                    json!({"mutation_id":Uuid::from_u128(23),
                    "task_id":task["id"], "expected_revision":2, "action":"complete", "result":"Report prepared."}),
                );
            }
            return text("Report prepared.");
        }
        if !has_tool_result(request, "create") {
            return call(
                "create",
                "TaskCreate",
                json!({"mutation_id":Uuid::from_u128(21),
                "title":"Report", "description":"Prepare the authorized report"}),
            );
        }
        if !has_tool_result(request, "delegate") {
            return call(
                "delegate",
                "Subagent",
                json!({"task":"prepare the report", "name":"writer"}),
            );
        }
        text("Report delivered.")
    }))
}

#[tokio::test]
async fn a_members_claim_in_the_root_board_survives_family_reload() {
    let mut cloud = Cloud::new(member_claiming_task());
    cloud.task_tools = Some(true);
    let asked = cloud
        .host("a")
        .deliver(cloud.chat, prompt("Prepare the report."))
        .await
        .unwrap();
    acknowledged(&asked);
    let Some(TurnOutcome::Waiting(TurnWait::Member { chat: child, wait })) = asked.outcome else {
        panic!("the family waits for the Task owner: {asked:?}");
    };
    let TurnWait::Question(question) = *wait else {
        panic!("the member waits for a person");
    };
    let before = task_boards(&cloud.root_lines());
    let task = &before.values().next().unwrap().tasks[0];
    assert_eq!(task.owner.as_ref().unwrap().chat_id, child);
    assert_eq!(
        task.status,
        tickloop_agent_protocol::SharedTaskStatus::InProgress
    );
    cloud.clock.advance(Duration::from_secs(3 * 60 * 60));
    let replied = cloud
        .host("b")
        .deliver_to(cloud.chat, child, answer(&question))
        .await
        .unwrap();
    acknowledged(&replied);
    assert_eq!(replied.waiting, None);
    let boards = task_boards(&cloud.root_lines());
    let board = boards.values().next().unwrap();
    assert_eq!(board.tasks[0].id, task.id);
    assert_eq!(
        board.tasks[0].status,
        tickloop_agent_protocol::SharedTaskStatus::Completed
    );
    assert_eq!(board.mutations.len(), 3);
    assert_eq!(
        results_of(&cloud.chats.member_lines(child), "complete")[0]["status"],
        "applied"
    );
    assert!(cloud.model.requests.lock().unwrap().iter().all(|request| {
        request
            .tools
            .iter()
            .filter(|tool| tool.name.starts_with("Task"))
            .count()
            == 4
    }));
}
