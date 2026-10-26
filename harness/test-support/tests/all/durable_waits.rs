//! The minimal hold loop with durable waits: a holding asks a question and releases its Run
//! suspended, and a later holding on another Harness delivers the reply. The Run goes on there
//! with the same model requests as a Run answered in one process.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tickloop_harness_base::network::Reach;
use tickloop_harness_protocol::{
    AgentHostOptions, Content, Decision, PermissionRule, PromptPolicy, QuestionAnswer,
    QuestionRequest,
};
use tickloop_harness_runtime::transcript::TranscriptStore;
use tickloop_harness_runtime::{
    Agent, Delivery, HarnessOptions, HarnessSeams, ParentSelection, PersonWaitKind, QuestionReply,
    ToolPolicy, TurnOp, TurnOutcome, TurnRequest, TurnWait, UserMessage, WaitHosting,
};
use tickloop_harness_test_support::event_stream::MemoryEventStream;
use tickloop_harness_test_support::harness::ChosenModels;
use tickloop_harness_test_support::stateless_host::{
    CrashPoint, DurableChats, Halt, HostConfig, ManualClock, ReferenceHost,
};
use tickloop_harness_test_support::transcripts::InMemoryTranscriptStore;
use tickloop_harness_test_support::{ScriptedModel, config, single_question, text};
use tickloop_harness_transcript::{Line, WaitKind};
use uuid::Uuid;

const PROMPT: &str = "Prepare the report.";
const DONE: &str = "Report ready.";
const PAGE_SUMMARY: &str = "Delivery is on Friday.";

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

/// A cloud Harness whose Chats may ask questions, with durable waits.
fn durable_harness(store: Arc<dyn TranscriptStore>, model: Arc<ScriptedModel>) -> Agent {
    Agent::with_seams(
        HarnessSeams {
            search: None,
            transcripts: Some(store),
            environment: None,
            models: ChosenModels::new(model),
        },
        HarnessOptions {
            host: AgentHostOptions {
                prompt_policy: PromptPolicy::DenyOnPrompt,
                ..AgentHostOptions::product_defaults()
            },
            tools: ToolPolicy {
                questions: true,
                folderless_reach: Reach::Anywhere,
                ..ToolPolicy::web_fetch_only()
            },
            host_rules: vec![allow_web_fetch()],
            waits: WaitHosting::Durable,
            ..HarnessOptions::local(config())
        },
    )
    .unwrap()
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

fn ask(id: &str) -> Content {
    Content::ToolUse {
        id: id.into(),
        name: "AskUserQuestion".into(),
        input: single_question(&format!("Which format for {id}?")),
    }
}

fn fetch(id: &str, url: &str) -> Content {
    Content::ToolUse {
        id: id.into(),
        name: "WebFetch".into(),
        input: json!({"url": url, "prompt": "When?"}),
    }
}

/// The model's replies: the batch, the page summary for each fetch in it, then the answer.
fn script(batch: &[Content]) -> Vec<Vec<Content>> {
    let fetches = batch
        .iter()
        .filter(|content| matches!(content, Content::ToolUse { name, .. } if name == "WebFetch"))
        .count();
    std::iter::once(batch.to_vec())
        .chain((0..fetches).map(|_| text(PAGE_SUMMARY)))
        .chain(std::iter::once(text(DONE)))
        .collect()
}

fn prompt() -> TurnRequest {
    TurnRequest {
        input_id: Uuid::new_v4().into(),
        op: TurnOp::Continue {
            after: ParentSelection::Root,
            message: UserMessage {
                text: PROMPT.into(),
                ..Default::default()
            },
        },
        delivery: Delivery::SendNow,
        host_context: None,
    }
}

fn answer(request: &QuestionRequest) -> TurnRequest {
    TurnRequest {
        input_id: Uuid::new_v4().into(),
        op: TurnOp::Answer {
            request: request.id,
            reply: QuestionReply::Answered(vec![QuestionAnswer::other(format!(
                "Answer to {}",
                request.call_id
            ))]),
        },
        delivery: Delivery::Queue,
        host_context: None,
    }
}

/// The model requests as text with ids replaced by their order of appearance and the times the
/// host state reports zeroed: two Chats making the same requests differ in nothing else.
fn normalized_requests(model: &ScriptedModel) -> Vec<String> {
    model
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|request| normalized(&serde_json::to_string(&request.messages).unwrap()))
        .collect()
}

