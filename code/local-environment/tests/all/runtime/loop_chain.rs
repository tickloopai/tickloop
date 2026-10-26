//! A Chat's Loop is recorded along the conversation chain: after rewind, Branch, compaction and
//! restart it keeps the same Loop, its Tick Subscriptions and its receiving switch. Several Agent
//! instances in the same test process simulate several processes; for the Trigger lease, lock file
//! takeover, receiving state and a child process holding the lock, see `loop_trigger_safety.rs`.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tickloop_code_settings::local_agent::LocalAgent;

use async_trait::async_trait;
use tickloop_harness::model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_harness::protocol::{
    BranchRequest, ChatId, Content, DirectedSummaryRequest, EventKind, LoopState, Message,
    ModelIdentity, PauseReason, Protocol, Receipt, Receiving, RewindMode, Role, SummaryDirection,
    Usage,
};
use tickloop_harness::runtime::Agent;
use tickloop_harness::test_support::shell_ticks::ShellTick;
use tickloop_harness::test_support::{config, next_event};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Replies to every request with some text; summary requests without tools get a summary reply.
struct EchoModel {
    requests: Mutex<Vec<ModelRequest>>,
}

impl EchoModel {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(Vec::new()),
        })
    }

    /// Number of requests whose last message contains `needle`, used to count runs started by
    /// Triggers.
    fn prompts_containing(&self, needle: &str) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| !request.tools.is_empty())
            .filter(|request| {
                request
                    .messages
                    .last()
                    .is_some_and(|message| message_text(message).contains(needle))
            })
            .count()
    }

    fn last_conversation_request(&self) -> ModelRequest {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|request| !request.tools.is_empty())
            .unwrap()
            .clone()
    }
}

