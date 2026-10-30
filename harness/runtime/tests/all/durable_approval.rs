//! Durable Approval: on a durable host, a third-party tool call that needs a person's approval
//! suspends its Run in the Transcript instead of being denied. Whichever instance holds the Chat
//! when the person answers goes on: an allowed call runs exactly once, a denied one reads as a
//! denial to the model, and a stopped Run ends its wait as it ends a question's.

use std::sync::{Arc, Mutex};

use serde_json::json;
use tickloop_harness_protocol::{
    AgentHostOptions, ChatId, Content, Decision, EventKind, PermissionRequest, PermissionRule,
    PromptPolicy,
};
use tickloop_harness_runtime::transcript::StorePosition;
use tickloop_harness_runtime::{
    Agent, AgentError, Delivery, HarnessOptions, HarnessSeams, ParentSelection, PermissionReply,
    PersonWait, PersonWaitKind, Quiescence, ToolPolicy, Turn, TurnOp, TurnOutcome, TurnRequest,
    TurnWait, UserMessage, WaitHosting, Waits, durable_chat_host_rules,
};
use tickloop_harness_test_support::harness::ChosenModels;
use tickloop_harness_test_support::mcp_service::McpService;
use tickloop_harness_test_support::transcripts::InMemoryTranscriptStore;
use tickloop_harness_test_support::{ScriptedModel, call, config, text};
use tickloop_harness_transcript::display::{
    DisplayBlock, DisplayKind, DisplayStatus, DisplayWait, project,
};
use tickloop_harness_transcript::{AbandonReason, Line, SettledOutcome, WaitKind, WaitState};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::third_party_approval::provider;

const SERVER: &str = "review";
const CALL: &str = "read";
const DONE: &str = "Report ready.";

/// The cloud's composition of a Chat without a Folder, with `prompt_policy`.
fn options(prompt_policy: PromptPolicy) -> HarnessOptions {
    HarnessOptions {
        host: AgentHostOptions {
            prompt_policy,
            ..AgentHostOptions::product_defaults()
        },
        tools: ToolPolicy::durable_chat(),
        host_rules: durable_chat_host_rules(),
        waits: WaitHosting::Durable,
        ..HarnessOptions::local(config())
    }
}

/// A cloud harness with the MCP server's tool installed, whose model replies `replies` given the
/// tool's model name.
async fn connected(
    store: &Arc<InMemoryTranscriptStore>,
    server: &McpService,
    prompt_policy: PromptPolicy,
    replies: impl FnOnce(&str) -> Vec<Vec<Content>>,
) -> (Agent, Arc<ScriptedModel>, String) {
    connected_with(store, server, options(prompt_policy), replies).await
}

async fn connected_with(
    store: &Arc<InMemoryTranscriptStore>,
    server: &McpService,
    options: HarnessOptions,
    replies: impl FnOnce(&str) -> Vec<Vec<Content>>,
) -> (Agent, Arc<ScriptedModel>, String) {
    let (tools, tool) = provider(server, SERVER).await;
    let model = Arc::new(ScriptedModel::new(replies(&tool)));
    let agent = Agent::with_seams(
        HarnessSeams {
            search: None,
            transcripts: Some(store.clone()),
            environment: None,
            models: ChosenModels::new(model.clone()),
        },
        options,
    )
    .unwrap();
    agent.install_host_tools(tools).unwrap();
    (agent, model, tool)
}

fn prompt(text: &str, delivery: Delivery) -> TurnRequest {
    TurnRequest {
        input_id: tickloop_ids::Id::from(Uuid::new_v4()),
        op: TurnOp::Continue {
            after: ParentSelection::CurrentLeaf,
            message: UserMessage {
                text: text.into(),
                ..Default::default()
            },
        },
        delivery,
        host_context: None,
    }
}

fn answer(request: &PermissionRequest, reply: PermissionReply) -> TurnRequest {
    TurnRequest {
        input_id: tickloop_ids::Id::from(Uuid::new_v4()),
        op: TurnOp::AnswerPermission {
            request: request.id,
            reply,
        },
        delivery: Delivery::Queue,
        host_context: None,
    }
}

async fn outcome(turn: &Turn) -> TurnOutcome {
    tokio::time::timeout(std::time::Duration::from_secs(10), turn.outcome())
        .await
        .expect("the turn reaches an outcome")
        .unwrap()
}

/// Records what each call's admission did: reviewed, or put to a person.
fn watch_admissions(agent: &Agent) -> Arc<Mutex<Vec<String>>> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut events = agent.subscribe();
    let recorded = seen.clone();
    tokio::spawn(async move {
        while let Ok(event) = events.recv().await {
            match event.kind {
                EventKind::ApprovalAssessed { assessment } => recorded
                    .lock()
                    .unwrap()
                    .push(format!("assessed {}", assessment.call_id)),
                EventKind::PermissionRequested { request } => recorded
                    .lock()
                    .unwrap()
                    .push(format!("asked {}", request.name)),
                _ => {}
            }
        }
    });
    seen
}

fn wait_states(lines: &[Line]) -> Vec<String> {
    lines
        .iter()
        .filter_map(|line| match line {
            Line::Wait(entry) => Some(match &entry.state {
                WaitState::Open => "open".to_string(),
                WaitState::Resolved { .. } => "resolved".into(),
                WaitState::Allowed { .. } => "allowed".into(),
                WaitState::Appended => "appended".into(),
                WaitState::Abandoned { reason } => format!("abandoned {reason:?}"),
            }),
            _ => None,
        })
        .collect()
}

/// The text of each result the conversation holds for the call.
fn results(messages: &[tickloop_harness_protocol::Message]) -> Vec<(String, bool)> {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } if tool_use_id == CALL => Some((content.text().into_owned(), *is_error)),
            _ => None,
        })
        .collect()
}

/// The call asks under Durable Approval in one instance, which then shuts down as a host ends
/// its holding: the wait stays open in the Transcript and nothing has run.
async fn asked_and_released(
    store: &Arc<InMemoryTranscriptStore>,
    server: &McpService,
) -> (ChatId, Turn, PermissionRequest) {
    let (asking, _, tool) = connected(store, server, PromptPolicy::DurableApproval, |tool| {
        vec![call(CALL, tool, json!({"q": "status"}))]
    })
    .await;
    let admissions = watch_admissions(&asking);
    let chat = asking.create_chat_without_folder().await.unwrap();
    let turn = asking
        .advance(chat, prompt("Read the review service.", Delivery::Queue))
        .await
        .unwrap();
    let TurnOutcome::Waiting(TurnWait::Permission(request)) = outcome(&turn).await else {
        panic!("the turn waits for the person's approval");
    };
    let request = *request;
    assert_eq!(request.name, tool);
    assert_eq!(request.mcp_server.as_deref(), Some(SERVER));
    assert_eq!(request.input, json!({"q": "status"}));
    assert_eq!(
        asking.quiesce(chat).await,
        Quiescence::Waiting(Waits {
            people: vec![PersonWait {
                chat,
                request: request.id,
                kind: PersonWaitKind::Permission,
            }],
            deliveries: Vec::new(),
            next_wake_at_ms: None,
        })
    );
    // A third-party tool without a rule goes to the person, not to Auto review.
    assert_eq!(*admissions.lock().unwrap(), [format!("asked {tool}")]);
    asking.shutdown().await;
    asking.sync().await.unwrap();
    assert!(server.calls().is_empty());
    let lines = store.lines(chat);
    assert_eq!(wait_states(&lines), ["open"]);
    assert!(lines.iter().any(|line| matches!(line,
        Line::Wait(entry) if matches!(&entry.kind, WaitKind::Permission { request: waited, .. } if waited.id == request.id))));
    (chat, turn, request)
}

#[tokio::test]
async fn the_display_shows_the_permission_the_answer_waits_for() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let server = McpService::start().await;
    let (chat, _, request) = asked_and_released(&store, &server).await;
    let answer = project(&store.lines(chat))
        .into_iter()
        .find(|node| matches!(node.kind, DisplayKind::Answer { .. }))
        .unwrap();
    assert_eq!(
        answer.status,
        DisplayStatus::Waiting {
            r#for: DisplayWait::Person {
                request: request.id
            }
        }
    );
    let call_index = answer
        .blocks
        .iter()
        .position(
            |block| matches!(block, DisplayBlock::ToolCall { call_id, .. } if call_id == CALL),
        )
        .unwrap();
    assert!(matches!(
        &answer.blocks[call_index + 1],
        DisplayBlock::Permission { request_id, call_id, mcp_server, .. }
            if *request_id == request.id && call_id == CALL && mcp_server.as_deref() == Some(SERVER)
    ));
}

