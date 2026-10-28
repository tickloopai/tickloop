//! Send now (ctrl+enter) while the Agent works, following Claude Code's low-latency submit: the
//! turn's foreground Bash commands, and on a non-interactive host the Subagent calls waiting for
//! a report, move to the background without being interrupted, so the turn takes the message at its tool boundary, and each completes later as
//! a task notification. Work that cannot move is interrupted, as before.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tickloop_code_local_environment::AgentLocalExt;
use tickloop_harness::model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_harness::protocol::{
    ChatId, CommandState, Content, Event, EventKind, LoopState, ModelIdentity, Receipt, Role,
    ToolDefinition, ToolResult,
};
use tickloop_harness::runtime::{
    Agent, SubagentLaunch,
    host_tools::{HostToolPermission, HostToolProvider},
};
use tickloop_harness::test_support::routed::{RoutedModel, has_tool_result, state};
use tickloop_harness::test_support::subagents::{task_notifications, wait_for_snapshot};
use tickloop_harness::test_support::{call, config, is_host_note, next_event, text};
use tokio::sync::{broadcast, mpsc::UnboundedSender};
use tokio_util::sync::CancellationToken;

const WAIT: Duration = Duration::from_secs(10);
const QUEUED: &str = "How is it going?";
const PARENT_REPLY: &str = "Parent answered";
const CHILD_REPORT: &str = "CHILD-FINAL-REPORT";
const MOVED_SUBAGENT: &str = "Moved to the background so that a message that arrived while it was running can reach you; it was not interrupted and keeps running. Its completion arrives as a task notification; you know nothing of its results until then, so do not report or predict them.";
const SLOW_LOOKUP: &str = "SlowLookup";

fn is_child(request: &ModelRequest) -> bool {
    !state(request)["delegation"].is_null()
}

/// Answers the root Chat with `root` and every delegated child with its report, which a child
/// gives only once `release` is cancelled.
struct HeldChildren {
    routed: RoutedModel,
    children: AtomicUsize,
    release: CancellationToken,
}

impl HeldChildren {
    fn new(root: impl Fn(&ModelRequest) -> Vec<Content> + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            routed: RoutedModel::new(move |request| {
                if is_child(request) {
                    text(CHILD_REPORT)
                } else {
                    root(request)
                }
            }),
            children: AtomicUsize::new(0),
            release: CancellationToken::new(),
        })
    }

    fn root_requests(&self) -> Vec<ModelRequest> {
        self.routed
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| !is_child(request))
            .cloned()
            .collect()
    }
}

#[async_trait]
impl ModelClient for HeldChildren {
    fn identity(&self) -> ModelIdentity {
        self.routed.identity()
    }

    async fn generate_title(
        &self,
        request: ModelRequest,
        _: UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        Ok(tickloop_harness::test_support::title_output(request))
    }

    async fn complete(
        &self,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        if is_child(&request) {
            self.children.fetch_add(1, Ordering::SeqCst);
            tokio::select! {
                () = self.release.cancelled() => {}
                () = cancel.cancelled() => return Err(ModelError::Cancelled),
            }
        }
        self.routed.complete(request, events, cancel).await
    }
}

/// A host tool that cannot move to the background: it answers once `release` is cancelled, and
/// counts the calls that were cancelled instead.
#[derive(Default)]
struct SlowLookup {
    release: CancellationToken,
    cancelled: AtomicUsize,
}

#[async_trait]
impl HostToolProvider for SlowLookup {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: SLOW_LOOKUP.into(),
            description: "Looks something up slowly.".into(),
            input_schema: json!({"type": "object", "properties": {}}),
        }]
    }

    fn permission(&self, _: ChatId, _: &str, _: &Value) -> HostToolPermission {
        HostToolPermission::Automatic
    }

    async fn execute(&self, _: ChatId, _: &str, _: Value, cancel: CancellationToken) -> ToolResult {
        tokio::select! {
            () = self.release.cancelled() => ToolResult {
                content: "looked up".into(),
                is_error: false,
                output_ref: None,
            },
            () = cancel.cancelled() => {
                self.cancelled.fetch_add(1, Ordering::SeqCst);
                ToolResult {
                    content: "lookup cancelled".into(),
                    is_error: true,
                    output_ref: None,
                }
            }
        }
    }
}

