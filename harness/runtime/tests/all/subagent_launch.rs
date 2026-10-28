//! How a `Subagent` call runs, following Claude Code. In the terminal every Subagent runs in the
//! background: the model is not offered `run_in_background`, the call returns at once, and each
//! child's completion arrives as a task notification that starts a turn of its own. The whole
//! family follows its root's host, grandchildren included. A non-interactive host backgrounds by
//! default and lets the model wait for a report with `run_in_background: false`.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use serde_json::{Value, json};
use tickloop_harness_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_harness_protocol::{
    ChatId, Content, EventKind, LoopState, ModelIdentity, Receipt, Role, ToolDefinition,
};
use tickloop_harness_runtime::{Agent, SubagentLaunch};
use tickloop_harness_test_support::routed::{RoutedModel, has_tool_result, tool_result};
use tickloop_harness_test_support::subagents::{task_notifications, wait_for_snapshot};
use tickloop_harness_test_support::{config, is_conversation, is_host_note, next_event, text};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

const FIRST_TASK: &str = "Inspect the first module";
const SECOND_TASK: &str = "Inspect the second module";
const GRANDCHILD_TASK: &str = "Inspect one file";
const QUESTION: &str = "How is it going?";
const ALWAYS_BACKGROUND: &str = "Subagents run in the background; you'll be notified when one completes. Never fabricate or predict a pending agent's results — the notification is never something you write yourself; if the user asks before it arrives, say it's still running.";
const BACKGROUND_BY_DEFAULT: &str = "Subagents run in the background by default; you'll be notified when one completes. Pass `run_in_background: false` only when your very next action depends on the result and nothing else could usefully happen while it runs — otherwise background it so the user can interject. Never fabricate or predict a pending agent's results — the notification is never something you write yourself; if the user asks before it arrives, say it's still running.";

/// The task the requesting member was delegated; `None` for the root.
fn task_of(request: &ModelRequest) -> Option<String> {
    let first = request
        .messages
        .iter()
        .find(|message| is_conversation(message))?;
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

fn notifications_in(request: &ModelRequest) -> usize {
    request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter(|content| matches!(content, Content::Text { text } if text.starts_with("<task-notification>")))
        .count()
}

fn subagent(id: &str, input: Value) -> Content {
    Content::ToolUse {
        id: id.into(),
        name: "Subagent".into(),
        input,
    }
}

/// Answers the family from `route`; a member's first reply waits until the release of its task,
/// if it has one, is cancelled.
struct Family {
    routed: RoutedModel,
    members_started: AtomicUsize,
    releases: Vec<(&'static str, CancellationToken)>,
}

impl Family {
    fn new(
        releases: &[&'static str],
        route: impl Fn(&ModelRequest) -> Vec<Content> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            routed: RoutedModel::new(route),
            members_started: AtomicUsize::new(0),
            releases: releases
                .iter()
                .map(|task| (*task, CancellationToken::new()))
                .collect(),
        })
    }

    fn release(&self, task: &str) {
        self.releases
            .iter()
            .find(|(held, _)| *held == task)
            .unwrap()
            .1
            .cancel();
    }

    fn requests_of(&self, task: Option<&str>) -> Vec<ModelRequest> {
        self.routed
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| task_of(request).as_deref() == task)
            .cloned()
            .collect()
    }
}

#[async_trait]
impl ModelClient for Family {
    fn identity(&self) -> ModelIdentity {
        self.routed.identity()
    }

    async fn generate_title(
        &self,
        request: ModelRequest,
        _: UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        Ok(tickloop_harness_test_support::title_output(request))
    }

    async fn complete(
        &self,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        if let Some(task) = task_of(&request) {
            self.members_started.fetch_add(1, Ordering::SeqCst);
            if let Some((_, release)) = self.releases.iter().find(|(held, _)| *held == task) {
                tokio::select! {
                    () = release.cancelled() => {}
                    () = cancel.cancelled() => return Err(ModelError::Cancelled),
                }
            }
        }
        self.routed.complete(request, events, cancel).await
    }
}

fn subagent_definition(request: &ModelRequest) -> &ToolDefinition {
    request
        .tools
        .iter()
        .find(|tool| tool.name == "Subagent")
        .expect("Subagent is offered")
}

async fn turn_completes(agent: &Agent, chat: ChatId, prompt: &str) -> Receipt {
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, prompt.into()).await.unwrap();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != receipt.run_id {
            continue;
        }
        match event.kind {
            EventKind::Completed { .. } => return receipt,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
}

fn assert_launched(request: &ModelRequest, call_id: &str) -> Value {
    let result = tool_result(&request.messages, call_id).expect("the call has a result");
    assert_eq!(result["status"], "async_launched", "{result}");
    assert!(result["child_chat_id"].is_string(), "{result}");
    assert!(
        result["message"]
            .as_str()
            .is_some_and(|message| message.starts_with("The agent is working in the background.")),
        "{result}"
    );
    result
}