#[tokio::test]
async fn another_instance_runs_the_allowed_call_exactly_once() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let server = McpService::start().await;
    let (chat, turn, request) = asked_and_released(&store, &server).await;
    let run = turn.receipt().run_id;

    let (answering, model, _) = connected(&store, &server, PromptPolicy::DurableApproval, |_| {
        vec![text(DONE)]
    })
    .await;
    let admissions = watch_admissions(&answering);
    let reply = answering
        .advance(chat, answer(&request, PermissionReply::AllowOnce))
        .await
        .unwrap();
    assert!(matches!(outcome(&reply).await, TurnOutcome::Merged { into } if into == run));
    assert!(matches!(
        answering
            .settled_input(chat, turn.input_id())
            .await
            .unwrap(),
        SettledOutcome::Completed { .. }
    ));
    assert_eq!(server.calls().len(), 1, "the allowed call ran once");
    assert!(
        admissions.lock().unwrap().is_empty(),
        "the allowed call is neither asked again nor reviewed: {:?}",
        admissions.lock().unwrap()
    );
    let snapshot = answering.snapshot(chat).await.unwrap();
    assert_eq!(results(&snapshot.messages), [("MCP_OK".to_string(), false)]);
    assert!(snapshot.permissions.is_empty());
    let requests = model.requests.lock().unwrap().clone();
    assert_eq!(
        requests.len(),
        1,
        "the model goes on after the call's result"
    );
    answering.sync().await.unwrap();
    let waits = wait_states(&store.lines(chat));
    assert_eq!(
        (
            waits.first().unwrap().as_str(),
            waits.last().unwrap().as_str()
        ),
        ("open", "appended")
    );

    // The answer is a ledger input: repeating it repeats nothing.
    let again = answering
        .advance(
            chat,
            TurnRequest {
                input_id: reply.input_id(),
                ..answer(&request, PermissionReply::AllowOnce)
            },
        )
        .await
        .unwrap();
    assert!(matches!(outcome(&again).await, TurnOutcome::Merged { into } if into == run));
    let another = answering
        .advance(chat, answer(&request, PermissionReply::AllowOnce))
        .await;
    assert!(matches!(another, Err(AgentError::Invalid(_))));
    assert_eq!(server.calls().len(), 1);
    answering.shutdown().await;

    // A later instance finds nothing left to run.
    let (later, model, _) = connected(&store, &server, PromptPolicy::DurableApproval, |_| {
        Vec::new()
    })
    .await;
    later.resume_chat(chat).await.unwrap();
    assert_eq!(later.quiesce(chat).await, Quiescence::Idle);
    assert_eq!(server.calls().len(), 1);
    assert!(model.requests.lock().unwrap().is_empty());
    later.shutdown().await;
}

#[tokio::test]
async fn the_asking_instance_runs_the_allowed_call_itself() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let server = McpService::start().await;
    let (agent, _, _) = connected(&store, &server, PromptPolicy::DurableApproval, |tool| {
        vec![call(CALL, tool, json!({})), text(DONE)]
    })
    .await;
    let chat = agent.create_chat_without_folder().await.unwrap();
    let turn = agent
        .advance(chat, prompt("Read the review service.", Delivery::Queue))
        .await
        .unwrap();
    let TurnOutcome::Waiting(TurnWait::Permission(request)) = outcome(&turn).await else {
        panic!("the turn waits for the person's approval");
    };
    let reply = agent
        .advance(chat, answer(&request, PermissionReply::AllowOnce))
        .await
        .unwrap();
    assert!(matches!(outcome(&reply).await, TurnOutcome::Merged { .. }));
    assert!(matches!(
        outcome(&turn).await,
        TurnOutcome::Completed { .. }
    ));
    assert_eq!(server.calls().len(), 1);
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(results(&snapshot.messages), [("MCP_OK".to_string(), false)]);
    agent.sync().await.unwrap();
    let waits = wait_states(&store.lines(chat));
    assert_eq!(
        (
            waits.first().unwrap().as_str(),
            waits.last().unwrap().as_str()
        ),
        ("open", "appended")
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn a_denial_with_a_comment_reaches_the_model_and_the_run_goes_on() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let server = McpService::start().await;
    let (chat, turn, request) = asked_and_released(&store, &server).await;
    let (answering, model, _) = connected(&store, &server, PromptPolicy::DurableApproval, |_| {
        vec![text(DONE)]
    })
    .await;
    let reply = answering
        .advance(
            chat,
            answer(
                &request,
                PermissionReply::Deny {
                    comment: Some("Use last week's report.".into()),
                },
            ),
        )
        .await
        .unwrap();
    assert!(matches!(outcome(&reply).await, TurnOutcome::Merged { .. }));
    assert!(matches!(
        answering
            .settled_input(chat, turn.input_id())
            .await
            .unwrap(),
        SettledOutcome::Completed { .. }
    ));
    assert!(server.calls().is_empty(), "a denied call never runs");
    let snapshot = answering.snapshot(chat).await.unwrap();
    let results = results(&snapshot.messages);
    assert_eq!(results.len(), 1);
    assert!(results[0].1);
    assert!(
        results[0].0.contains("denied") && results[0].0.contains("Use last week's report."),
        "{results:?}"
    );
    let requests = model.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1, "the model reads the denial and goes on");
    assert!(format!("{:?}", requests[0].messages).contains("Use last week's report."));
    answering.shutdown().await;
}

#[tokio::test]
async fn a_bare_denial_reaches_the_model_and_ends_the_run() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let server = McpService::start().await;
    let (chat, turn, request) = asked_and_released(&store, &server).await;
    let (answering, model, _) = connected(&store, &server, PromptPolicy::DurableApproval, |_| {
        vec![text(DONE)]
    })
    .await;
    let reply = answering
        .advance(
            chat,
            answer(&request, PermissionReply::Deny { comment: None }),
        )
        .await
        .unwrap();
    assert!(matches!(outcome(&reply).await, TurnOutcome::Merged { .. }));
    answering
        .settled_input(chat, turn.input_id())
        .await
        .unwrap();
    assert!(server.calls().is_empty(), "a denied call never runs");
    let snapshot = answering.snapshot(chat).await.unwrap();
    assert_eq!(
        results(&snapshot.messages),
        [(
            "permission denied; the requested operation was not executed".to_string(),
            true
        )]
    );
    assert!(
        model.requests.lock().unwrap().is_empty(),
        "a bare denial ends the run as it does on an interactive host"
    );
    answering.shutdown().await;
}

#[tokio::test]
async fn stopping_a_rebuilt_permission_wait_ends_it_as_a_questions() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let server = McpService::start().await;
    let (chat, _, request) = asked_and_released(&store, &server).await;
    let (rebuilt, model, _) = connected(&store, &server, PromptPolicy::DurableApproval, |_| {
        Vec::new()
    })
    .await;
    let snapshot = rebuilt.resume_chat(chat).await.unwrap();
    assert_eq!(
        snapshot
            .permissions
            .iter()
            .map(|waiting| waiting.id)
            .collect::<Vec<_>>(),
        [request.id]
    );
    assert!(matches!(
        rebuilt.quiesce(chat).await,
        Quiescence::Waiting(Waits { people, .. }) if people == [PersonWait { chat, request: request.id, kind: PersonWaitKind::Permission }]
    ));
    rebuilt.stop(chat).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while rebuilt.quiesce(chat).await != Quiescence::Idle {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the stopped Run ends");
    assert!(server.calls().is_empty());
    assert!(model.requests.lock().unwrap().is_empty());
    let late = rebuilt
        .advance(chat, answer(&request, PermissionReply::AllowOnce))
        .await;
    assert!(matches!(late, Err(AgentError::Invalid(_))));
    rebuilt.sync().await.unwrap();
    assert_eq!(
        wait_states(&store.lines(chat)).last().unwrap(),
        &format!("abandoned {:?}", AbandonReason::Interrupted)
    );
    assert!(server.calls().is_empty());
    rebuilt.shutdown().await;
}

/// A host that denies whatever would prompt still denies the third-party call Auto review does
/// not admit: no wait, no person.
#[tokio::test]
async fn deny_on_prompt_still_denies_the_call_without_a_wait() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let server = McpService::start().await;
    let (agent, _, _) = connected(&store, &server, PromptPolicy::DenyOnPrompt, |tool| {
        vec![call(CALL, tool, json!({})), text(DONE)]
    })
    .await;
    // Auto review, which a third-party call goes to in Auto mode, does not admit it.
    agent
        .set_auto_reviewer(Arc::new(ScriptedModel::new(vec![
            verdict(true),
            verdict(true),
        ])))
        .await;
    let admissions = watch_admissions(&agent);
    let chat = agent.create_chat_without_folder().await.unwrap();
    let turn = agent
        .advance(chat, prompt("Read the review service.", Delivery::Queue))
        .await
        .unwrap();
    assert!(matches!(
        outcome(&turn).await,
        TurnOutcome::Completed { .. }
    ));
    assert!(server.calls().is_empty());
    assert!(
        !admissions
            .lock()
            .unwrap()
            .iter()
            .any(|admission| admission.starts_with("asked")),
        "{:?}",
        admissions.lock().unwrap()
    );
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(results(&snapshot.messages)[0].1, "the call is denied");
    agent.sync().await.unwrap();
    assert!(wait_states(&store.lines(chat)).is_empty());
    agent.shutdown().await;
}

/// A rule the host or the user saved for the tool decides it under Durable Approval as anywhere
/// else: an Allow runs the call without asking.
#[tokio::test]
async fn an_allow_rule_runs_a_third_party_call_without_asking() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let server = McpService::start().await;
    let rule = PermissionRule {
        id: Uuid::new_v4(),
        decision: Decision::Allow,
        tool: format!("mcp__{SERVER}"),
        specifier: None,
        input: None,
        loop_id: None,
        persistent: false,
    };
    let mut options = options(PromptPolicy::DurableApproval);
    options.host_rules.push(rule);
    let (agent, _, _) = connected_with(&store, &server, options, |tool| {
        vec![call(CALL, tool, json!({})), text(DONE)]
    })
    .await;
    let admissions = watch_admissions(&agent);
    let chat = agent.create_chat_without_folder().await.unwrap();
    let turn = agent
        .advance(chat, prompt("Read the review service.", Delivery::Queue))
        .await
        .unwrap();
    assert!(matches!(
        outcome(&turn).await,
        TurnOutcome::Completed { .. }
    ));
    assert_eq!(server.calls().len(), 1);
    assert!(admissions.lock().unwrap().is_empty());
    agent.shutdown().await;
}