#[async_trait]
impl ModelClient for EchoModel {
    async fn generate_title(
        &self,
        request: tickloop_harness::model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_harness::model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_harness::model::ModelOutput, tickloop_harness::model::ModelError> {
        Ok(tickloop_harness::test_support::title_output(request))
    }

    fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            provider: tickloop_harness::protocol::ModelProvider::Api,
            profile: "fixture".into(),
            protocol: Protocol::Messages,
            requested_model: "fixture".into(),
        }
    }

    async fn complete(
        &self,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        let reply = if request.tools.is_empty() {
            "summary of the earlier conversation".to_owned()
        } else {
            format!("answered {}", request.messages.len())
        };
        self.requests.lock().unwrap().push(request);
        let _ = events.send(ModelDelta::Text(reply.clone()));
        Ok(ModelOutput {
            private_source: None,
            content: vec![Content::Text { text: reply }],
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

fn message_text(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|content| match content {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

const HISTORY_LOADED: Receiving = Receiving::Paused {
    reason: PauseReason::HistoryLoaded,
};

/// An Agent opened on `root` watching the Shell Tick of `task` on its own in-memory stream.
async fn watching_agent(
    root: &std::path::Path,
    model: Arc<EchoModel>,
    task: Uuid,
) -> (Agent, ShellTick) {
    let agent = Agent::open_embedded(root.into(), config(), model).unwrap();
    let shell = ShellTick::new(task);
    shell.install(&agent).await;
    (agent, shell)
}

async fn complete_turn(agent: &Agent, chat: ChatId, prompt: &str) -> Receipt {
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, prompt.into()).await.unwrap();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != receipt.run_id {
            continue;
        }
        match event.kind {
            EventKind::Completed { .. } => return receipt,
            EventKind::Failed(error) => panic!("turn failed: {error}"),
            _ => {}
        }
    }
}

async fn wait_until(what: &str, timeout: Duration, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + timeout;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn transcript_files(directory: &std::path::Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(directory).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(transcript_files(&path));
        } else if path
            .extension()
            .is_some_and(|extension| extension == "jsonl")
        {
            files.push(path);
        }
    }
    files
}

/// A finished Loop's state time is written with its Loop record and read back on restart;
/// records written before the field existed restore the same Loop with the time unknown, never
/// the restart time.
#[tokio::test]
async fn loop_state_times_survive_restart_and_legacy_records_stay_unknown() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let agent = Agent::open_embedded(root.clone(), config(), EchoModel::new()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let started = now_ms();
    let finished = complete_turn(&agent, chat, "finish once").await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    let goal = &snapshot.chat_loop;
    assert_eq!(goal.id, finished.loop_id);
    assert_eq!(goal.state, LoopState::Idle);
    let finished_at = goal.state_since.expect("a finish records its time");
    assert!(finished_at >= started && finished_at <= now_ms());
    agent.shutdown().await;
    drop(agent);

    let transcripts = transcript_files(&root);
    assert!(
        transcripts.iter().any(|path| std::fs::read_to_string(path)
            .unwrap()
            .contains(&format!("\"stateSince\":{finished_at}"))),
        "the Loop record carries the time on disk"
    );
    tokio::time::sleep(Duration::from_millis(5)).await;
    let restarted = Agent::open_embedded(root.clone(), config(), EchoModel::new()).unwrap();
    let restored = restarted.resume_chat(chat).await.unwrap();
    assert_eq!(restored.chat_loop.id, finished.loop_id);
    assert_eq!(restored.chat_loop.state, LoopState::Idle);
    assert_eq!(
        restored.chat_loop.state_since,
        Some(finished_at),
        "restart keeps the original time"
    );
    restarted.shutdown().await;
    drop(restarted);

    for path in &transcripts {
        let legacy = std::fs::read_to_string(path)
            .unwrap()
            .replace(&format!(",\"stateSince\":{finished_at}"), "");
        std::fs::write(path, legacy).unwrap();
    }
    let reopened = Agent::open_embedded(root, config(), EchoModel::new()).unwrap();
    let restored = reopened.resume_chat(chat).await.unwrap();
    assert_eq!(restored.chat_loop.state, LoopState::Idle);
    assert_eq!(
        restored.chat_loop.state_since, None,
        "a legacy record leaves the time unknown"
    );
    reopened.shutdown().await;
}

/// A restart pauses an idle Loop's receiving without converting its state, so the Loop keeps
/// its recorded state time.
#[tokio::test]
async fn a_restart_pauses_an_idle_loop_and_keeps_its_state_time() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let agent = Agent::open_embedded(root.clone(), config(), EchoModel::new()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    complete_turn(&agent, chat, "watched goal").await;
    let before = agent.snapshot(chat).await.unwrap();
    assert_eq!(before.chat_loop.state, LoopState::Idle);
    let idle_at = before.chat_loop.state_since;
    assert!(idle_at.is_some());
    agent.shutdown().await;
    drop(agent);

    let restarted = Agent::open_embedded(root, config(), EchoModel::new()).unwrap();
    let restored = restarted.resume_chat(chat).await.unwrap();
    assert_eq!(restored.chat_loop.state, LoopState::Idle);
    assert_eq!(restored.chat_loop.state_since, idle_at);
    assert_eq!(restored.chat_loop.receiving, HISTORY_LOADED);
    restarted.shutdown().await;
}

#[tokio::test]
async fn rewind_keeps_the_loop_and_its_subscriptions_across_restarts() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let task = Uuid::new_v4();
    let model = EchoModel::new();
    let (agent, shell) = watching_agent(&root, model.clone(), task).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let first = complete_turn(&agent, chat, "first goal").await;
    complete_turn(&agent, chat, "second goal").await;
    shell.subscribe(&agent, chat).await;
    let subscriptions = agent.snapshot(chat).await.unwrap().chat_loop.subscriptions;
    let point = agent.snapshot(chat).await.unwrap().rewind_points()[1].clone();
    let outcome = agent
        .rewind(chat, point.message_id, RewindMode::Conversation)
        .await
        .unwrap();
    assert_eq!(outcome.draft.unwrap().prompt, "second goal");
    assert_eq!(outcome.snapshot.chat_loop.id, first.loop_id);
    assert_eq!(outcome.snapshot.chat_loop.subscriptions, subscriptions);
    assert!(outcome.snapshot.chat_loop.receiving.is_receiving());
    shell.update(1, "second check");
    wait_until(
        "the subscription to deliver after the rewind",
        Duration::from_secs(3),
        || model.prompts_containing("second check") >= 1,
    )
    .await;
    let third = complete_turn(&agent, chat, "third goal").await;
    assert_eq!(third.loop_id, first.loop_id);
    agent.shutdown().await;
    drop(agent);

    let restarted_model = EchoModel::new();
    let (restarted, _shell) = watching_agent(&root, restarted_model.clone(), task).await;
    let restored = restarted.resume_chat(chat).await.unwrap();
    assert_eq!(restored.chat_loop.id, first.loop_id);
    let third_point = restored
        .rewind_points()
        .into_iter()
        .find(|point| point.prompt == "third goal")
        .unwrap();
    let outcome = restarted
        .rewind(chat, third_point.message_id, RewindMode::Conversation)
        .await
        .unwrap();
    assert_eq!(outcome.snapshot.chat_loop.id, first.loop_id);
    complete_turn(&restarted, chat, "fourth goal").await;
    let request = restarted_model.last_conversation_request();
    let sent = request
        .messages
        .iter()
        .filter(|message| message.role != Role::System)
        .map(message_text)
        .collect::<Vec<_>>();
    assert_eq!(sent.first().map(String::as_str), Some("first goal"));
    assert_eq!(sent.last().map(String::as_str), Some("fourth goal"));
    assert!(!sent.iter().any(|text| text == "third goal"));
    restarted.shutdown().await;
    drop(restarted);

    let reopened = Agent::open_embedded(root, config(), EchoModel::new()).unwrap();
    let restored = reopened.resume_chat(chat).await.unwrap();
    assert_eq!(restored.chat_loop.id, first.loop_id);
    assert_eq!(restored.chat_loop.subscriptions, subscriptions);
    reopened.shutdown().await;
}

#[tokio::test]
async fn branch_copies_the_loop_paused_until_the_branch_is_continued() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let (agent, shell) = watching_agent(&root, EchoModel::new(), Uuid::new_v4()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let watched = complete_turn(&agent, chat, "watch goal").await;
    shell.subscribe(&agent, chat).await;

    let outcome = agent
        .branch_chat(BranchRequest {
            request_id: Uuid::new_v4(),
            source_chat_id: chat,
            name: None,
        })
        .await
        .unwrap();
    let child = outcome.snapshot;
    let source = agent.snapshot(chat).await.unwrap();
    assert_eq!(child.chat_loop.id, watched.loop_id);
    assert_eq!(child.chat_loop.receiving, HISTORY_LOADED);
    assert_eq!(child.chat_loop.state, LoopState::Idle);
    assert_eq!(child.chat_loop.subscriptions.len(), 1);
    assert_eq!(
        child.chat_loop.subscriptions[0].instance,
        source.chat_loop.subscriptions[0].instance
    );
    assert_ne!(
        child.chat_loop.subscriptions[0].id, source.chat_loop.subscriptions[0].id,
        "the copy subscribes on its own"
    );
    assert!(source.chat_loop.receiving.is_receiving());

    complete_turn(
        &agent,
        outcome.child_chat_id,
        "resume watching in the branch",
    )
    .await;
    let resumed = agent.snapshot(outcome.child_chat_id).await.unwrap();
    assert!(resumed.chat_loop.receiving.is_receiving());
    assert_eq!(resumed.chat_loop.state, LoopState::Idle);
    agent.shutdown().await;
    drop(agent);

    let reopened = Agent::open_embedded(root, config(), EchoModel::new()).unwrap();
    let restored = reopened.resume_chat(outcome.child_chat_id).await.unwrap();
    assert_eq!(restored.chat_loop.id, watched.loop_id);
    assert_eq!(restored.chat_loop.receiving, HISTORY_LOADED);
    reopened.shutdown().await;
}

#[tokio::test]
async fn subscriptions_survive_restart_after_compaction() {
    for directed in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("root");
        let (agent, shell) = watching_agent(&root, EchoModel::new(), Uuid::new_v4()).await;
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let watched = complete_turn(&agent, chat, "watch goal").await;
        shell.subscribe(&agent, chat).await;
        let subscriptions = agent.snapshot(chat).await.unwrap().chat_loop.subscriptions;
        complete_turn(&agent, chat, "other goal").await;
        complete_turn(&agent, chat, "third goal").await;
        if directed {
            let point = agent.snapshot(chat).await.unwrap().rewind_points()[2].clone();
            agent
                .summarize_checkpoint(
                    chat,
                    DirectedSummaryRequest {
                        message_id: point.message_id,
                        direction: SummaryDirection::UpToHere,
                        guidance: None,
                    },
                )
                .await
                .unwrap();
        } else {
            agent.compact(chat).await.unwrap();
        }
        let in_process = agent.snapshot(chat).await.unwrap();
        let in_process_conversation = in_process
            .conversation_messages()
            .unwrap()
            .into_iter()
            .filter(|message| message.role != Role::System)
            .collect::<Vec<_>>();
        agent.shutdown().await;
        drop(agent);

        let model = EchoModel::new();
        let restarted = Agent::open_embedded(root, config(), model.clone()).unwrap();
        let restored = restarted.resume_chat(chat).await.unwrap();
        let label = if directed { "directed" } else { "full" };
        assert_eq!(restored.chat_loop.id, watched.loop_id, "{label}");
        assert_eq!(restored.chat_loop.subscriptions, subscriptions, "{label}");
        assert_eq!(restored.chat_loop.state, LoopState::Idle, "{label}");
        assert_eq!(restored.chat_loop.receiving, HISTORY_LOADED, "{label}");

        complete_turn(&restarted, chat, "resume watching").await;
        // Loop entries do not enter the conversation sent to the model: the request after restart
        // adds only the resume sentinel and the new prompt.
        let sent = model
            .last_conversation_request()
            .messages
            .into_iter()
            .filter(|message| message.role != Role::System)
            .collect::<Vec<_>>();
        assert!(
            sent.iter().all(|message| !message.content.is_empty()),
            "{label}"
        );
        assert!(sent.starts_with(&in_process_conversation), "{label}");
        let extra = sent[in_process_conversation.len()..]
            .iter()
            .map(message_text)
            .collect::<Vec<_>>();
        assert!(
            extra == ["resume watching"] || extra == ["No response requested.", "resume watching"],
            "{label}: {extra:?}"
        );
        assert!(
            restarted
                .snapshot(chat)
                .await
                .unwrap()
                .chat_loop
                .receiving
                .is_receiving(),
            "{label}"
        );
        restarted.shutdown().await;
    }
}

