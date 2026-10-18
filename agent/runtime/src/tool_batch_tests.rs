//! Re-entering a Run's tool batch: a Run whose task was lost while it waited on a question
//! continues from the waiting call and ends with the conversation and model requests an
//! uninterrupted Run produces.

use std::{path::Path, sync::Arc};

use serde_json::{Value, json};
use tickloop_agent_protocol::{
    Content, Event, EventKind, LoopState, QuestionAnswer, QuestionRequest, Receipt, ToolResult,
    ToolResultContent, projection,
};
use tickloop_agent_test_support::{ScriptedModel, config, next_event, single_question, text};
use tickloop_agent_tools::ASK_USER_QUESTION_TOOL_NAME;
use tokio::sync::broadcast::Receiver;
use tokio_util::sync::CancellationToken;

use super::{CallResult, ResumePoint, RunStart};
use crate::Agent;

const PROMPT: &str = "Prepare the report.";
const FOLLOW_UP: &str = "Also keep it brief.";
const DONE: &str = "Report ready.";
const OTHER_GOAL: &str = "Check the calendar.";
const OTHER_GOAL_DONE: &str = "The calendar is clear.";

fn read(id: &str, file: &str) -> Content {
    Content::ToolUse {
        id: id.into(),
        name: "Read".into(),
        input: json!({ "file_path": file }),
    }
}

fn ask(id: &str) -> Content {
    Content::ToolUse {
        id: id.into(),
        name: ASK_USER_QUESTION_TOOL_NAME.into(),
        input: single_question(&format!("Which format for {id}?")),
    }
}

fn answer(request: &QuestionRequest) -> Vec<QuestionAnswer> {
    vec![QuestionAnswer::other(format!(
        "Answer to {}",
        request.call_id
    ))]
}

fn folder() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("a.txt"), "alpha").unwrap();
    std::fs::write(directory.path().join("b.txt"), "beta").unwrap();
    directory
}

/// What a finished Run leaves: the conversation, and every model request it made.
struct Finished {
    messages: Value,
    requests: Vec<Value>,
    completed_calls: Vec<String>,
}

impl Finished {
    async fn of(
        agent: &Agent,
        receipt: &Receipt,
        model: &ScriptedModel,
        completed_calls: Vec<String>,
    ) -> Self {
        let snapshot = agent.snapshot(receipt.chat_id).await.unwrap();
        projection::history_calls(&snapshot).expect("the conversation stays valid");
        Self {
            messages: serde_json::to_value(&snapshot.messages).unwrap(),
            requests: model
                .requests
                .lock()
                .unwrap()
                .iter()
                .map(|request| {
                    let messages = serde_json::to_string(&request.messages).unwrap();
                    serde_json::from_str(&without_run_identities(messages)).unwrap()
                })
                .collect(),
            completed_calls,
        }
    }

    fn results_of(&self, call_id: &str) -> usize {
        self.messages
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|message| message["content"].as_array().unwrap())
            .filter(|content| content["tool_use_id"] == call_id)
            .count()
    }

    fn mentions(&self, text: &str) -> usize {
        self.messages
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|message| message["content"].as_array().unwrap())
            .filter(|content| content["text"] == text)
            .count()
    }
}