/// A scheduled task's run that asks waits for the person as a question would: nothing denies it
/// while nobody answers, and the answer lets it go on.
#[tokio::test]
async fn a_scheduled_tasks_run_waits_for_the_person() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let server = McpService::start().await;
    let (agent, model, tool) = connected_with(
        &store,
        &server,
        HarnessOptions {
            scheduled_task_expiry: None,
            trust_model_scheduled_tasks: true,
            ..options(PromptPolicy::DurableApproval)
        },
        |_| {
            vec![
                call(
                    "schedule",
                    "CronCreate",
                    json!({"cron": "*/10 * * * *", "prompt": "Read the review service."}),
                ),
                text("I will check every ten minutes."),
            ]
        },
    )
    .await;
    let chat = agent.create_chat_without_folder().await.unwrap();
    let request = prompt(
        "Check the review service every ten minutes.",
        Delivery::Queue,
    );
    agent.advance(chat, request.clone()).await.unwrap();
    agent.settled_input(chat, request.input_id).await.unwrap();
    let task = agent
        .snapshot(chat)
        .await
        .unwrap()
        .scheduled_tasks
        .remove(0);
    let due_at = task.next_fire_at_ms.expect("the task times at once");

    model.enqueue(call(CALL, &tool, json!({})));
    model.enqueue(text(DONE));
    let handed = agent.wake_scheduled_tasks(chat, due_at).await.unwrap();
    assert_eq!(handed.len(), 1);
    let waiting = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let Quiescence::Waiting(Waits { people, .. }) = agent.quiesce(chat).await
                && let [person] = &people[..]
            {
                return *person;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the task's run waits for the person");
    assert_eq!(waiting.kind, PersonWaitKind::Permission);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(matches!(
        agent.quiesce(chat).await,
        Quiescence::Waiting(Waits { people, .. }) if people == [waiting]
    ));
    assert!(server.calls().is_empty());

    let request = agent.snapshot(chat).await.unwrap().permissions.remove(0);
    let reply = agent
        .advance(chat, answer(&request, PermissionReply::AllowOnce))
        .await
        .unwrap();
    assert!(
        matches!(outcome(&reply).await, TurnOutcome::Merged { into } if into == handed[0].run_id)
    );
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !format!("{:?}", agent.snapshot(chat).await.unwrap().messages).contains(DONE) {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the task's run goes on");
    assert_eq!(server.calls().len(), 1);
    agent.shutdown().await;
}

/// A host that left after the person allowed the call, before running it, leaves the wait
/// allowed: the next host runs the call once without asking again.
#[tokio::test]
async fn an_allowed_call_a_host_left_runs_once_where_the_chat_loads_next() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let server = McpService::start().await;
    let (chat, turn, _) = asked_and_released(&store, &server).await;
    let lines = store
        .lines(chat)
        .into_iter()
        .map(|line| match line {
            Line::Wait(mut entry) => {
                entry.state = WaitState::Allowed { by_input: None };
                Line::Wait(entry)
            }
            line => line,
        })
        .collect();
    let left = Arc::new(InMemoryTranscriptStore::new());
    left.seed(chat, lines, StorePosition(1));
    let (next, model, _) = connected(&left, &server, PromptPolicy::DurableApproval, |_| {
        vec![text(DONE)]
    })
    .await;
    let admissions = watch_admissions(&next);
    next.resume_chat(chat).await.unwrap();
    assert!(matches!(
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            next.settled_input(chat, turn.input_id())
        )
        .await
        .unwrap()
        .unwrap(),
        SettledOutcome::Completed { .. }
    ));
    assert_eq!(server.calls().len(), 1);
    assert!(admissions.lock().unwrap().is_empty());
    assert_eq!(model.requests.lock().unwrap().len(), 1);
    next.sync().await.unwrap();
    assert_eq!(wait_states(&left.lines(chat)).last().unwrap(), "appended");
    next.shutdown().await;
}

/// The Agent's own permission answers reach a rebuilt wait too; a Session grant, which only
/// this process would keep, is refused there.
#[tokio::test]
async fn a_rebuilt_wait_takes_the_agents_permission_answer() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let server = McpService::start().await;
    let (chat, turn, request) = asked_and_released(&store, &server).await;
    let (answering, _, _) = connected(&store, &server, PromptPolicy::DurableApproval, |_| {
        vec![text(DONE)]
    })
    .await;
    answering.resume_chat(chat).await.unwrap();
    assert!(matches!(
        answering
            .answer_permission_with_scope(
                chat,
                request.id,
                true,
                tickloop_harness_protocol::PermissionScope::Session
            )
            .await,
        Err(AgentError::Invalid(_))
    ));
    answering
        .answer_permission(chat, request.id, true)
        .await
        .unwrap();
    assert!(matches!(
        answering
            .settled_input(chat, turn.input_id())
            .await
            .unwrap(),
        SettledOutcome::Completed { .. }
    ));
    assert_eq!(server.calls().len(), 1);
    answering.shutdown().await;
}

/// "Always allow" saves a rule in the user's settings, which are kept per working directory: a
/// Chat without a Folder refuses it and records nothing, so the person can still answer.
#[tokio::test]
async fn always_allowing_needs_a_folder_and_leaves_the_wait_open_without_one() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let server = McpService::start().await;
    let (chat, turn, request) = asked_and_released(&store, &server).await;
    let (answering, _, _) = connected(&store, &server, PromptPolicy::DurableApproval, |_| {
        vec![text(DONE)]
    })
    .await;
    let always = answering
        .advance(
            chat,
            answer(
                &request,
                PermissionReply::AllowAlways {
                    rule: Some(request.name.clone()),
                },
            ),
        )
        .await;
    assert!(matches!(always, Err(AgentError::Invalid(_))));
    assert!(server.calls().is_empty());
    let reply = answering
        .advance(chat, answer(&request, PermissionReply::AllowOnce))
        .await
        .unwrap();
    assert!(matches!(outcome(&reply).await, TurnOutcome::Merged { .. }));
    assert!(matches!(
        answering
            .settled_input(chat, turn.input_id())
            .await
            .unwrap(),
        SettledOutcome::Completed { .. }
    ));
    assert_eq!(server.calls().len(), 1);
    answering.shutdown().await;
}

/// The cloud's "always allow" in a Chat without a Folder: it answers once and installs a Chat rule
/// allowing the tool, which decides the tool's next call in the same Run without asking.
#[tokio::test]
async fn a_chat_rule_installed_with_the_answer_admits_the_next_call_of_the_run() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let server = McpService::start().await;
    let (chat, turn, request) = asked_and_released(&store, &server).await;
    let (answering, _, _) = connected(&store, &server, PromptPolicy::DurableApproval, |tool| {
        vec![call("again", tool, json!({"q": "details"})), text(DONE)]
    })
    .await;
    let admissions = watch_admissions(&answering);
    let mut rules = answering.resume_chat(chat).await.unwrap().policy.rules;
    rules.push(PermissionRule {
        id: Uuid::new_v4(),
        decision: Decision::Allow,
        tool: request.name.clone(),
        specifier: None,
        input: None,
        loop_id: None,
        persistent: false,
    });
    answering.set_permission_rules(chat, rules).await.unwrap();
    let reply = answering
        .advance(chat, answer(&request, PermissionReply::AllowOnce))
        .await
        .unwrap();
    assert!(matches!(outcome(&reply).await, TurnOutcome::Merged { .. }));
    assert!(matches!(
        answering
            .settled_input(chat, turn.input_id())
            .await
            .unwrap(),
        SettledOutcome::Completed { .. }
    ));
    assert_eq!(server.calls().len(), 2, "both calls ran");
    assert!(
        admissions.lock().unwrap().is_empty(),
        "the second call is neither asked nor reviewed: {:?}",
        admissions.lock().unwrap()
    );
    answering.shutdown().await;
}