/// A loaded Loop pauses receiving but keeps watching: the latest update waits, and the next user
/// input continues the Chat and delivers it.
#[tokio::test]
async fn a_restarted_loop_keeps_the_latest_update_until_a_new_input_resumes_it() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let task = Uuid::new_v4();
    let (agent, shell) = watching_agent(&root, EchoModel::new(), task).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    complete_turn(&agent, chat, "watched goal").await;
    shell.subscribe(&agent, chat).await;
    agent.shutdown().await;
    drop(agent);

    let model = EchoModel::new();
    let (restarted, shell) = watching_agent(&root, model.clone(), task).await;
    let restored = restarted.resume_chat(chat).await.unwrap();
    assert_eq!(restored.chat_loop.state, LoopState::Idle);
    assert_eq!(restored.chat_loop.receiving, HISTORY_LOADED);
    shell.wait_until_live(&restarted, chat).await;
    shell.update(1, "older check");
    shell.update(2, "newest check");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(model.requests.lock().unwrap().is_empty());

    complete_turn(&restarted, chat, "resume watching").await;
    assert!(
        restarted
            .snapshot(chat)
            .await
            .unwrap()
            .chat_loop
            .receiving
            .is_receiving()
    );
    wait_until(
        "the held update to deliver once resumed",
        Duration::from_secs(3),
        || model.prompts_containing("newest check") >= 1,
    )
    .await;
    assert_eq!(model.prompts_containing("older check"), 0);
    restarted.shutdown().await;
}