/// A non-interactive host, the only one whose Subagent calls wait for the report and so can be
/// moved; the terminal runs every Subagent in the background from the start.
fn waiting_host(model: Arc<HeldChildren>) -> Agent {
    Agent::new_embedded_with_subagent_launch(config(), model, SubagentLaunch::BackgroundByDefault)
}

fn subagent(id: &str, name: &str) -> Content {
    Content::ToolUse {
        id: id.into(),
        name: "Subagent".into(),
        input: json!({"task": "Inspect bounded evidence", "name": name, "run_in_background": false}),
    }
}

async fn eventually(mut condition: impl AsyncFnMut() -> bool) {
    tokio::time::timeout(WAIT, async {
        while !condition().await {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("condition was not reached");
}

/// The text of the result `call_id` got in `request`.
fn tool_result_text(request: &ModelRequest, call_id: &str) -> String {
    request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|content| match content {
            Content::ToolResult {
                tool_use_id,
                content,
                ..
            } if tool_use_id == call_id => Some(content.text().into_owned()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{call_id} has a result"))
}

/// The newest user text of `request` that is neither a host note nor a notification.
fn newest_prompt(request: &ModelRequest) -> Option<&str> {
    request
        .messages
        .iter()
        .rev()
        .filter(|message| message.role == Role::User && !is_host_note(message))
        .flat_map(|message| message.content.iter().rev())
        .find_map(|content| match content {
            Content::Text { text } if !text.starts_with('<') => Some(text.as_str()),
            _ => None,
        })
}

/// Follows `active` to its end and returns whether `queued` joined it; an interruption fails.
async fn joins_without_interruption(
    events: &mut broadcast::Receiver<Event>,
    active: &Receipt,
    queued: &Receipt,
) -> bool {
    let mut joined = false;
    loop {
        let event = next_event(events).await;
        match event.kind {
            EventKind::InputMerged { into_run_id } if event.run_id == queued.run_id => {
                joined = into_run_id == active.run_id;
            }
            EventKind::Interrupted if event.chat_id == active.chat_id => {
                panic!("send now interrupted the turn: {event:?}")
            }
            EventKind::Completed { .. } if event.run_id == active.run_id => return joined,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
}

async fn queue(agent: &Agent, chat: ChatId) -> Receipt {
    let queued = agent.submit(chat, QUEUED.into()).await.unwrap();
    assert_eq!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .queued_inputs()
            .iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
        [queued.run_id],
        "a message alone waits for the tool boundary"
    );
    queued
}

fn assert_moved_subagent(request: &ModelRequest, call_id: &str, handle: &str) {
    let result: Value = serde_json::from_str(&tool_result_text(request, call_id)).unwrap();
    assert_eq!(result["status"], "async_launched", "{result}");
    assert_eq!(result["handle"], handle, "{result}");
    assert_eq!(result["message"], MOVED_SUBAGENT, "{result}");
}

#[tokio::test]
async fn send_now_moves_a_foreground_subagent_so_the_turn_takes_the_message() {
    let model = HeldChildren::new(|request| {
        if has_tool_result(request, "launch") {
            text(PARENT_REPLY)
        } else {
            vec![subagent("launch", "audit")]
        }
    });
    let agent = waiting_host(model.clone());
    let directory = tempfile::tempdir().unwrap();
    let root = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let active = agent.submit(root, "Start the audit".into()).await.unwrap();
    eventually(async || model.children.load(Ordering::SeqCst) == 1).await;
    let queued = queue(&agent, root).await;

    assert!(
        agent
            .send_now(root, queued.run_id)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        joins_without_interruption(&mut events, &active, &queued).await,
        "the message joins the turn the Subagent call was part of"
    );
    let requests = model.root_requests();
    assert_eq!(requests.len(), 2);
    assert_moved_subagent(&requests[1], "launch", "audit");
    assert_eq!(newest_prompt(&requests[1]), Some(QUEUED));
    for request in &requests {
        assert_eq!(
            serde_json::to_vec(&request.tools).unwrap(),
            serde_json::to_vec(&requests[0].tools).unwrap(),
            "moving a call leaves the listed tools as they were"
        );
        assert_eq!(
            serde_json::to_vec(&request.messages[..2]).unwrap(),
            serde_json::to_vec(&requests[0].messages[..2]).unwrap(),
            "moving a call leaves the system part as it was"
        );
    }
    let snapshot = agent.snapshot(root).await.unwrap();
    assert!(
        snapshot.family_completions.is_empty(),
        "the child keeps working"
    );
    assert_eq!(model.children.load(Ordering::SeqCst), 1);

    model.release.cancel();
    let delivered = wait_for_snapshot(&agent, root.uuid(), "the child's completion", |snapshot| {
        task_notifications(snapshot).len() == 1 && snapshot.chat_loop.state == LoopState::Idle
    })
    .await;
    let (_, notification) = task_notifications(&delivered)[0];
    assert!(notification.contains(CHILD_REPORT), "{notification}");
    assert_eq!(model.root_requests().len(), 3);
    agent.shutdown().await;
}

#[tokio::test]
async fn send_now_moves_every_parallel_foreground_subagent() {
    let model = HeldChildren::new(|request| {
        if has_tool_result(request, "first") {
            text(PARENT_REPLY)
        } else {
            vec![subagent("first", "audit"), subagent("second", "review")]
        }
    });
    let agent = waiting_host(model.clone());
    let directory = tempfile::tempdir().unwrap();
    let root = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let active = agent.submit(root, "Start both".into()).await.unwrap();
    eventually(async || model.children.load(Ordering::SeqCst) >= 1).await;
    let queued = queue(&agent, root).await;

    assert!(
        agent
            .send_now(root, queued.run_id)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(joins_without_interruption(&mut events, &active, &queued).await);
    let requests = model.root_requests();
    assert_eq!(requests.len(), 2);
    assert_moved_subagent(&requests[1], "first", "audit");
    assert_moved_subagent(&requests[1], "second", "review");
    assert_eq!(newest_prompt(&requests[1]), Some(QUEUED));
    eventually(async || model.children.load(Ordering::SeqCst) == 2).await;

    model.release.cancel();
    let delivered = wait_for_snapshot(&agent, root.uuid(), "both completions", |snapshot| {
        task_notifications(snapshot).len() == 2 && snapshot.chat_loop.state == LoopState::Idle
    })
    .await;
    assert_eq!(delivered.family_completions.len(), 2);
    agent.shutdown().await;
}

#[tokio::test]
async fn enter_alone_keeps_waiting_for_a_foreground_subagent() {
    let model = HeldChildren::new(|request| {
        if has_tool_result(request, "launch") {
            text(PARENT_REPLY)
        } else {
            vec![subagent("launch", "audit")]
        }
    });
    let agent = waiting_host(model.clone());
    let directory = tempfile::tempdir().unwrap();
    let root = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let active = agent.submit(root, "Start the audit".into()).await.unwrap();
    eventually(async || model.children.load(Ordering::SeqCst) == 1).await;
    let queued = queue(&agent, root).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        agent.snapshot(root).await.unwrap().queued_inputs().len(),
        1,
        "a queued message moves nothing"
    );
    assert_eq!(model.root_requests().len(), 1);

    model.release.cancel();
    assert!(joins_without_interruption(&mut events, &active, &queued).await);
    let requests = model.root_requests();
    let result: Value = serde_json::from_str(&tool_result_text(&requests[1], "launch")).unwrap();
    assert_eq!(result["status"], "completed", "{result}");
    assert_eq!(newest_prompt(&requests[1]), Some(QUEUED));
    let snapshot = agent.snapshot(root).await.unwrap();
    assert!(task_notifications(&snapshot).is_empty());
    agent.shutdown().await;
}

#[tokio::test]
async fn send_now_moves_a_foreground_bash_command_that_then_notifies() {
    let directory = tempfile::tempdir().unwrap();
    let gate = directory.path().join("release");
    let command = format!(
        "while [ ! -e '{}' ]; do sleep 0.05; done; printf finished",
        gate.display()
    );
    let model = HeldChildren::new(move |request| {
        if has_tool_result(request, "long-shell") {
            text(PARENT_REPLY)
        } else {
            call(
                "long-shell",
                "Bash",
                json!({"command": command, "description": "Wait for the release gate"}),
            )
        }
    });
    let agent = Agent::new_embedded(config(), model.clone());
    agent.install_own_tickhost();
    let root = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(root, tickloop_harness::protocol::ExecutionMode::FullAccess)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let active = agent.submit(root, "Run it".into()).await.unwrap();
    let running = loop {
        match next_event(&mut events).await.kind {
            EventKind::CommandUpdated { command }
                if command.state == CommandState::Running && command.pid.is_some() =>
            {
                break command;
            }
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    };
    let queued = queue(&agent, root).await;

    assert!(
        agent
            .send_now(root, queued.run_id)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(joins_without_interruption(&mut events, &active, &queued).await);
    let requests = model.root_requests();
    assert_eq!(
        tool_result_text(&requests[1], "long-shell"),
        format!(
            "Command was moved to the background (ID: {}) so that a message that arrived while it was running can reach you; it was not interrupted. Output is being written to: {}. You will be notified when it completes.",
            tickloop_ids::IdContext::new().model_text(
                tickloop_harness::protocol::ShellTaskId::from(running.operation_id)
            ),
            running.output_ref.display()
        )
    );
    assert_eq!(newest_prompt(&requests[1]), Some(QUEUED));
    assert_eq!(
        agent.commands(root).await.unwrap()[0].state,
        CommandState::Background,
        "the command keeps running"
    );

    std::fs::write(&gate, b"release").unwrap();
    let delivered = wait_for_snapshot(
        &agent,
        root.uuid(),
        "the command's completion",
        |snapshot| {
            task_notifications(snapshot).len() == 1 && snapshot.chat_loop.state == LoopState::Idle
        },
    )
    .await;
    let (_, notification) = task_notifications(&delivered)[0];
    assert!(
        notification.contains("<status>completed</status>"),
        "{notification}"
    );
    assert_eq!(
        std::fs::read_to_string(&running.output_ref).unwrap(),
        "finished"
    );
    agent.shutdown().await;
}

async fn agent_with_lookup(
    model: Arc<HeldChildren>,
    directory: &std::path::Path,
) -> (Agent, ChatId, Arc<SlowLookup>) {
    let agent = waiting_host(model);
    let lookup = Arc::new(SlowLookup::default());
    agent.install_host_tools(lookup.clone()).unwrap();
    let root = agent.create_chat(directory.into()).await.unwrap();
    (agent, root, lookup)
}

/// Waits for the run of `active` to be interrupted and for `queued` to complete in a run of its
/// own.
async fn interrupted_then_answered(
    events: &mut broadcast::Receiver<Event>,
    active: &Receipt,
    queued: &Receipt,
) {
    let mut interrupted = false;
    loop {
        let event = next_event(events).await;
        match event.kind {
            EventKind::Interrupted if event.run_id == active.run_id => interrupted = true,
            EventKind::InputMerged { into_run_id } if into_run_id == active.run_id => {
                panic!("a message sent now never joins the interrupted run")
            }
            EventKind::Completed { .. } if event.run_id == queued.run_id => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert!(interrupted, "the run ahead of the message is interrupted");
}

#[tokio::test]
async fn send_now_interrupts_a_call_that_cannot_move() {
    let model = HeldChildren::new(|request| {
        if newest_prompt(request) == Some(QUEUED) {
            text(PARENT_REPLY)
        } else {
            call("lookup", SLOW_LOOKUP, json!({}))
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let (agent, root, lookup) = agent_with_lookup(model.clone(), directory.path()).await;
    let mut events = agent.subscribe();
    let active = agent.submit(root, "Look it up".into()).await.unwrap();
    loop {
        if matches!(next_event(&mut events).await.kind, EventKind::ToolStarted { name, .. } if name == SLOW_LOOKUP)
        {
            break;
        }
    }
    let queued = queue(&agent, root).await;

    let sent_at = std::time::Instant::now();
    agent.send_now(root, queued.run_id).await.unwrap();
    interrupted_then_answered(&mut events, &active, &queued).await;
    assert!(sent_at.elapsed() < Duration::from_secs(5));
    assert_eq!(lookup.cancelled.load(Ordering::SeqCst), 1);
    let requests = model.root_requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(newest_prompt(&requests[1]), Some(QUEUED));
    agent.shutdown().await;
}

#[tokio::test]
async fn a_queued_message_waits_for_a_call_that_cannot_move() {
    let model = HeldChildren::new(|request| {
        if has_tool_result(request, "lookup") {
            text(PARENT_REPLY)
        } else {
            call("lookup", SLOW_LOOKUP, json!({}))
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let (agent, root, lookup) = agent_with_lookup(model.clone(), directory.path()).await;
    let mut events = agent.subscribe();
    let active = agent.submit(root, "Look it up".into()).await.unwrap();
    loop {
        if matches!(next_event(&mut events).await.kind, EventKind::ToolStarted { name, .. } if name == SLOW_LOOKUP)
        {
            break;
        }
    }
    let queued = queue(&agent, root).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        model.root_requests().len(),
        1,
        "the message waits for the call"
    );

    lookup.release.cancel();
    assert!(joins_without_interruption(&mut events, &active, &queued).await);
    assert_eq!(lookup.cancelled.load(Ordering::SeqCst), 0);
    let requests = model.root_requests();
    assert_eq!(tool_result_text(&requests[1], "lookup"), "looked up");
    assert_eq!(newest_prompt(&requests[1]), Some(QUEUED));
    agent.shutdown().await;
}

/// Claude Code moves what it can, then interrupts work that cannot move once its grace ends:
/// the moved Subagent keeps running and still reports as a task notification.
#[tokio::test]
async fn send_now_interrupts_work_left_after_moving_a_subagent() {
    let model = HeldChildren::new(|request| {
        if newest_prompt(request) == Some(QUEUED) {
            text(PARENT_REPLY)
        } else {
            let mut calls = vec![subagent("launch", "audit")];
            calls.extend(call("lookup", SLOW_LOOKUP, json!({})));
            calls
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let (agent, root, lookup) = agent_with_lookup(model.clone(), directory.path()).await;
    let mut events = agent.subscribe();
    let active = agent.submit(root, "Start the audit".into()).await.unwrap();
    eventually(async || model.children.load(Ordering::SeqCst) == 1).await;
    let queued = queue(&agent, root).await;

    assert!(
        agent
            .send_now(root, queued.run_id)
            .await
            .unwrap()
            .is_empty()
    );
    interrupted_then_answered(&mut events, &active, &queued).await;
    let requests = model.root_requests();
    assert_moved_subagent(&requests[1], "launch", "audit");
    assert_eq!(lookup.cancelled.load(Ordering::SeqCst), 1);
    assert_eq!(newest_prompt(requests.last().unwrap()), Some(QUEUED));

    model.release.cancel();
    wait_for_snapshot(
        &agent,
        root.uuid(),
        "the moved child's completion",
        |snapshot| {
            task_notifications(snapshot).len() == 1 && snapshot.chat_loop.state == LoopState::Idle
        },
    )
    .await;
    agent.shutdown().await;
}