/// Two children launched together: the turn that launched them ends at once, and each child's
/// completion arrives on its own as a task notification that starts a turn of its own. Every
/// request of the Chat lists the same tools.
#[tokio::test]
async fn parallel_subagents_return_at_once_and_each_completion_starts_a_turn() {
    let model = Family::new(&[FIRST_TASK, SECOND_TASK], |request| {
        if task_of(request).is_some() {
            return text("Module report");
        }
        if !has_tool_result(request, "first") {
            vec![
                subagent("first", json!({"task": FIRST_TASK, "name": "first"})),
                subagent("second", json!({"task": SECOND_TASK, "name": "second"})),
            ]
        } else if notifications_in(request) > 0 {
            text("Noted the report")
        } else {
            text("Both are running")
        }
    });
    let agent = Agent::new_embedded(config(), model.clone());
    let directory = tempfile::tempdir().unwrap();
    let root = agent.create_chat(directory.path().into()).await.unwrap();
    turn_completes(&agent, root, "Inspect both modules").await;

    let root_requests = model.requests_of(None);
    assert_eq!(
        root_requests.len(),
        2,
        "the launch turn ended without waiting"
    );
    let first = assert_launched(&root_requests[1], "first");
    let second = assert_launched(&root_requests[1], "second");
    assert_eq!(first["handle"], "first");
    assert_eq!(second["handle"], "second");
    assert_eq!(
        agent.snapshot(root).await.unwrap().chat_loop.state,
        LoopState::Idle
    );

    model.release(FIRST_TASK);
    wait_for_snapshot(
        &agent,
        root.uuid(),
        "the first completion's turn",
        |snapshot| {
            task_notifications(snapshot).len() == 1 && snapshot.chat_loop.state == LoopState::Idle
        },
    )
    .await;
    model.release(SECOND_TASK);
    wait_for_snapshot(
        &agent,
        root.uuid(),
        "the second completion's turn",
        |snapshot| {
            task_notifications(snapshot).len() == 2 && snapshot.chat_loop.state == LoopState::Idle
        },
    )
    .await;
    let root_requests = model.requests_of(None);
    assert_eq!(
        root_requests.len(),
        4,
        "each completion took a turn of its own"
    );
    assert_eq!(notifications_in(&root_requests[2]), 1);
    assert_eq!(notifications_in(&root_requests[3]), 2);
    for request in &root_requests {
        assert_eq!(
            serde_json::to_vec(&request.tools).unwrap(),
            serde_json::to_vec(&root_requests[0].tools).unwrap()
        );
        assert_eq!(
            serde_json::to_vec(&request.messages[..2]).unwrap(),
            serde_json::to_vec(&root_requests[0].messages[..2]).unwrap()
        );
    }
    agent.shutdown().await;
}

/// While a child works, the parent is idle, so a message the user sends is answered at once.
#[tokio::test]
async fn a_message_sent_while_a_subagent_works_is_answered_at_once() {
    let model = Family::new(&[FIRST_TASK], |request| {
        if task_of(request).is_some() {
            text("Module report")
        } else if newest_prompt(request) == Some(QUESTION) && notifications_in(request) == 0 {
            text("It is still running")
        } else if !has_tool_result(request, "first") {
            vec![subagent("first", json!({"task": FIRST_TASK}))]
        } else {
            text("Done for now")
        }
    });
    let agent = Agent::new_embedded(config(), model.clone());
    let directory = tempfile::tempdir().unwrap();
    let root = agent.create_chat(directory.path().into()).await.unwrap();
    turn_completes(&agent, root, "Inspect the module").await;
    let question = turn_completes(&agent, root, QUESTION).await;

    let snapshot = agent.snapshot(root).await.unwrap();
    assert!(
        snapshot.pending_inputs.is_empty() && snapshot.family_completions.is_empty(),
        "the question took a turn of its own while the child still works"
    );
    assert_eq!(snapshot.chat_loop.run_id, Some(question.run_id));
    assert_eq!(
        newest_prompt(model.requests_of(None).last().unwrap()),
        Some(QUESTION)
    );

    model.release(FIRST_TASK);
    wait_for_snapshot(&agent, root.uuid(), "the completion's turn", |snapshot| {
        task_notifications(snapshot).len() == 1 && snapshot.chat_loop.state == LoopState::Idle
    })
    .await;
    agent.shutdown().await;
}

/// The terminal does not offer `run_in_background`; a call that still passes it, from an older
/// conversation or a habit of the model, runs in the background without an error.
#[tokio::test]
async fn the_terminal_ignores_a_run_in_background_it_does_not_offer() {
    let model = Family::new(&[FIRST_TASK], |request| {
        if task_of(request).is_some() {
            text("Module report")
        } else if !has_tool_result(request, "first") {
            vec![subagent(
                "first",
                json!({"task": FIRST_TASK, "run_in_background": false}),
            )]
        } else {
            text("Launched")
        }
    });
    let agent = Agent::new_embedded(config(), model.clone());
    let directory = tempfile::tempdir().unwrap();
    let root = agent.create_chat(directory.path().into()).await.unwrap();
    turn_completes(&agent, root, "Inspect the module").await;

    let root_requests = model.requests_of(None);
    let definition = subagent_definition(&root_requests[0]);
    assert!(
        definition.description.contains(ALWAYS_BACKGROUND),
        "{}",
        definition.description
    );
    assert!(!definition.description.contains("foreground"));
    assert!(
        definition.input_schema["properties"]
            .get("run_in_background")
            .is_none()
    );
    assert_eq!(definition.input_schema["additionalProperties"], false);
    assert_launched(&root_requests[1], "first");

    model.release(FIRST_TASK);
    wait_for_snapshot(&agent, root.uuid(), "the completion's turn", |snapshot| {
        task_notifications(snapshot).len() == 1 && snapshot.chat_loop.state == LoopState::Idle
    })
    .await;
    agent.shutdown().await;
}