/// A Chat is created with its one Loop: idle, with nothing to receive. Every turn runs in it and
/// a restart restores the same Loop.
#[tokio::test]
async fn a_new_chat_has_one_loop_that_every_turn_and_restart_keep() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let agent = Agent::open_embedded(root.clone(), config(), EchoModel::new()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let created = agent.snapshot(chat).await.unwrap().chat_loop;
    assert_eq!(created.state, LoopState::Idle);
    assert!(created.run_id.is_none());
    assert!(created.receiving.is_receiving());
    assert!(created.subscriptions.is_empty());
    assert!(created.event_sources.is_empty());
    let first = complete_turn(&agent, chat, "first task").await;
    let second = complete_turn(&agent, chat, "an unrelated task").await;
    assert_eq!(first.loop_id, created.id);
    assert_eq!(second.loop_id, created.id);
    agent.shutdown().await;
    drop(agent);

    let restarted = Agent::open_embedded(root, config(), EchoModel::new()).unwrap();
    let restored = restarted.resume_chat(chat).await.unwrap();
    assert_eq!(restored.chat_loop.id, created.id);
    let third = complete_turn(&restarted, chat, "after the restart").await;
    assert_eq!(third.loop_id, created.id);
    restarted.shutdown().await;
}

/// An interrupted or failed turn is only how that turn ended: the next input continues the same
/// Loop.
#[tokio::test]
async fn interrupted_and_failed_turns_continue_with_the_next_input() {
    use tickloop_harness::test_support::{ScriptedModel, call, single_question, text};
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![call(
        "ask",
        "AskUserQuestion",
        single_question("Which file?"),
    )]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let asked = agent.submit(chat, "ask me".into()).await.unwrap();
    loop {
        if matches!(
            next_event(&mut events).await.kind,
            EventKind::QuestionAsked { .. }
        ) {
            break;
        }
    }
    agent.interrupt(chat).await.unwrap();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id == asked.run_id && matches!(event.kind, EventKind::Interrupted) {
            break;
        }
    }
    assert_eq!(
        agent.snapshot(chat).await.unwrap().chat_loop.state,
        LoopState::Interrupted
    );

    // The script is empty now, so this turn fails.
    let failed = agent.submit(chat, "try again".into()).await.unwrap();
    assert_eq!(failed.loop_id, asked.loop_id);
    loop {
        let event = next_event(&mut events).await;
        if event.run_id == failed.run_id && matches!(event.kind, EventKind::Failed(_)) {
            break;
        }
    }
    assert_eq!(
        agent.snapshot(chat).await.unwrap().chat_loop.state,
        LoopState::Failed
    );

    model.enqueue(text("done at last"));
    let continued = complete_turn(&agent, chat, "go on").await;
    assert_eq!(continued.loop_id, asked.loop_id);
    assert_eq!(
        agent.snapshot(chat).await.unwrap().chat_loop.state,
        LoopState::Idle
    );
    agent.shutdown().await;
}