pub(super) fn normalized(text: &str) -> String {
    const UUID_LENGTH: usize = 36;
    let text = tickloop_harness_test_support::model_ids_as_uuids(text);
    let mut ids = Vec::new();
    let mut normalized = String::new();
    let mut rest = text.as_str();
    while !rest.is_empty() {
        match rest
            .get(..UUID_LENGTH)
            .filter(|candidate| candidate.as_bytes()[8] == b'-')
            .and_then(|candidate| Uuid::parse_str(candidate).ok())
        {
            Some(id) => {
                let order = ids.iter().position(|seen| *seen == id).unwrap_or_else(|| {
                    ids.push(id);
                    ids.len() - 1
                });
                normalized.push_str(&format!("id{order}"));
                rest = &rest[UUID_LENGTH..];
            }
            None => {
                let character = rest.chars().next().unwrap();
                normalized.push(character);
                rest = &rest[character.len_utf8()..];
            }
        }
    }
    for field in ["\\\"run_started_at\\\":", "\\\"state_since\\\":"] {
        let mut rest = normalized.as_str();
        let mut zeroed = String::new();
        while let Some(position) = rest.find(field) {
            let value_start = position + field.len();
            zeroed.push_str(&rest[..value_start]);
            zeroed.push('0');
            rest = rest[value_start..]
                .trim_start_matches(|character: char| character.is_ascii_digit());
        }
        zeroed.push_str(rest);
        normalized = zeroed;
    }
    normalized
}

async fn outcome(turn: &tickloop_harness_runtime::Turn) -> TurnOutcome {
    tokio::time::timeout(Duration::from_secs(10), turn.outcome())
        .await
        .expect("the turn reaches an outcome")
        .unwrap()
}

/// The batch answered in one process; gives the questions it asked and its model requests.
async fn in_process(batch: &[Content]) -> (Vec<String>, Vec<String>) {
    let model = Arc::new(ScriptedModel::new(script(batch)));
    let agent = durable_harness(Arc::new(InMemoryTranscriptStore::new()), model.clone());
    let chat = agent.create_chat_without_folder().await.unwrap();
    let turn = agent.advance(chat, prompt()).await.unwrap();
    let mut asked = Vec::new();
    loop {
        match outcome(&turn).await {
            TurnOutcome::Waiting(TurnWait::Question(request)) => {
                asked.push(request.call_id.clone());
                let reply = agent.advance(chat, answer(&request)).await.unwrap();
                assert!(matches!(outcome(&reply).await, TurnOutcome::Merged { .. }));
            }
            TurnOutcome::Completed { .. } => break,
            other => panic!("{other:?}"),
        }
    }
    agent.shutdown().await;
    (asked, normalized_requests(&model))
}

/// The question a recorded wait is on.
fn recorded_question(chats: &DurableChats, chat: Uuid, wait: Uuid) -> QuestionRequest {
    chats
        .history(chat)
        .lines
        .into_iter()
        .rev()
        .find_map(|line| match line {
            Line::Wait(entry) if entry.wait_id == wait => match entry.kind {
                WaitKind::Question { request, .. } => Some(request),
                _ => None,
            },
            _ => None,
        })
        .expect("the question's wait is recorded")
}

/// The batch asked in one holding and every question answered in a holding of its own, each on
/// a fresh Harness of one of two hosts; gives the questions asked and the model requests.
async fn across_holdings(batch: &[Content]) -> (Vec<String>, Vec<String>) {
    let clock = ManualClock::new();
    let chats = DurableChats::new(clock.clone());
    let stream = Arc::new(MemoryEventStream::new());
    let model = Arc::new(ScriptedModel::new(script(batch)));
    let hosts = ["a", "b"].map(|holder| {
        let model = model.clone();
        ReferenceHost::new(
            holder,
            chats.clone(),
            stream.clone(),
            clock.clone(),
            HostConfig::default(),
            move |store| durable_harness(store, model.clone()),
        )
    });
    let chat = Uuid::new_v4();
    let mut report = hosts[0].deliver(chat, prompt()).await.unwrap();
    assert!(matches!(
        report.outcome,
        Some(TurnOutcome::Waiting(TurnWait::Question(_)))
    ));
    let mut asked = Vec::new();
    loop {
        assert!(report.acknowledged, "{report:?}");
        assert_eq!(chats.waiting(chat), report.waiting);
        let Some(waits) = report.waiting.clone() else {
            break;
        };
        let [person] = waits.people.as_slice() else {
            panic!("one question waits: {waits:?} after {asked:?}: {report:?}");
        };
        assert_eq!(person.kind, PersonWaitKind::Question);
        assert_eq!(Some(person.chat), report.harness_chat);
        let request = recorded_question(&chats, chat, person.request);
        asked.push(request.call_id.clone());
        report = hosts[asked.len() % 2]
            .deliver(chat, answer(&request))
            .await
            .unwrap();
        assert!(
            matches!(report.outcome, Some(TurnOutcome::Merged { .. })),
            "{report:?}"
        );
    }
    (asked, normalized_requests(&model))
}