/// A child of a terminal family delegates in the background too: a grandchild follows the
/// root's host, and the child is not offered `run_in_background` either.
#[tokio::test]
async fn a_childs_own_subagent_follows_the_roots_host() {
    let model = Family::new(&[GRANDCHILD_TASK], |request| {
        match task_of(request).as_deref() {
            Some(GRANDCHILD_TASK) => text("File report"),
            Some(_) if !has_tool_result(request, "grandchild") => vec![subagent(
                "grandchild",
                json!({"task": GRANDCHILD_TASK, "run_in_background": false}),
            )],
            Some(_) => text("Child launched its own helper"),
            None if !has_tool_result(request, "child") => {
                vec![subagent("child", json!({"task": FIRST_TASK}))]
            }
            None => text("Child launched"),
        }
    });
    let agent = Agent::new_embedded(config(), model.clone());
    let directory = tempfile::tempdir().unwrap();
    let root = agent.create_chat(directory.path().into()).await.unwrap();
    turn_completes(&agent, root, "Inspect the module").await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while model.requests_of(Some(FIRST_TASK)).len() < 2 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the child goes on after launching its own helper");

    let child_requests = model.requests_of(Some(FIRST_TASK));
    assert_eq!(
        child_requests.len(),
        2,
        "the child went on without waiting for the grandchild"
    );
    let definition = subagent_definition(&child_requests[0]);
    assert!(definition.description.contains(ALWAYS_BACKGROUND));
    assert!(
        definition.input_schema["properties"]
            .get("run_in_background")
            .is_none()
    );
    assert_launched(&child_requests[1], "grandchild");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while model.members_started.load(Ordering::SeqCst) < 3 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the grandchild starts while its parent goes on");

    // A delegated member reports once its own children have; the report then reaches the root.
    model.release(GRANDCHILD_TASK);
    wait_for_snapshot(&agent, root.uuid(), "the child's completion", |snapshot| {
        task_notifications(snapshot).len() == 1 && snapshot.chat_loop.state == LoopState::Idle
    })
    .await;
    agent.shutdown().await;
}

/// A non-interactive host offers `run_in_background`, true by default: an omitted value runs
/// the child in the background, and `false` waits for its report.
#[tokio::test]
async fn a_non_interactive_host_backgrounds_by_default_and_may_wait() {
    let model = Family::new(&[FIRST_TASK], |request| {
        if task_of(request).is_some() {
            text("Module report")
        } else if !has_tool_result(request, "background") {
            vec![subagent("background", json!({"task": FIRST_TASK}))]
        } else if !has_tool_result(request, "waited") {
            vec![subagent(
                "waited",
                json!({"task": SECOND_TASK, "run_in_background": false}),
            )]
        } else if notifications_in(request) > 0 {
            text("Noted the report")
        } else {
            text("Waited for the second report")
        }
    });
    let agent = Agent::new_embedded_with_subagent_launch(
        config(),
        model.clone(),
        SubagentLaunch::BackgroundByDefault,
    );
    let directory = tempfile::tempdir().unwrap();
    let root = agent.create_chat(directory.path().into()).await.unwrap();
    turn_completes(&agent, root, "Inspect both modules").await;

    let root_requests = model.requests_of(None);
    let definition = subagent_definition(&root_requests[0]);
    assert!(
        definition.description.contains(BACKGROUND_BY_DEFAULT),
        "{}",
        definition.description
    );
    assert_eq!(
        definition.input_schema["properties"]["run_in_background"]["default"],
        true
    );
    assert_launched(&root_requests[1], "background");
    let waited = tool_result(&root_requests[2].messages, "waited").unwrap();
    assert_eq!(waited["status"], "completed", "{waited}");
    assert_eq!(waited["result"], "Module report", "{waited}");

    model.release(FIRST_TASK);
    wait_for_snapshot(
        &agent,
        root.uuid(),
        "the background completion",
        |snapshot| {
            task_notifications(snapshot).len() == 1 && snapshot.chat_loop.state == LoopState::Idle
        },
    )
    .await;
    for request in model.requests_of(None) {
        assert_eq!(
            serde_json::to_vec(&request.tools).unwrap(),
            serde_json::to_vec(&root_requests[0].tools).unwrap()
        );
    }
    agent.shutdown().await;
}