/// Loop records written before a Chat had one Loop are dropped on load; the conversation they
/// sat in stays visible and the Chat goes on with a Loop of its own.
#[tokio::test]
async fn old_format_loop_records_are_dropped_and_the_history_stays_visible() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let (agent, shell) = watching_agent(&root, EchoModel::new(), Uuid::new_v4()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let before = complete_turn(&agent, chat, "OLD-FORMAT-PROMPT").await;
    shell.subscribe(&agent, chat).await;
    agent.shutdown().await;
    drop(agent);
    for path in transcript_files(&root) {
        let old = std::fs::read_to_string(&path).unwrap().replace(
            "\"subtype\":\"chat_loop\",\"loop\":{",
            "\"subtype\":\"loop_state\",\"loop\":{\"goal\":\"OLD-FORMAT-PROMPT\",",
        );
        std::fs::write(&path, old).unwrap();
    }

    let model = EchoModel::new();
    let reopened = Agent::open_embedded(root, config(), model.clone()).unwrap();
    let restored = reopened.resume_chat(chat).await.unwrap();
    assert!(
        restored
            .messages
            .iter()
            .any(|message| message_text(message).contains("OLD-FORMAT-PROMPT")),
        "the history stays visible"
    );
    assert_ne!(restored.chat_loop.id, before.loop_id);
    assert_eq!(restored.chat_loop.state, LoopState::Idle);
    assert!(restored.chat_loop.subscriptions.is_empty());
    let continued = complete_turn(&reopened, chat, "go on").await;
    assert_eq!(continued.loop_id, restored.chat_loop.id);
    assert!(
        model
            .last_conversation_request()
            .messages
            .iter()
            .any(|message| message_text(message).contains("OLD-FORMAT-PROMPT")),
        "the continued turn still sees the earlier conversation"
    );
    reopened.shutdown().await;
}