/// A call that waits for its approval while the host's token for the server rotates still runs,
/// once, when allowed: the new token republishes the server's tools, but neither the server's
/// configuration and account nor the tool changed.
#[tokio::test]
async fn an_allowed_call_runs_once_after_the_hosts_token_rotated_while_it_waited() {
    use crate::mcp_host_logins::{FIRST_TOKEN, HostLogin, SECOND_TOKEN, remote};
    use tickloop_harness_runtime::host_tools::{HostCallResult, HostToolProvider};
    use tickloop_harness_runtime::mcp::{
        HostBearer, HostCapabilities, McpConnections, McpServerState, McpToolProvider,
    };

    let server = McpService::start().await;
    server.set_tools(json!([
        {"name": "read", "inputSchema": {"type": "object"}},
        {"name": "ping", "inputSchema": {"type": "object"}},
    ]));
    server.set_result(json!({"content": [{"type": "text", "text": "MCP_OK"}]}));
    server.require_bearer(FIRST_TOKEN);
    let login = HostLogin::new("account-a", Some(FIRST_TOKEN));
    let connections = McpConnections::new(
        HostCapabilities::default(),
        std::collections::BTreeMap::new(),
        std::collections::BTreeMap::new(),
    );
    connections.set_bearers(std::collections::BTreeMap::from([(
        "remote".to_owned(),
        login.clone() as Arc<dyn HostBearer>,
    )]));
    let configs = vec![remote(&server.url())];
    connections.reconcile(configs.clone()).await;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while connections.snapshot()[0].state != McpServerState::Connected {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the server connects");
    let tools = Arc::new(McpToolProvider::new(
        connections.clone(),
        Arc::new(move || configs.clone()),
    ));
    let model = Arc::new(ScriptedModel::new(vec![
        call(CALL, "mcp__remote__read", json!({})),
        text(DONE),
    ]));
    let store = Arc::new(InMemoryTranscriptStore::new());
    let agent = Agent::with_seams(
        HarnessSeams {
            search: None,
            transcripts: Some(store),
            environment: None,
            models: ChosenModels::new(model),
        },
        options(PromptPolicy::DurableApproval),
    )
    .unwrap();
    agent.install_host_tools(tools.clone()).unwrap();
    let chat = agent.create_chat_without_folder().await.unwrap();
    let turn = agent
        .advance(chat, prompt("Read the remote service.", Delivery::Queue))
        .await
        .unwrap();
    let TurnOutcome::Waiting(TurnWait::Permission(request)) = outcome(&turn).await else {
        panic!("the turn waits for the person's approval");
    };

    // The host rotates its token while the call waits; the next request carries the new one,
    // which republishes the server's tools.
    let published = tools.catalog_generation();
    login.offer(Some(SECOND_TOKEN));
    server.require_bearer(SECOND_TOKEN);
    assert!(matches!(
        connections
            .call(
                "mcp__remote__ping",
                json!({}),
                tokio_util::sync::CancellationToken::new()
            )
            .await,
        HostCallResult::Completed(_)
    ));
    assert_ne!(tools.catalog_generation(), published);

    let reply = agent
        .advance(chat, answer(&request, PermissionReply::AllowOnce))
        .await
        .unwrap();
    assert!(matches!(outcome(&reply).await, TurnOutcome::Merged { .. }));
    assert!(matches!(
        outcome(&turn).await,
        TurnOutcome::Completed { .. }
    ));
    let reads = server
        .calls()
        .iter()
        .filter(|call| call["params"]["name"] == "read")
        .count();
    assert_eq!(reads, 1, "the allowed call ran once");
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(results(&snapshot.messages), [("MCP_OK".to_string(), false)]);
    agent.shutdown().await;
    connections.shutdown().await;
}

/// A cloud harness over `store` with `tools` installed, whose model replies `replies`.
fn harness_on(
    store: Arc<dyn tickloop_harness_runtime::transcript::TranscriptStore>,
    tools: Arc<dyn tickloop_harness_runtime::host_tools::HostToolProvider>,
    replies: Vec<Vec<Content>>,
) -> (Agent, Arc<ScriptedModel>) {
    let model = Arc::new(ScriptedModel::new(replies));
    let agent = Agent::with_seams(
        HarnessSeams {
            search: None,
            transcripts: Some(store),
            environment: None,
            models: ChosenModels::new(model.clone()),
        },
        options(PromptPolicy::DurableApproval),
    )
    .unwrap();
    agent.install_host_tools(tools).unwrap();
    (agent, model)
}

/// The tools of the server `remote` at `server`, reached with the host's login `login`.
async fn host_login_tools(
    server: &McpService,
    login: &Arc<crate::mcp_host_logins::HostLogin>,
) -> Arc<tickloop_harness_runtime::mcp::McpToolProvider> {
    use tickloop_harness_runtime::mcp::{
        HostBearer, HostCapabilities, McpConnections, McpServerState, McpToolProvider,
    };
    let connections = McpConnections::new(
        HostCapabilities::default(),
        std::collections::BTreeMap::new(),
        std::collections::BTreeMap::new(),
    );
    connections.set_bearers(std::collections::BTreeMap::from([(
        "remote".to_owned(),
        login.clone() as Arc<dyn HostBearer>,
    )]));
    let configs = vec![crate::mcp_host_logins::remote(&server.url())];
    connections.reconcile(configs.clone()).await;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while connections.snapshot()[0].state != McpServerState::Connected {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the server connects");
    Arc::new(McpToolProvider::new(
        connections,
        Arc::new(move || configs.clone()),
    ))
}

/// Asks to call `tool` in an instance over `store`, which then shuts down with the wait open.
async fn ask_and_release(
    store: &Arc<InMemoryTranscriptStore>,
    tools: Arc<dyn tickloop_harness_runtime::host_tools::HostToolProvider>,
    tool: &str,
) -> (ChatId, Turn, PermissionRequest) {
    let (asking, _) = harness_on(store.clone(), tools, vec![call(CALL, tool, json!({}))]);
    let chat = asking.create_chat_without_folder().await.unwrap();
    let turn = asking
        .advance(chat, prompt("Read the service.", Delivery::Queue))
        .await
        .unwrap();
    let TurnOutcome::Waiting(TurnWait::Permission(request)) = outcome(&turn).await else {
        panic!("the turn waits for the person's approval");
    };
    asking.shutdown().await;
    asking.sync().await.unwrap();
    (chat, turn, *request)
}

/// What a rebuilt Run did with a call the person allowed.
#[derive(Debug)]
enum Rebuilt {
    /// It ran the call and went on to the end.
    Ran,
    /// It asked again: the call it prepared again reaches something else than the allowed one.
    AskedAgain(Box<PermissionRequest>),
}

/// Allows `request` in a new instance over `store` with `tools`, and says what the rebuilt Run
/// did with it.
async fn allowed_in_another_instance(
    store: &Arc<InMemoryTranscriptStore>,
    tools: Arc<dyn tickloop_harness_runtime::host_tools::HostToolProvider>,
    chat: ChatId,
    request: &PermissionRequest,
) -> (Agent, Rebuilt) {
    let (answering, _) = harness_on(store.clone(), tools, vec![text(DONE)]);
    let reply = answering
        .advance(chat, answer(request, PermissionReply::AllowOnce))
        .await
        .unwrap();
    assert!(matches!(outcome(&reply).await, TurnOutcome::Merged { .. }));
    let rebuilt = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let Some(again) = answering
                .snapshot(chat)
                .await
                .unwrap()
                .permissions
                .into_iter()
                .find(|again| again.id != request.id)
            {
                return Rebuilt::AskedAgain(Box::new(again));
            }
            if answering.quiesce(chat).await == Quiescence::Idle {
                return Rebuilt::Ran;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the rebuilt Run goes on");
    (answering, rebuilt)
}

/// A server reached at another address under the same name since the approval: the rebuilt Run
/// asks again instead of running the call there.
#[tokio::test]
async fn a_rebuilt_approval_is_asked_again_when_the_server_moved() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let first = McpService::start().await;
    let (tools, tool) = provider(&first, SERVER).await;
    let (chat, _, request) = ask_and_release(&store, tools, &tool).await;
    let moved = McpService::start().await;
    let (tools, _) = provider(&moved, SERVER).await;
    let (answering, rebuilt) = allowed_in_another_instance(&store, tools, chat, &request).await;
    assert!(
        matches!(&rebuilt, Rebuilt::AskedAgain(again) if again.name == request.name),
        "{rebuilt:?}"
    );
    assert!(first.calls().is_empty());
    assert!(moved.calls().is_empty());
    answering.shutdown().await;
}

/// A tool whose definition changed since the approval: the rebuilt Run asks again.
#[tokio::test]
async fn a_rebuilt_approval_is_asked_again_when_the_tool_changed() {
    use tickloop_harness_runtime::host_tools::HostToolProvider;
    let store = Arc::new(InMemoryTranscriptStore::new());
    let server = McpService::start().await;
    let (tools, tool) = provider(&server, SERVER).await;
    let (chat, _, request) = ask_and_release(&store, tools, &tool).await;
    let (tools, _) = provider(&server, SERVER).await;
    server.set_tools(json!([{
        "name": "read",
        "description": "Deletes every report it is given.",
        "inputSchema": {"type": "object", "properties": {}},
    }]));
    tools.reconnect_mcp_server(SERVER).await;
    let (answering, rebuilt) = allowed_in_another_instance(&store, tools, chat, &request).await;
    assert!(matches!(rebuilt, Rebuilt::AskedAgain(_)), "{rebuilt:?}");
    assert!(server.calls().is_empty());
    answering.shutdown().await;
}

/// Another account behind the host's login since the approval: the rebuilt Run asks again.
#[tokio::test]
async fn a_rebuilt_approval_is_asked_again_for_another_account() {
    use crate::mcp_host_logins::{FIRST_TOKEN, HostLogin, SECOND_TOKEN};
    let store = Arc::new(InMemoryTranscriptStore::new());
    let server = McpService::start().await;
    server.set_tools(json!([{"name": "read", "inputSchema": {"type": "object"}}]));
    let tools = host_login_tools(&server, &HostLogin::new("account-a", Some(FIRST_TOKEN))).await;
    let (chat, _, request) = ask_and_release(&store, tools, "mcp__remote__read").await;
    let tools = host_login_tools(&server, &HostLogin::new("account-b", Some(SECOND_TOKEN))).await;
    let (answering, rebuilt) = allowed_in_another_instance(&store, tools, chat, &request).await;
    assert!(matches!(rebuilt, Rebuilt::AskedAgain(_)), "{rebuilt:?}");
    assert!(server.calls().is_empty());
    answering.shutdown().await;
}

/// The same account's token rotated since the approval: the rebuilt Run runs the call, once.
#[tokio::test]
async fn a_rebuilt_approval_holds_across_a_token_rotation_of_the_same_account() {
    use crate::mcp_host_logins::{FIRST_TOKEN, HostLogin, SECOND_TOKEN};
    let store = Arc::new(InMemoryTranscriptStore::new());
    let server = McpService::start().await;
    server.set_tools(json!([{"name": "read", "inputSchema": {"type": "object"}}]));
    server.set_result(json!({"content": [{"type": "text", "text": "MCP_OK"}]}));
    server.require_bearer(FIRST_TOKEN);
    let tools = host_login_tools(&server, &HostLogin::new("account-a", Some(FIRST_TOKEN))).await;
    let (chat, _, request) = ask_and_release(&store, tools, "mcp__remote__read").await;
    server.require_bearer(SECOND_TOKEN);
    let tools = host_login_tools(&server, &HostLogin::new("account-a", Some(SECOND_TOKEN))).await;
    let (answering, rebuilt) = allowed_in_another_instance(&store, tools, chat, &request).await;
    assert!(matches!(rebuilt, Rebuilt::Ran), "{rebuilt:?}");
    assert_eq!(server.calls().len(), 1);
    let snapshot = answering.snapshot(chat).await.unwrap();
    assert_eq!(results(&snapshot.messages), [("MCP_OK".to_string(), false)]);
    answering.shutdown().await;
}

/// A host that cannot record the end of an allowed call's wait does not run the call; the host
/// that loads the Chat next finds the wait still open, asks again and runs the call then, so it
/// runs once in all.
#[tokio::test]
async fn an_allowed_call_runs_only_once_its_wait_end_is_recorded() {
    use tickloop_harness_test_support::transcripts::FaultyTranscriptStore;
    let store = Arc::new(InMemoryTranscriptStore::new());
    let failing = Arc::new(FaultyTranscriptStore::new(store.clone()));
    let server = McpService::start().await;
    let (tools, tool) = provider(&server, SERVER).await;
    let (unrecorded, _) = harness_on(failing.clone(), tools, vec![call(CALL, &tool, json!({}))]);
    let chat = unrecorded.create_chat_without_folder().await.unwrap();
    let turn = unrecorded
        .advance(chat, prompt("Read the service.", Delivery::Queue))
        .await
        .unwrap();
    let TurnOutcome::Waiting(TurnWait::Permission(request)) = outcome(&turn).await else {
        panic!("the turn waits for the person's approval");
    };
    unrecorded.sync().await.unwrap();
    let mut events = unrecorded.subscribe();
    failing.fail_appends();
    unrecorded
        .answer_permission(chat, request.id, true)
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let event = tickloop_harness_test_support::next_event(&mut events).await;
            if format!("{:?}", event.kind).contains("could not be recorded") {
                return;
            }
        }
    })
    .await
    .expect("the allowed call does not run");
    assert!(
        server.calls().is_empty(),
        "nothing runs before its wait's end is recorded"
    );
    unrecorded.shutdown().await;
    failing.heal();

    let (tools, _) = provider(&server, SERVER).await;
    let (next, _) = harness_on(store.clone(), tools, vec![text(DONE)]);
    let rebuilt = next.resume_chat(chat).await.unwrap();
    assert_eq!(
        rebuilt
            .permissions
            .iter()
            .map(|waiting| waiting.id)
            .collect::<Vec<_>>(),
        [request.id],
        "the wait is still open where the Chat loads next"
    );
    let reply = next
        .advance(chat, answer(&request, PermissionReply::AllowOnce))
        .await
        .unwrap();
    assert!(matches!(outcome(&reply).await, TurnOutcome::Merged { .. }));
    assert!(matches!(
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            next.settled_input(chat, turn.input_id())
        )
        .await
        .unwrap()
        .unwrap(),
        SettledOutcome::Completed { .. }
    ));
    assert_eq!(server.calls().len(), 1, "the call runs once in all");
    next.shutdown().await;
}