/// The host state each request carries names the Chat's Loops and Runs and when they started,
/// which differ between two Chats making the same requests: ids become their order of appearance.
fn without_run_identities(text: String) -> String {
    const UUID_LENGTH: usize = 36;
    let mut ids = Vec::new();
    let mut normalized = String::new();
    let mut rest = text.as_str();
    while !rest.is_empty() {
        let id = rest
            .get(..UUID_LENGTH)
            .filter(|candidate| candidate.as_bytes()[8] == b'-')
            .and_then(|candidate| uuid::Uuid::parse_str(candidate).ok());
        match id {
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

/// What happens in the Chat while the Run's first question waits.
#[derive(Clone, Copy, Default)]
struct WhileWaiting {
    /// A message for the waiting Run's Loop is queued.
    follow_up: bool,
    /// Another goal runs to completion in the same Chat.
    other_goal: bool,
}

impl WhileWaiting {
    async fn happen(self, agent: &Agent, events: &mut Receiver<Event>, receipt: &Receipt) {
        if self.other_goal {
            let other = agent
                .submit(receipt.chat_id, None, OTHER_GOAL.into())
                .await
                .unwrap();
            loop {
                let event = next_event(events).await;
                match event.kind {
                    EventKind::Completed { .. } if event.run_id == other.run_id => break,
                    EventKind::Failed(error) => panic!("{error}"),
                    _ => {}
                }
            }
        }
        if self.follow_up {
            agent
                .submit(receipt.chat_id, Some(receipt.loop_id), FOLLOW_UP.into())
                .await
                .unwrap();
        }
    }
}

/// Answers every question the Run asks until it completes, with `while_waiting` happening
/// before the first answer.
async fn answer_until_completed(
    agent: &Agent,
    events: &mut Receiver<Event>,
    receipt: &Receipt,
    mut while_waiting: Option<WhileWaiting>,
) -> Vec<String> {
    let mut completed_calls = Vec::new();
    loop {
        let event = next_event(events).await;
        match event.kind {
            EventKind::QuestionAsked { request } => {
                if let Some(while_waiting) = while_waiting.take() {
                    while_waiting.happen(agent, events, receipt).await;
                }
                agent
                    .answer_question(receipt.chat_id, request.id, answer(&request))
                    .await
                    .unwrap();
            }
            EventKind::ToolCompleted { call_id, .. } if event.run_id == receipt.run_id => {
                completed_calls.push(call_id)
            }
            EventKind::Completed { .. } if event.run_id == receipt.run_id => {
                return completed_calls;
            }
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
}

/// The Run answered in process, as today.
async fn in_process(
    directory: &Path,
    batch: Vec<Content>,
    while_waiting: WhileWaiting,
) -> Finished {
    let model = Arc::new(ScriptedModel::new(script(batch, while_waiting)));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, None, PROMPT.into()).await.unwrap();
    let completed_calls =
        answer_until_completed(&agent, &mut events, &receipt, Some(while_waiting)).await;
    let finished = Finished::of(&agent, &receipt, &model, completed_calls).await;
    agent.shutdown().await;
    finished
}

/// A Run whose task is the test's own, so the test can lose it while it waits.
struct OwnedRun {
    agent: Agent,
    model: Arc<ScriptedModel>,
    events: Receiver<Event>,
    receipt: Receipt,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl OwnedRun {
    async fn start(directory: &Path, script: Vec<Vec<Content>>) -> Self {
        let model = Arc::new(ScriptedModel::new(script));
        let agent = Agent::new_embedded(config(), model.clone());
        let chat = agent.create_chat(directory.into()).await.unwrap();
        let events = agent.subscribe();
        agent
            .inner
            .chats
            .lock()
            .await
            .get_mut(&chat)
            .unwrap()
            .active = Some(CancellationToken::new());
        let receipt = agent.submit(chat, None, PROMPT.into()).await.unwrap();
        agent
            .inner
            .chats
            .lock()
            .await
            .get_mut(&chat)
            .unwrap()
            .active = None;
        let (admitted, request, cancel) = agent.next_run(chat).await.unwrap().unwrap();
        assert_eq!(admitted.run_id, receipt.run_id);
        let task = tokio::spawn({
            let agent = agent.clone();
            let receipt = receipt.clone();
            async move {
                assert!(agent.run(receipt, request, cancel).await.is_none());
            }
        });
        Self {
            agent,
            model,
            events,
            receipt,
            task: Some(task),
        }
    }

    /// Waits for the Run's first question, then loses the Run's task as a lost host does: the
    /// Chat keeps the parked Run, its question and its Loop waiting.
    async fn lose_task_at_question(&mut self) -> QuestionRequest {
        let request = loop {
            match next_event(&mut self.events).await.kind {
                EventKind::QuestionAsked { request } => break request,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        };
        let task = self.task.take().unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        request
    }

    /// The answered question's Run takes the execution slot back, as a resumed parked Run does.
    async fn take_slot(&self, question: &QuestionRequest) -> CancellationToken {
        let cancel = CancellationToken::new();
        self.agent
            .inner
            .question_answers
            .lock()
            .await
            .remove(&question.id);
        let mut chats = self.agent.inner.chats.lock().await;
        let chat = chats.get_mut(&self.receipt.chat_id).unwrap();
        assert!(chat.parked.remove(&self.receipt.run_id).is_some());
        assert!(chat.active.is_none());
        chat.snapshot
            .questions
            .retain(|request| request.id != question.id);
        let goal = chat
            .snapshot
            .loops
            .iter_mut()
            .find(|goal| goal.id == self.receipt.loop_id)
            .unwrap();
        assert_eq!(goal.state, LoopState::WaitingQuestion);
        goal.state = LoopState::Running;
        chat.active = Some(cancel.clone());
        chat.active_run = Some(self.receipt.run_id);
        cancel
    }

    /// The canonical id of the Run's assistant message that made call `call_id`.
    async fn batch_message(&self, call_id: &str) -> uuid::Uuid {
        let snapshot = self.agent.snapshot(self.receipt.chat_id).await.unwrap();
        let index = snapshot
            .messages
            .iter()
            .position(|message| {
                message
                    .content
                    .iter()
                    .any(|content| matches!(content, Content::ToolUse { id, .. } if id == call_id))
            })
            .unwrap();
        *snapshot
            .original_message_ids
            .get(&index)
            .or_else(|| snapshot.message_ids.get(&index))
            .unwrap()
    }

    fn resume(&self, point: ResumePoint, cancel: CancellationToken) -> tokio::task::JoinHandle<()> {
        let agent = self.agent.clone();
        let receipt = self.receipt.clone();
        tokio::spawn(async move {
            if let Some(next) = agent.run(receipt, RunStart::Resume(point), cancel).await {
                agent.run_task(next);
            }
        })
    }
}

fn result_text(finished: &Finished, call_id: &str) -> String {
    finished
        .messages
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| message["content"].as_array().unwrap())
        .find(|content| content["tool_use_id"] == call_id)
        .map(|content| content["content"].as_str().unwrap().to_owned())
        .unwrap()
}

fn answered(text: String) -> CallResult {
    CallResult {
        tool: ToolResult {
            content: text.clone(),
            is_error: false,
            output_ref: None,
        },
        content: ToolResultContent::from(text),
        ends_run: false,
    }
}

fn script(batch: Vec<Content>, while_waiting: WhileWaiting) -> Vec<Vec<Content>> {
    let mut script = vec![batch];
    if while_waiting.other_goal {
        script.push(text(OTHER_GOAL_DONE));
    }
    script.push(text(DONE));
    script
}

/// Loses the Run at its first question, then re-enters the batch at that call with the answer
/// the in-process Run got.
async fn reentered(
    directory: &Path,
    batch: Vec<Content>,
    while_waiting: WhileWaiting,
    reference: &Finished,
) -> Finished {
    let mut run = OwnedRun::start(directory, script(batch, while_waiting)).await;
    let question = run.lose_task_at_question().await;
    while_waiting
        .happen(&run.agent, &mut run.events, &run.receipt)
        .await;
    let cancel = run.take_slot(&question).await;
    let point = ResumePoint::InBatch {
        assistant_message: run.batch_message(&question.call_id).await,
        call_id: question.call_id.clone(),
        result: Some(answered(result_text(reference, &question.call_id))),
    };
    let resumed = run.resume(point, cancel);
    let completed_calls =
        answer_until_completed(&run.agent, &mut run.events, &run.receipt, None).await;
    resumed.await.unwrap();
    let finished = Finished::of(&run.agent, &run.receipt, &run.model, completed_calls).await;
    run.agent.shutdown().await;
    finished
}

async fn assert_reentry_matches_in_process(
    batch: Vec<Content>,
    while_waiting: WhileWaiting,
) -> (Finished, Finished) {
    let directory = folder();
    let reference = in_process(directory.path(), batch.clone(), while_waiting).await;
    let resumed = reentered(directory.path(), batch, while_waiting, &reference).await;
    assert_eq!(resumed.messages, reference.messages);
    assert_eq!(resumed.requests, reference.requests);
    (reference, resumed)
}

#[tokio::test]
async fn a_run_reentering_at_its_first_call_runs_the_rest_of_the_batch() {
    let (reference, resumed) = assert_reentry_matches_in_process(
        vec![ask("q"), read("r", "b.txt")],
        WhileWaiting::default(),
    )
    .await;
    assert_eq!(reference.completed_calls, ["q", "r"]);
    assert_eq!(resumed.completed_calls, ["q", "r"]);
    assert_eq!(resumed.requests.len(), 2);
    for call in ["q", "r"] {
        assert_eq!(resumed.results_of(call), 1);
    }
}

#[tokio::test]
async fn a_run_reentering_at_its_last_call_keeps_the_earlier_results() {
    let (_, resumed) = assert_reentry_matches_in_process(
        vec![read("r", "a.txt"), ask("q")],
        WhileWaiting::default(),
    )
    .await;
    assert_eq!(resumed.completed_calls, ["q"]);
    for call in ["r", "q"] {
        assert_eq!(resumed.results_of(call), 1);
    }
}

#[tokio::test]
async fn a_later_question_in_the_reentered_batch_waits_in_process() {
    let (reference, resumed) = assert_reentry_matches_in_process(
        vec![
            read("r1", "a.txt"),
            ask("q1"),
            read("r2", "b.txt"),
            ask("q2"),
        ],
        WhileWaiting::default(),
    )
    .await;
    assert_eq!(reference.completed_calls, ["r1", "q1", "r2", "q2"]);
    assert_eq!(resumed.completed_calls, ["q1", "r2", "q2"]);
    for call in ["r1", "q1", "r2", "q2"] {
        assert_eq!(resumed.results_of(call), 1);
    }
}

#[tokio::test]
async fn a_message_queued_to_the_waiting_loop_joins_once_at_the_end_of_the_batch() {
    let (reference, resumed) = assert_reentry_matches_in_process(
        vec![ask("q"), read("r", "b.txt")],
        WhileWaiting {
            follow_up: true,
            ..WhileWaiting::default()
        },
    )
    .await;
    assert_eq!(reference.mentions(FOLLOW_UP), 1);
    assert_eq!(resumed.mentions(FOLLOW_UP), 1);
    let last_request = resumed.requests.last().unwrap().as_array().unwrap();
    assert_eq!(
        last_request.last().unwrap()["content"][0]["text"],
        FOLLOW_UP,
        "the follow-up follows the batch's results in the next request"
    );
}

#[tokio::test]
async fn results_after_another_goal_ran_in_the_wait_start_a_new_message() {
    let (_, resumed) = assert_reentry_matches_in_process(
        vec![read("r1", "a.txt"), ask("q"), read("r2", "b.txt")],
        WhileWaiting {
            other_goal: true,
            follow_up: true,
        },
    )
    .await;
    assert_eq!(resumed.mentions(OTHER_GOAL_DONE), 1);
    let messages = resumed.messages.as_array().unwrap();
    let results = |message: &Value| {
        message["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|content| content["tool_use_id"].as_str().map(str::to_owned))
            .collect::<Vec<_>>()
    };
    let result_messages = messages
        .iter()
        .map(results)
        .filter(|calls| !calls.is_empty())
        .collect::<Vec<_>>();
    assert_eq!(result_messages, [vec!["r1"], vec!["q", "r2"]]);
}

#[tokio::test]
async fn a_reentered_run_reusing_a_call_id_fails_without_invalidating_the_chat() {
    let directory = folder();
    let mut run = OwnedRun::start(
        directory.path(),
        vec![
            vec![read("r", "a.txt"), ask("q")],
            vec![read("r", "b.txt")],
            text("Recovered."),
        ],
    )
    .await;
    let question = run.lose_task_at_question().await;
    let cancel = run.take_slot(&question).await;
    let point = ResumePoint::InBatch {
        assistant_message: run.batch_message("q").await,
        call_id: "q".into(),
        result: Some(answered("Weekly".into())),
    };
    let resumed = run.resume(point, cancel);
    let failure = loop {
        let event = next_event(&mut run.events).await;
        match event.kind {
            EventKind::Failed(error) if event.run_id == run.receipt.run_id => break error,
            EventKind::Completed { .. } if event.run_id == run.receipt.run_id => {
                panic!("a repeated call id must not run")
            }
            _ => {}
        }
    };
    assert!(failure.contains("repeated tool call"), "{failure}");
    resumed.await.unwrap();
    let snapshot = run.agent.snapshot(run.receipt.chat_id).await.unwrap();
    projection::history_calls(&snapshot).expect("the conversation stays valid");
    let next = run
        .agent
        .submit(run.receipt.chat_id, None, "Try again.".into())
        .await
        .unwrap();
    loop {
        let event = next_event(&mut run.events).await;
        match event.kind {
            EventKind::Completed { .. } if event.run_id == next.run_id => break,
            EventKind::Failed(error) if event.run_id == next.run_id => panic!("{error}"),
            _ => {}
        }
    }
    run.agent.shutdown().await;
}

async fn run_end(events: &mut Receiver<Event>, receipt: &Receipt) -> EventKind {
    loop {
        let event = next_event(events).await;
        if event.run_id == receipt.run_id
            && matches!(
                event.kind,
                EventKind::Completed { .. } | EventKind::Interrupted | EventKind::Failed(_)
            )
        {
            return event.kind;
        }
    }
}

#[tokio::test]
async fn a_reentered_declined_question_ends_the_run_as_in_process() {
    let directory = folder();
    let batch = vec![ask("q"), read("r", "b.txt")];

    let model = Arc::new(ScriptedModel::new(vec![batch.clone()]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, None, PROMPT.into()).await.unwrap();
    loop {
        if let EventKind::QuestionAsked { request } = next_event(&mut events).await.kind {
            agent.decline_question(chat, request.id).await.unwrap();
            break;
        }
    }
    assert!(matches!(
        run_end(&mut events, &receipt).await,
        EventKind::Interrupted
    ));
    let reference = Finished::of(&agent, &receipt, &model, Vec::new()).await;
    agent.shutdown().await;

    let mut run = OwnedRun::start(directory.path(), vec![batch]).await;
    let question = run.lose_task_at_question().await;
    let cancel = run.take_slot(&question).await;
    let declined = result_text(&reference, "q");
    let point = ResumePoint::InBatch {
        assistant_message: run.batch_message("q").await,
        call_id: "q".into(),
        result: Some(CallResult {
            tool: ToolResult {
                content: declined.clone(),
                is_error: true,
                output_ref: None,
            },
            content: ToolResultContent::from(declined),
            ends_run: true,
        }),
    };
    let resumed = run.resume(point, cancel);
    assert!(matches!(
        run_end(&mut run.events, &run.receipt).await,
        EventKind::Interrupted
    ));
    resumed.await.unwrap();
    let finished = Finished::of(&run.agent, &run.receipt, &run.model, Vec::new()).await;
    assert_eq!(finished.messages, reference.messages);
    assert_eq!(finished.requests, reference.requests);
    assert_eq!(
        run.agent.snapshot(run.receipt.chat_id).await.unwrap().loops[0].state,
        LoopState::Interrupted
    );
    run.agent.shutdown().await;
}