async fn assert_across_holdings_as_in_process(batch: Vec<Content>) {
    let (asked_in_process, in_process) = in_process(&batch).await;
    let (asked, requests) = across_holdings(&batch).await;
    assert_eq!(asked, asked_in_process);
    assert_eq!(requests.len(), in_process.len());
    for (index, (across, local)) in requests.iter().zip(&in_process).enumerate() {
        assert_eq!(across, local, "request {index} differs");
    }
}

#[tokio::test]
async fn a_question_first_in_its_batch_is_answered_in_a_later_holding_as_in_process() {
    let url = page_server().await;
    assert_across_holdings_as_in_process(vec![ask("q"), fetch("f", &url)]).await;
}

#[tokio::test]
async fn a_question_last_in_its_batch_is_answered_in_a_later_holding_as_in_process() {
    let url = page_server().await;
    assert_across_holdings_as_in_process(vec![fetch("f", &url), ask("q")]).await;
}

#[tokio::test]
async fn a_batch_asking_twice_is_answered_over_two_later_holdings_as_in_process() {
    let url = page_server().await;
    assert_across_holdings_as_in_process(vec![ask("q1"), fetch("f", &url), ask("q2")]).await;
}

fn puts_a_wait_result_on_the_chain(lines: &[Line]) -> bool {
    lines.iter().any(|line| {
        matches!(line, Line::Wait(entry) if matches!(entry.state, tickloop_harness_transcript::WaitState::Appended))
    })
}

/// The holding that delivers the reply dies before the result reaches the chain; the reply's
/// redelivery to another host goes on with the Run once, without asking again.
#[tokio::test]
async fn a_reply_whose_holder_died_before_the_result_committed_is_redelivered_once() {
    let url = page_server().await;
    let clock = ManualClock::new();
    let chats = DurableChats::new(clock.clone());
    let stream = Arc::new(MemoryEventStream::new());
    let host = |holder: &str, replies: Vec<Vec<Content>>| {
        let model = Arc::new(ScriptedModel::new(replies));
        let built = model.clone();
        let host = ReferenceHost::new(
            holder,
            chats.clone(),
            stream.clone(),
            clock.clone(),
            HostConfig::default(),
            move |store| durable_harness(store, built.clone()),
        );
        (host, model)
    };
    // Each host's model has the replies its own holding needs, whatever the others used.
    let after_reply = || vec![text(PAGE_SUMMARY), text(DONE)];
    let (asking, _) = host("asking", vec![vec![ask("q"), fetch("f", &url)]]);
    let (crashing, _) = host("crashing", after_reply());
    let (surviving, surviving_model) = host("surviving", after_reply());
    let chat = Uuid::new_v4();
    let first = prompt();
    let asked = asking.deliver(chat, first.clone()).await.unwrap();
    assert!(asked.acknowledged);
    let person = asked.waiting.as_ref().unwrap().people[0];
    let reply = answer(&recorded_question(&chats, chat, person.request));

    let holding = crashing.acquire(chat).unwrap();
    holding.crash_at(CrashPoint::BeforeCommit(puts_a_wait_result_on_the_chain));
    let crashed = holding.run(reply.clone()).await;
    assert_eq!(crashed.halt, Some(Halt::Crashed), "{crashed:?}");
    assert!(!puts_a_wait_result_on_the_chain(&chats.history(chat).lines));

    clock.advance(Duration::from_secs(31));
    let redelivered = surviving.deliver(chat, reply.clone()).await.unwrap();
    assert!(redelivered.acknowledged, "{redelivered:?}");
    assert!(matches!(
        redelivered.outcome,
        Some(TurnOutcome::Merged { .. })
    ));
    assert_eq!(redelivered.waiting, None);
    assert_eq!(chats.acknowledgements(chat, reply.input_id.into()), 1);
    let lines = chats.history(chat).lines;
    assert!(lines.iter().any(|line| matches!(
        line,
        Line::Input(entry) if entry.input_id == first.input_id
            && matches!(entry.state, tickloop_harness_transcript::InputState::Settled {
                outcome: tickloop_harness_transcript::SettledOutcome::Completed { .. }
            })
    )));
    let results = lines
        .iter()
        .filter(|line| {
            matches!(line, Line::User(entry) if entry.message.content.iter().any(|content| matches!(content, Content::ToolResult { tool_use_id, .. } if tool_use_id == "q")))
        })
        .count();
    assert_eq!(results, 1, "the reply's result reaches the chain once");
    let questions = lines
        .iter()
        .filter(|line| matches!(line, Line::Wait(entry) if matches!(entry.state, tickloop_harness_transcript::WaitState::Open)))
        .count();
    assert_eq!(questions, 1, "the question is not asked again");
    assert_eq!(surviving_model.requests.lock().unwrap().len(), 2);
}