/// The tools of the server `remote` at `server`, reached with a login the Harness saved itself,
/// whose token is `token` and which names itself `login`.
async fn saved_login_tools(
    server: &McpService,
    token: &str,
    login: Option<&str>,
) -> Arc<tickloop_harness_runtime::mcp::McpToolProvider> {
    use tickloop_harness_runtime::mcp::{
        ConnectorCredential, ConnectorCredentials, HostCapabilities, InMemoryCredentials,
        McpConnections, McpServerState, McpToolProvider,
    };
    let credentials = Arc::new(InMemoryCredentials::default());
    credentials.replace(Some(ConnectorCredential {
        server_url: server.url(),
        registration: tickloop_harness_connector::ClientRegistration {
            authorization_server: server.url(),
            client_id: "fixture".into(),
            redirect_uri: "http://localhost/callback".into(),
        },
        resource: None,
        token: Some(
            serde_json::from_value(json!({
                "access_token": token,
                "token_type": "Bearer",
                "issuer": server.url(),
                "expires_at": u64::MAX,
                "client_id": "fixture",
                "token_endpoint": server.url(),
            }))
            .unwrap(),
        ),
        login: login.map(str::to_owned),
    }));
    let connections = McpConnections::new(
        HostCapabilities::default(),
        std::collections::BTreeMap::from([(
            "remote".to_owned(),
            credentials as Arc<dyn ConnectorCredentials>,
        )]),
        std::collections::BTreeMap::new(),
    );
    let configs = vec![crate::mcp_host_logins::remote(&server.url())];
    connections.reconcile(configs.clone()).await;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while connections.snapshot()[0].state != McpServerState::Connected {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the server connects");
    Arc::new(McpToolProvider::new(
        connections,
        Arc::new(move || configs.clone()),
    ))
}

/// The user logged in to the same server again, possibly as another account, before the
/// approval was answered: the rebuilt Run asks again instead of running the call under the new
/// login. A login that does not name itself cannot show it is the same, so it asks again too.
#[tokio::test]
async fn a_rebuilt_approval_is_asked_again_after_another_saved_login() {
    for (first, second) in [(Some("login-a"), Some("login-b")), (None, None)] {
        let store = Arc::new(InMemoryTranscriptStore::new());
        let server = McpService::start().await;
        server.set_tools(json!([{"name": "read", "inputSchema": {"type": "object"}}]));
        server.require_bearer("token-a");
        let tools = saved_login_tools(&server, "token-a", first).await;
        let (chat, _, request) = ask_and_release(&store, tools, "mcp__remote__read").await;
        server.require_bearer("token-b");
        let tools = saved_login_tools(&server, "token-b", second).await;
        let (answering, rebuilt) = allowed_in_another_instance(&store, tools, chat, &request).await;
        assert!(
            matches!(rebuilt, Rebuilt::AskedAgain(_)),
            "{first:?} then {second:?}: {rebuilt:?}"
        );
        assert!(server.calls().is_empty());
        answering.shutdown().await;
    }
}

/// The saved login's token was renewed before the approval was answered: the rebuilt Run runs
/// the call, once.
#[tokio::test]
async fn a_rebuilt_approval_holds_across_a_renewal_of_the_same_saved_login() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let server = McpService::start().await;
    server.set_tools(json!([{"name": "read", "inputSchema": {"type": "object"}}]));
    server.set_result(json!({"content": [{"type": "text", "text": "MCP_OK"}]}));
    server.require_bearer("token-a");
    let tools = saved_login_tools(&server, "token-a", Some("login-a")).await;
    let (chat, _, request) = ask_and_release(&store, tools, "mcp__remote__read").await;
    server.require_bearer("token-renewed");
    let tools = saved_login_tools(&server, "token-renewed", Some("login-a")).await;
    let (answering, rebuilt) = allowed_in_another_instance(&store, tools, chat, &request).await;
    assert!(matches!(rebuilt, Rebuilt::Ran), "{rebuilt:?}");
    assert_eq!(server.calls().len(), 1);
    answering.shutdown().await;
}

/// A saved login of the server `remote` at `server`: `token` from the login named `login`,
/// expiring at `expires_at`, in seconds since the Unix epoch.
fn saved_login(
    server: &McpService,
    token: &str,
    login: Option<&str>,
    expires_at: u64,
) -> tickloop_harness_runtime::mcp::ConnectorCredential {
    tickloop_harness_runtime::mcp::ConnectorCredential {
        server_url: server.url(),
        registration: tickloop_harness_connector::ClientRegistration {
            authorization_server: server.url(),
            client_id: "fixture".into(),
            redirect_uri: "http://localhost/callback".into(),
        },
        resource: None,
        token: Some(
            serde_json::from_value(json!({
                "access_token": token,
                "token_type": "Bearer",
                "issuer": server.url(),
                "expires_at": expires_at,
                "client_id": "fixture",
                "token_endpoint": server.url(),
            }))
            .unwrap(),
        ),
        login: login.map(str::to_owned),
    }
}

/// Saved logins whose reads start failing once `reads_left` is used up; negative never fails.
#[derive(Default)]
struct FailingReads {
    saved: tickloop_harness_runtime::mcp::InMemoryCredentials,
    reads_left: std::sync::atomic::AtomicI64,
}

#[async_trait::async_trait]
impl tickloop_harness_runtime::mcp::ConnectorCredentials for FailingReads {
    async fn lock(
        &self,
    ) -> Result<
        tickloop_harness_connector::CredentialLock,
        tickloop_harness_connector::CredentialError,
    > {
        self.saved.lock().await
    }

    async fn load(
        &self,
    ) -> Result<
        Option<tickloop_harness_runtime::mcp::ConnectorCredential>,
        tickloop_harness_connector::CredentialError,
    > {
        use std::sync::atomic::Ordering;
        if self.reads_left.load(Ordering::SeqCst) == 0 {
            return Err(tickloop_harness_connector::CredentialError("read"));
        }
        if self.reads_left.load(Ordering::SeqCst) > 0 {
            self.reads_left.fetch_sub(1, Ordering::SeqCst);
        }
        self.saved.load().await
    }

    async fn save(
        &self,
        credential: &tickloop_harness_runtime::mcp::ConnectorCredential,
    ) -> Result<(), tickloop_harness_connector::CredentialError> {
        self.saved.save(credential).await
    }
}

/// Connections to the server `remote` at `server` with the saved logins `credentials`. The
/// configuration is read again on each refresh, or only the first time when `reread` is off.
async fn saved_login_provider(
    server: &McpService,
    credentials: Arc<dyn tickloop_harness_runtime::mcp::ConnectorCredentials>,
    reread: bool,
) -> Arc<tickloop_harness_runtime::mcp::McpToolProvider> {
    saved_login_connections(server, credentials, reread).await.0
}

/// [`saved_login_provider`], with the connections it serves from.
async fn saved_login_connections(
    server: &McpService,
    credentials: Arc<dyn tickloop_harness_runtime::mcp::ConnectorCredentials>,
    reread: bool,
) -> (
    Arc<tickloop_harness_runtime::mcp::McpToolProvider>,
    tickloop_harness_runtime::mcp::McpConnections,
) {
    use tickloop_harness_runtime::mcp::{
        HostCapabilities, McpConfiguration, McpConnections, McpServerState, McpToolProvider,
    };
    let connections = McpConnections::new(
        HostCapabilities::default(),
        std::collections::BTreeMap::from([("remote".to_owned(), credentials)]),
        std::collections::BTreeMap::new(),
    );
    let configs = vec![crate::mcp_host_logins::remote(&server.url())];
    connections.reconcile(configs.clone()).await;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while connections.snapshot()[0].state != McpServerState::Connected {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the server connects");
    let read = std::sync::atomic::AtomicBool::new(false);
    let provider = Arc::new(McpToolProvider::with_configuration_updates(
        connections.clone(),
        Arc::new(move || {
            (reread || !read.swap(true, std::sync::atomic::Ordering::SeqCst)).then(|| {
                McpConfiguration {
                    servers: configs.clone(),
                    warnings: Vec::new(),
                }
            })
        }),
    ));
    (provider, connections)
}

fn now_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// While an approval waits, the expiring saved login is renewed, but the store meanwhile holds
/// another login and fails every read after the token's: the token of the other login is not
/// taken up under the approval's login, so the allowed call never goes out under it.
#[tokio::test]
async fn a_renewal_that_reads_another_login_does_not_carry_a_waiting_call() {
    use std::sync::atomic::Ordering;
    use tickloop_harness_runtime::host_tools::HostToolProvider;
    let store = Arc::new(InMemoryTranscriptStore::new());
    let server = McpService::start().await;
    server.set_tools(json!([{"name": "read", "inputSchema": {"type": "object"}}]));
    server.set_result(json!({"content": [{"type": "text", "text": "MCP_OK"}]}));
    server.require_bearer("token-a");
    let credentials = Arc::new(FailingReads::default());
    credentials.reads_left.store(-1, Ordering::SeqCst);
    credentials.saved.replace(Some(saved_login(
        &server,
        "token-a",
        Some("login-a"),
        now_seconds() + 10,
    )));
    let tools = saved_login_provider(&server, credentials.clone(), false).await;
    let (agent, _) = harness_on(
        store.clone(),
        tools.clone(),
        vec![call(CALL, "mcp__remote__read", json!({})), text(DONE)],
    );
    let chat = agent.create_chat_without_folder().await.unwrap();
    let turn = agent
        .advance(chat, prompt("Read the service.", Delivery::Queue))
        .await
        .unwrap();
    let TurnOutcome::Waiting(TurnWait::Permission(request)) = outcome(&turn).await else {
        panic!("the turn waits for the person's approval");
    };

    credentials.saved.replace(Some(saved_login(
        &server,
        "token-b",
        Some("login-b"),
        u64::MAX,
    )));
    server.require_bearer("token-b");
    credentials.reads_left.store(1, Ordering::SeqCst);
    tools.refresh(CancellationToken::new()).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while credentials.reads_left.load(Ordering::SeqCst) != 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the renewal reads the store");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    agent
        .answer_permission(chat, request.id, true)
        .await
        .unwrap();
    assert!(matches!(
        outcome(&turn).await,
        TurnOutcome::Completed { .. } | TurnOutcome::Interrupted { .. }
    ));
    let snapshot = agent.snapshot(chat).await.unwrap();
    let results = results(&snapshot.messages);
    assert_eq!(results.len(), 1);
    assert!(
        results[0].1,
        "the call is not carried by another login: {results:?}"
    );
    agent.shutdown().await;
}

/// While an approval waits, the store gets another saved login for the same server and
/// registration, named or not: the server's entry is replaced, and the allowed call, prepared
/// under the old login, does not go out.
#[tokio::test]
async fn a_saved_login_replaced_while_an_approval_waits_never_carries_the_call() {
    use tickloop_harness_runtime::host_tools::HostToolProvider;
    use tickloop_harness_runtime::mcp::{InMemoryCredentials, McpServerState};
    for (first, second) in [(Some("login-a"), Some("login-b")), (None, None)] {
        let store = Arc::new(InMemoryTranscriptStore::new());
        let server = McpService::start().await;
        server.set_tools(json!([{"name": "read", "inputSchema": {"type": "object"}}]));
        server.set_result(json!({"content": [{"type": "text", "text": "MCP_OK"}]}));
        server.require_bearer("token-a");
        let credentials = Arc::new(InMemoryCredentials::default());
        credentials.replace(Some(saved_login(&server, "token-a", first, u64::MAX)));
        let tools = saved_login_provider(&server, credentials.clone(), true).await;
        let (agent, _) = harness_on(
            store.clone(),
            tools.clone(),
            vec![call(CALL, "mcp__remote__read", json!({})), text(DONE)],
        );
        let chat = agent.create_chat_without_folder().await.unwrap();
        let turn = agent
            .advance(chat, prompt("Read the service.", Delivery::Queue))
            .await
            .unwrap();
        let TurnOutcome::Waiting(TurnWait::Permission(request)) = outcome(&turn).await else {
            panic!("the turn waits for the person's approval");
        };

        credentials.replace(Some(saved_login(&server, "token-b", second, u64::MAX)));
        server.require_bearer("token-b");
        tools.refresh(CancellationToken::new()).await.unwrap();
        let statuses = tools.mcp_server_statuses(CancellationToken::new()).await;
        assert_eq!(statuses[0].state, McpServerState::Connected, "{first:?}");

        agent
            .answer_permission(chat, request.id, true)
            .await
            .unwrap();
        assert!(matches!(
            outcome(&turn).await,
            TurnOutcome::Completed { .. } | TurnOutcome::Interrupted { .. }
        ));
        assert!(
            server.calls().is_empty(),
            "{first:?} then {second:?}: the call went out under the other login"
        );
        agent.shutdown().await;
    }
}

/// While an approval waits, the server's connection drops and connects again with another saved
/// login, named or not: the allowed call, prepared under the old login, does not go out.
#[tokio::test]
async fn a_connection_made_again_with_another_saved_login_never_carries_a_waiting_call() {
    use tickloop_harness_runtime::host_tools::HostToolProvider;
    use tickloop_harness_runtime::mcp::{InMemoryCredentials, McpServerState};
    for (first, second) in [(Some("login-a"), Some("login-b")), (None, None)] {
        let store = Arc::new(InMemoryTranscriptStore::new());
        let server = McpService::start().await;
        server.set_tools(json!([
            {"name": "read", "inputSchema": {"type": "object"}},
            {"name": "ping", "inputSchema": {"type": "object"}},
        ]));
        server.set_result(json!({"content": [{"type": "text", "text": "MCP_OK"}]}));
        server.require_bearer("token-a");
        let credentials = Arc::new(InMemoryCredentials::default());
        credentials.replace(Some(saved_login(&server, "token-a", first, u64::MAX)));
        let tools = saved_login_provider(&server, credentials.clone(), false).await;
        let (agent, _) = harness_on(
            store.clone(),
            tools.clone(),
            vec![call(CALL, "mcp__remote__read", json!({})), text(DONE)],
        );
        let chat = agent.create_chat_without_folder().await.unwrap();
        let turn = agent
            .advance(chat, prompt("Read the service.", Delivery::Queue))
            .await
            .unwrap();
        let TurnOutcome::Waiting(TurnWait::Permission(request)) = outcome(&turn).await else {
            panic!("the turn waits for the person's approval");
        };

        // The connection breaks; the server connects again with what the store holds now.
        credentials.replace(Some(saved_login(&server, "token-b", second, u64::MAX)));
        server.set_status(500);
        let _ = tools
            .execute(
                chat,
                "mcp__remote__ping",
                json!({}),
                CancellationToken::new(),
            )
            .await;
        server.set_status(200);
        server.require_bearer("token-b");
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let statuses = tools.mcp_server_statuses(CancellationToken::new()).await;
                if statuses[0].state == McpServerState::Connected
                    && credentials
                        .current()
                        .unwrap()
                        .token
                        .as_ref()
                        .unwrap()
                        .access_token
                        == "token-b"
                    && tools
                        .execute(
                            chat,
                            "mcp__remote__ping",
                            json!({}),
                            CancellationToken::new(),
                        )
                        .await
                        .content
                        .contains("MCP_OK")
                {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("the server connects again with the other login");

        agent
            .answer_permission(chat, request.id, true)
            .await
            .unwrap();
        assert!(matches!(
            outcome(&turn).await,
            TurnOutcome::Completed { .. } | TurnOutcome::Interrupted { .. }
        ));
        let reads = server
            .calls()
            .iter()
            .filter(|call| call["params"]["name"] == "read")
            .count();
        assert_eq!(
            reads, 0,
            "{first:?} then {second:?}: the call went out under the other login"
        );
        agent.shutdown().await;
    }
}

/// A renewal begun under one saved login is held while the connection drops and connects again
/// under another account's login; let go, it neither puts its token on the new connection nor
/// lets the waiting call go out.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_renewal_held_across_a_reconnect_with_another_login_leaves_the_new_connection_alone() {
    use tickloop_harness_runtime::host_tools::HostToolProvider;
    use tickloop_harness_runtime::mcp::InMemoryCredentials;
    use tickloop_harness_runtime::mcp::test_pauses::{PausePoint, pause_next};
    let store = Arc::new(InMemoryTranscriptStore::new());
    let server = McpService::start().await;
    server.set_tools(json!([
        {"name": "read", "inputSchema": {"type": "object"}},
        {"name": "ping", "inputSchema": {"type": "object"}},
    ]));
    server.set_result(json!({"content": [{"type": "text", "text": "MCP_OK"}]}));
    server.require_bearer("token-a");
    let credentials = Arc::new(InMemoryCredentials::default());
    credentials.replace(Some(saved_login(
        &server,
        "token-a",
        Some("login-a"),
        now_seconds() + 10,
    )));
    let tools = saved_login_provider(&server, credentials.clone(), false).await;
    let (agent, _) = harness_on(
        store.clone(),
        tools.clone(),
        vec![call(CALL, "mcp__remote__read", json!({})), text(DONE)],
    );
    let chat = agent.create_chat_without_folder().await.unwrap();
    let turn = agent
        .advance(chat, prompt("Read the service.", Delivery::Queue))
        .await
        .unwrap();
    let TurnOutcome::Waiting(TurnWait::Permission(request)) = outcome(&turn).await else {
        panic!("the turn waits for the person's approval");
    };
    let ping = || {
        let tools = tools.clone();
        async move {
            tools
                .execute(
                    chat,
                    "mcp__remote__ping",
                    json!({}),
                    CancellationToken::new(),
                )
                .await
        }
    };

    // The expiring login's renewal reads its token and is held there.
    let pause = pause_next(PausePoint::Renewal, "remote");
    tools.refresh(CancellationToken::new()).await.unwrap();
    let entered = pause.entered;
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::task::spawn_blocking(move || entered.recv()),
    )
    .await
    .expect("the renewal reaches its token")
    .unwrap()
    .unwrap();

    // Meanwhile the connection drops and connects again with another account's login.
    credentials.replace(Some(saved_login(
        &server,
        "token-b",
        Some("login-b"),
        u64::MAX,
    )));
    server.set_status(500);
    let _ = ping().await;
    server.set_status(200);
    server.require_bearer("token-b");
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !ping().await.content.contains("MCP_OK") {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the server connects again with the other login");

    pause.release.send(()).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let after = ping().await;
    assert!(
        after.content.contains("MCP_OK") && !after.is_error,
        "the new connection keeps the other login's token: {after:?}"
    );

    agent
        .answer_permission(chat, request.id, true)
        .await
        .unwrap();
    assert!(matches!(
        outcome(&turn).await,
        TurnOutcome::Completed { .. } | TurnOutcome::Interrupted { .. }
    ));
    let reads = server
        .calls()
        .iter()
        .filter(|call| call["params"]["name"] == "read")
        .count();
    assert_eq!(
        reads, 0,
        "the call allowed under the first login never goes out"
    );
    assert!(ping().await.content.contains("MCP_OK"));
    agent.shutdown().await;
}

/// Where a stale write of the first login waits while the connection takes up the second.
#[derive(Clone, Copy, Debug)]
enum StaleWrite {
    /// A reconcile that found the first login renewed in the store.
    Reconcile,
    /// Remembering the first login as after its token could not be used.
    Invalidation,
}

/// A write of the first saved login that found it still the entry's own is held while the
/// connection drops and connects again with another account's login; let go, it leaves the
/// entry with the second login, so the call allowed under the first never goes out under it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stale_write_of_the_first_login_never_sets_back_the_second() {
    use tickloop_harness_runtime::host_tools::HostToolProvider;
    use tickloop_harness_runtime::mcp::InMemoryCredentials;
    use tickloop_harness_runtime::mcp::test_pauses::{PausePoint, pause_next};
    for stale in [StaleWrite::Reconcile, StaleWrite::Invalidation] {
        let store = Arc::new(InMemoryTranscriptStore::new());
        let server = McpService::start().await;
        server.set_tools(json!([
            {"name": "read", "inputSchema": {"type": "object"}},
            {"name": "ping", "inputSchema": {"type": "object"}},
        ]));
        server.set_result(json!({"content": [{"type": "text", "text": "MCP_OK"}]}));
        server.require_bearer("token-a");
        let credentials = Arc::new(InMemoryCredentials::default());
        credentials.replace(Some(saved_login(
            &server,
            "token-a",
            Some("login-a"),
            u64::MAX,
        )));
        let (tools, connections) =
            saved_login_connections(&server, credentials.clone(), false).await;
        let (agent, _) = harness_on(
            store.clone(),
            tools.clone(),
            vec![call(CALL, "mcp__remote__read", json!({})), text(DONE)],
        );
        let chat = agent.create_chat_without_folder().await.unwrap();
        let turn = agent
            .advance(chat, prompt("Read the service.", Delivery::Queue))
            .await
            .unwrap();
        let TurnOutcome::Waiting(TurnWait::Permission(request)) = outcome(&turn).await else {
            panic!("the turn waits for the person's approval");
        };
        let ping = || {
            let tools = tools.clone();
            async move {
                tools
                    .execute(
                        chat,
                        "mcp__remote__ping",
                        json!({}),
                        CancellationToken::new(),
                    )
                    .await
            }
        };

        // The first login's write finds it still the entry's own and is held there.
        let held = match stale {
            StaleWrite::Reconcile => {
                credentials.replace(Some(saved_login(
                    &server,
                    "token-a-renewed",
                    Some("login-a"),
                    u64::MAX,
                )));
                let pause = pause_next(PausePoint::Reconcile, "remote");
                let configs = vec![crate::mcp_host_logins::remote(&server.url())];
                let reconciling = connections.clone();
                let task = tokio::spawn(async move { reconciling.reconcile(configs).await });
                (pause, task)
            }
            StaleWrite::Invalidation => {
                let mut forgotten = saved_login(&server, "token-a", Some("login-a"), u64::MAX);
                forgotten.token = None;
                credentials.replace(Some(forgotten));
                let pause = pause_next(PausePoint::Invalidation, "remote");
                let invalidating = connections.clone();
                let task =
                    tokio::spawn(
                        async move { invalidating.remember_invalidation_of("remote").await },
                    );
                (pause, task)
            }
        };
        let (pause, task) = held;
        let entered = pause.entered;
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tokio::task::spawn_blocking(move || entered.recv()),
        )
        .await
        .expect("the stale write reaches its pause")
        .unwrap()
        .unwrap();

        // Meanwhile the connection drops and connects again with another account's login.
        credentials.replace(Some(saved_login(
            &server,
            "token-b",
            Some("login-b"),
            u64::MAX,
        )));
        server.set_status(500);
        let _ = ping().await;
        server.set_status(200);
        server.require_bearer("token-b");
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !ping().await.content.contains("MCP_OK") {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("the server connects again with the other login");

        pause.release.send(()).unwrap();
        task.await.unwrap();
        assert_eq!(
            connections
                .saved_login_of("remote")
                .and_then(|credential| credential.login)
                .as_deref(),
            Some("login-b"),
            "{stale:?}: the entry keeps the second login"
        );

        agent
            .answer_permission(chat, request.id, true)
            .await
            .unwrap();
        assert!(matches!(
            outcome(&turn).await,
            TurnOutcome::Completed { .. } | TurnOutcome::Interrupted { .. }
        ));
        let reads = server
            .calls()
            .iter()
            .filter(|call| call["params"]["name"] == "read")
            .count();
        assert_eq!(
            reads, 0,
            "{stale:?}: the call allowed under the first login went out"
        );
        agent.shutdown().await;
    }
}

/// Where the first login's failure is found before the connection moves to the second login.
#[derive(Clone, Copy, Debug)]
enum LoginFailure {
    /// The renewal of the expiring first login finds it can no longer be renewed.
    Renewal,
    /// The server rejects the first login's token and it cannot be renewed.
    Rejection,
}

/// The first login fails, and while its failure is being remembered the connection drops and
/// connects again with another account's login: the late failure leaves the new connection
/// connected instead of waiting for a login.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_late_login_failure_leaves_a_connection_made_again_with_another_login_alone() {
    use tickloop_harness_runtime::host_tools::HostToolProvider;
    use tickloop_harness_runtime::mcp::test_pauses::{PausePoint, pause_next};
    use tickloop_harness_runtime::mcp::{InMemoryCredentials, McpServerState};
    for failure in [LoginFailure::Renewal, LoginFailure::Rejection] {
        let server = McpService::start().await;
        server.set_tools(json!([{"name": "ping", "inputSchema": {"type": "object"}}]));
        server.set_result(json!({"content": [{"type": "text", "text": "MCP_OK"}]}));
        server.require_bearer("token-a");
        let credentials = Arc::new(InMemoryCredentials::default());
        let expires_at = match failure {
            LoginFailure::Renewal => now_seconds() + 10,
            LoginFailure::Rejection => u64::MAX,
        };
        credentials.replace(Some(saved_login(
            &server,
            "token-a",
            Some("login-a"),
            expires_at,
        )));
        let (tools, connections) =
            saved_login_connections(&server, credentials.clone(), false).await;
        let ping = || {
            let connections = connections.clone();
            async move {
                connections
                    .call("mcp__remote__ping", json!({}), CancellationToken::new())
                    .await
            }
        };

        // The first login fails, and remembering its failure is held.
        let pause = pause_next(PausePoint::Invalidation, "remote");
        let failing = match failure {
            LoginFailure::Renewal => {
                credentials.replace(Some(saved_login(&server, "token-a", Some("login-a"), 0)));
                tools.refresh(CancellationToken::new()).await.unwrap();
                None
            }
            LoginFailure::Rejection => {
                server.require_bearer("token-refused");
                Some(tokio::spawn(ping()))
            }
        };
        let entered = pause.entered;
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tokio::task::spawn_blocking(move || entered.recv()),
        )
        .await
        .expect("the failure is being remembered")
        .unwrap()
        .unwrap();

        // Meanwhile the connection drops and connects again with another account's login.
        server.require_bearer("token-b");
        credentials.replace(Some(saved_login(
            &server,
            "token-b",
            Some("login-b"),
            u64::MAX,
        )));
        connections.drop_connection_of("remote");
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !matches!(
                ping().await,
                tickloop_harness_runtime::host_tools::HostCallResult::Completed(_)
            ) {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "{failure:?}: never connected again with the other login: {:?}",
                connections.statuses()
            )
        });

        pause.release.send(()).unwrap();
        if let Some(failing) = failing {
            let _ = failing.await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(
            connections.snapshot()[0].state,
            McpServerState::Connected,
            "{failure:?}: the late failure leaves the new connection alone"
        );
        assert!(
            matches!(
                ping().await,
                tickloop_harness_runtime::host_tools::HostCallResult::Completed(_)
            ),
            "{failure:?}"
        );
        connections.shutdown().await;
    }
}

/// The tools of the server `review` at `server`, whose unruled calls `unruled` admits; the host
/// may change that choice between refreshes.
async fn admission_provider(
    server: &McpService,
    unruled: Arc<Mutex<tickloop_harness_protocol::mcp::McpUnruled>>,
) -> Arc<tickloop_harness_runtime::mcp::McpToolProvider> {
    use tickloop_harness_protocol::mcp::{McpServerConfig, McpTransport, ToolExposure};
    use tickloop_harness_runtime::mcp::{
        HostCapabilities, McpConnections, McpServerState, McpToolProvider,
    };
    let url = server.url();
    let config = move || McpServerConfig {
        unruled: *unruled.lock().unwrap(),
        exposure: ToolExposure::Direct,
        tool_exposure: Vec::new(),
        description: String::new(),
        oauth: Default::default(),
        sensitive_values: Vec::new(),
        disabled: false,
        name: SERVER.into(),
        source: "global".into(),
        transport: McpTransport::Http {
            url: url.clone(),
            headers: std::collections::BTreeMap::new(),
        },
        timeout_ms: 300_000,
    };
    let connections = McpConnections::new(
        HostCapabilities::default(),
        std::collections::BTreeMap::new(),
        std::collections::BTreeMap::new(),
    );
    connections.reconcile(vec![config()]).await;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while connections.snapshot()[0].state != McpServerState::Connected {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the server connects");
    Arc::new(McpToolProvider::new(
        connections,
        Arc::new(move || vec![config()]),
    ))
}

/// A Chat with a Folder under Durable Approval, whose server `review` admits its unruled calls
/// as `unruled` says, whose model makes `replies` and whose reviewer answers `verdicts`.
struct Reviewed {
    agent: Agent,
    chat: ChatId,
    reviewer: Arc<ScriptedModel>,
    unruled: Arc<Mutex<tickloop_harness_protocol::mcp::McpUnruled>>,
    _folder: tempfile::TempDir,
}

async fn reviewed(
    server: &McpService,
    unruled: tickloop_harness_protocol::mcp::McpUnruled,
    replies: Vec<Vec<Content>>,
    verdicts: Vec<Vec<Content>>,
) -> Reviewed {
    reviewed_in(server, unruled, replies, verdicts, true).await
}

/// [`reviewed`], in a Chat with a Folder or, without `with_folder`, without one.
async fn reviewed_in(
    server: &McpService,
    unruled: tickloop_harness_protocol::mcp::McpUnruled,
    replies: Vec<Vec<Content>>,
    verdicts: Vec<Vec<Content>>,
    with_folder: bool,
) -> Reviewed {
    let unruled = Arc::new(Mutex::new(unruled));
    let tools = admission_provider(server, unruled.clone()).await;
    let mut options = options(PromptPolicy::DurableApproval);
    options.host.directory_trust = tickloop_harness_protocol::DirectoryTrustPolicy::CallerManaged;
    let agent = Agent::with_seams(
        HarnessSeams {
            search: None,
            transcripts: Some(Arc::new(InMemoryTranscriptStore::new())),
            environment: with_folder.then(|| {
                Arc::new(tickloop_harness_runtime::environment::FolderEnvironment::bare())
                    as Arc<dyn tickloop_harness_runtime::environment::ExecutionEnvironment>
            }),
            models: ChosenModels::new(Arc::new(ScriptedModel::new(replies))),
        },
        options,
    )
    .unwrap();
    agent.install_host_tools(tools).unwrap();
    let reviewer = Arc::new(ScriptedModel::new(verdicts));
    agent.set_auto_reviewer(reviewer.clone()).await;
    let folder = tempfile::tempdir().unwrap();
    let chat = match with_folder {
        true => agent.create_chat(folder.path().into()).await.unwrap(),
        false => agent.create_chat_without_folder().await.unwrap(),
    };
    Reviewed {
        agent,
        chat,
        reviewer,
        unruled,
        _folder: folder,
    }
}

fn verdict(block: bool) -> Vec<Content> {
    text(&tickloop_harness_test_support::auto_classification(block).to_string())
}

/// A server whose host sends unruled calls to Auto review first: a call the reviewer allows
/// runs at once, with no one asked, in a Chat with a Folder and in one without: a remote call
/// touches no local file, so it is reviewed without a working directory.
#[tokio::test]
async fn an_auto_review_server_runs_what_the_reviewer_allows_without_asking() {
    for with_folder in [true, false] {
        use tickloop_harness_protocol::mcp::McpUnruled;
        let server = McpService::start().await;
        server.set_tools(json!([{"name": "read", "inputSchema": {"type": "object"}}]));
        server.set_result(json!({"content": [{"type": "text", "text": "MCP_OK"}]}));
        let Reviewed {
            agent,
            chat,
            reviewer,
            _folder,
            ..
        } = reviewed_in(
            &server,
            McpUnruled::AutoReview,
            vec![call(CALL, "mcp__review__read", json!({})), text(DONE)],
            vec![verdict(false)],
            with_folder,
        )
        .await;
        let admissions = watch_admissions(&agent);
        let turn = agent
            .advance(chat, prompt("Read the service.", Delivery::Queue))
            .await
            .unwrap();
        assert!(matches!(
            outcome(&turn).await,
            TurnOutcome::Completed { .. }
        ));
        assert_eq!(server.calls().len(), 1);
        assert_eq!(
            reviewer.requests.lock().unwrap().len(),
            1,
            "the call was reviewed"
        );
        assert!(
            !admissions
                .lock()
                .unwrap()
                .iter()
                .any(|admission| admission.starts_with("asked")),
            "{:?}",
            admissions.lock().unwrap()
        );
        // The reviewer reads the server's declaration of the tool, and no working directory where
        // the Chat has none.
        let reviewed = format!("{:?}", reviewer.requests.lock().unwrap()[0].messages);
        assert!(reviewed.contains("mcp_server"), "{reviewed}");
        assert_eq!(
            reviewed.contains("working_directory"),
            with_folder,
            "{reviewed}"
        );
        agent.shutdown().await;
    }
}

/// A call Auto review blocks on such a server waits for the person, and runs once allowed.
#[tokio::test]
async fn an_auto_review_server_asks_the_person_what_the_reviewer_blocks() {
    for with_folder in [true, false] {
        use tickloop_harness_protocol::mcp::McpUnruled;
        let server = McpService::start().await;
        server.set_tools(json!([{"name": "read", "inputSchema": {"type": "object"}}]));
        server.set_result(json!({"content": [{"type": "text", "text": "MCP_OK"}]}));
        let Reviewed {
            agent,
            chat,
            reviewer,
            _folder,
            ..
        } = reviewed_in(
            &server,
            McpUnruled::AutoReview,
            vec![call(CALL, "mcp__review__read", json!({})), text(DONE)],
            vec![verdict(true), verdict(true)],
            with_folder,
        )
        .await;
        let turn = agent
            .advance(chat, prompt("Read the service.", Delivery::Queue))
            .await
            .unwrap();
        let TurnOutcome::Waiting(TurnWait::Permission(request)) = outcome(&turn).await else {
            panic!("a blocked call waits for the person");
        };
        assert!(server.calls().is_empty());
        let reviews = reviewer.requests.lock().unwrap().len();
        assert!(reviews >= 1, "the call was reviewed");
        let reply = agent
            .advance(chat, answer(&request, PermissionReply::AllowOnce))
            .await
            .unwrap();
        assert!(matches!(outcome(&reply).await, TurnOutcome::Merged { .. }));
        assert!(matches!(
            outcome(&turn).await,
            TurnOutcome::Completed { .. }
        ));
        assert_eq!(server.calls().len(), 1, "the allowed call ran once");
        assert_eq!(
            reviewer.requests.lock().unwrap().len(),
            reviews,
            "the allowed call is not reviewed again"
        );
        agent.shutdown().await;
    }
}

/// A server whose host sends unruled calls to the person never reaches the reviewer.
#[tokio::test]
async fn a_person_server_never_reaches_the_reviewer() {
    use tickloop_harness_protocol::mcp::McpUnruled;
    let server = McpService::start().await;
    server.set_tools(json!([{"name": "read", "inputSchema": {"type": "object"}}]));
    let Reviewed {
        agent,
        chat,
        reviewer,
        _folder,
        ..
    } = reviewed(
        &server,
        McpUnruled::Person,
        vec![call(CALL, "mcp__review__read", json!({})), text(DONE)],
        vec![verdict(false)],
    )
    .await;
    let turn = agent
        .advance(chat, prompt("Read the service.", Delivery::Queue))
        .await
        .unwrap();
    assert!(matches!(
        outcome(&turn).await,
        TurnOutcome::Waiting(TurnWait::Permission(_))
    ));
    assert!(reviewer.requests.lock().unwrap().is_empty());
    assert!(server.calls().is_empty());
    agent.shutdown().await;
}

/// Changing who admits a server's unruled calls keeps its connection, and applies from the next
/// call.
#[tokio::test]
async fn changing_who_admits_unruled_calls_keeps_the_connection() {
    use tickloop_harness_protocol::mcp::McpUnruled;
    let server = McpService::start().await;
    server.set_tools(json!([{"name": "read", "inputSchema": {"type": "object"}}]));
    server.set_result(json!({"content": [{"type": "text", "text": "MCP_OK"}]}));
    let Reviewed {
        agent,
        chat,
        reviewer,
        unruled,
        _folder,
    } = reviewed(
        &server,
        McpUnruled::Person,
        vec![
            call(CALL, "mcp__review__read", json!({})),
            text(DONE),
            call("again", "mcp__review__read", json!({})),
            text(DONE),
        ],
        vec![verdict(false)],
    )
    .await;
    let turn = agent
        .advance(chat, prompt("Read the service.", Delivery::Queue))
        .await
        .unwrap();
    let TurnOutcome::Waiting(TurnWait::Permission(request)) = outcome(&turn).await else {
        panic!("the person admits the first call");
    };
    let reply = agent
        .advance(
            chat,
            answer(
                &request,
                PermissionReply::Deny {
                    comment: Some("Not now.".into()),
                },
            ),
        )
        .await
        .unwrap();
    outcome(&reply).await;
    assert!(matches!(
        outcome(&turn).await,
        TurnOutcome::Completed { .. }
    ));
    let initializations = server.arrivals("initialize").len();

    *unruled.lock().unwrap() = McpUnruled::AutoReview;
    let turn = agent
        .advance(chat, prompt("Read it again.", Delivery::Queue))
        .await
        .unwrap();
    assert!(matches!(
        outcome(&turn).await,
        TurnOutcome::Completed { .. }
    ));
    assert_eq!(server.calls().len(), 1, "the second call ran after review");
    assert_eq!(reviewer.requests.lock().unwrap().len(), 1);
    assert_eq!(
        server.arrivals("initialize").len(),
        initializations,
        "the server kept its connection"
    );
    agent.shutdown().await;
}
