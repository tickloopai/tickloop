use crate::runtime::support;
use tickloop_code_settings::local_agent::LocalAgent;

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use serde_json::{Value, json};
use tickloop_harness::model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_harness::protocol::{
    Content, CounterTotal, Event, EventKind, ExecutionMode, InputModality, LedgerError, LoopState,
    ModelChange, ModelConfig, ModelIdentity, ModelRequestKind, Protocol, ProtocolFailure,
    RequestFailure, RequestOutcome, ResponseRejection, RunId, Usage, UsageCounter,
};
use tickloop_harness::runtime::Agent;
use tickloop_harness::test_support::harness::{ChosenModels, embedded_agent};
use tokio::sync::{Notify, broadcast, mpsc::UnboundedSender};
use tokio_util::sync::CancellationToken;

const EVENT_TIMEOUT: Duration = Duration::from_secs(10);

struct ResponseStep {
    observations: Vec<Usage>,
    result: Result<ModelOutput, ModelError>,
    wait_for_cancel: Option<Arc<Notify>>,
    usage_separated_text_chunks: bool,
}

impl ResponseStep {
    fn reply(content: Vec<Content>, usage: Usage) -> Self {
        Self {
            observations: vec![usage.clone()],
            result: Ok(ModelOutput {
                private_source: None,
                stop_reason: if content
                    .iter()
                    .any(|block| matches!(block, Content::ToolUse { .. }))
                {
                    "tool_use".into()
                } else {
                    "end_turn".into()
                },
                content,
                usage,
            }),
            wait_for_cancel: None,
            usage_separated_text_chunks: false,
        }
    }

    fn pending(usage: Usage, entered: Arc<Notify>) -> Self {
        Self {
            observations: vec![usage],
            result: Err(ModelError::Cancelled),
            wait_for_cancel: Some(entered),
            usage_separated_text_chunks: false,
        }
    }
}

struct UsageModel {
    identity: ModelIdentity,
    steps: Mutex<VecDeque<ResponseStep>>,
    requests: Mutex<Vec<ModelRequest>>,
}

impl UsageModel {
    fn new(identity: ModelIdentity, steps: Vec<ResponseStep>) -> Self {
        Self {
            identity,
            steps: Mutex::new(steps.into()),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

#[async_trait]
impl ModelClient for UsageModel {
    async fn generate_title(
        &self,
        request: tickloop_harness::model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_harness::model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_harness::model::ModelOutput, tickloop_harness::model::ModelError> {
        Ok(tickloop_harness::test_support::title_output(request))
    }

    fn identity(&self) -> ModelIdentity {
        self.identity.clone()
    }

    async fn complete(
        &self,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        self.requests.lock().unwrap().push(request);
        let step = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected model request exhausted the usage fixture");
        for usage in step.observations {
            events.send(ModelDelta::Usage(usage)).unwrap();
        }
        if let Some(entered) = step.wait_for_cancel {
            entered.notify_one();
            cancel.cancelled().await;
        }
        if let Ok(output) = &step.result {
            for block in &output.content {
                if let Content::Text { text } = block {
                    if step.usage_separated_text_chunks {
                        for character in text.chars() {
                            events
                                .send(ModelDelta::Text(character.to_string()))
                                .unwrap();
                            events
                                .send(ModelDelta::Usage(output.usage.clone()))
                                .unwrap();
                        }
                    } else {
                        events.send(ModelDelta::Text(text.clone())).unwrap();
                    }
                }
            }
        }
        step.result
    }
}

fn identity(profile: &str, protocol: Protocol, requested_model: &str) -> ModelIdentity {
    ModelIdentity {
        provider: tickloop_harness::protocol::ModelProvider::Api,
        profile: profile.into(),
        protocol,
        requested_model: requested_model.into(),
    }
}

fn main_identity() -> ModelIdentity {
    identity("main", Protocol::Messages, "main-model")
}

fn config(identity: &ModelIdentity) -> ModelConfig {
    ModelConfig {
        provider: tickloop_harness::protocol::ModelProvider::Api,
        auth_source: tickloop_harness::protocol::ModelAuthSource::ApiKeyEnv,
        name: identity.profile.clone(),
        protocol: identity.protocol,
        model: identity.requested_model.clone(),
        base_url: "http://127.0.0.1".into(),
        api_key_env: String::new(),
        max_output_tokens: 1024,
        provider_id: "deepseek-api".into(),
        provider_name: "DeepSeek API".into(),
        model_id: identity.requested_model.clone(),
        connection: None,
        card: None,
        input_modalities: vec![InputModality::Text],
    }
}

fn counters(input: u64, output: u64, read: u64, write: u64) -> Usage {
    Usage {
        input_tokens: Some(input),
        output_tokens: Some(output),
        cache_read_input_tokens: Some(read),
        cache_write_input_tokens: Some(write),
    }
}

fn text(value: &str) -> Vec<Content> {
    vec![Content::Text { text: value.into() }]
}

fn tool(id: &str, name: &str, input: Value) -> Content {
    Content::ToolUse {
        id: id.into(),
        name: name.into(),
        input,
    }
}

async fn next_event(events: &mut broadcast::Receiver<Event>) -> Event {
    tokio::time::timeout(EVENT_TIMEOUT, async {
        loop {
            match events.recv().await {
                Ok(event) => return event,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(error) => panic!("event channel closed: {error}"),
            }
        }
    })
    .await
    .expect("usage fixture timed out waiting for an event")
}

async fn terminal_event(events: &mut broadcast::Receiver<Event>, run: RunId) -> Event {
    tokio::time::timeout(EVENT_TIMEOUT, async {
        loop {
            let event = next_event(events).await;
            if event.run_id == run
                && matches!(
                    event.kind,
                    EventKind::Completed { .. } | EventKind::Failed(_) | EventKind::Interrupted
                )
            {
                return event;
            }
        }
    })
    .await
    .expect("usage fixture timed out waiting for Run completion")
}

async fn completed_usage(events: &mut broadcast::Receiver<Event>, run: RunId) -> Usage {
    match terminal_event(events, run).await.kind {
        EventKind::Completed { usage } => usage,
        other => panic!("expected a completed Run, received {other:?}"),
    }
}

#[tokio::test]
async fn auto_setup_records_chat_owned_usage_and_terminal_outcomes_without_a_loop() {
    for (step, outcome) in [
        (
            ResponseStep::reply(text(r#"{"selected":[0]}"#), counters(12, 3, 0, 0)),
            RequestOutcome::Accepted,
        ),
        (
            ResponseStep::reply(text(r#"{"selected":[99]}"#), counters(12, 3, 0, 0)),
            RequestOutcome::HostRejected(ResponseRejection::InvalidVerdict),
        ),
        (
            ResponseStep {
                result: Err(ModelError::Configuration("fixture failure".into())),
                ..ResponseStep::reply(Vec::new(), counters(12, 3, 0, 0))
            },
            RequestOutcome::Failed(RequestFailure::LocalConfiguration),
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("README.md"), "Project metadata").unwrap();
        let archive = directory.path().join("state");
        let model = Arc::new(UsageModel::new(main_identity(), vec![step]));
        let agent =
            Agent::open_embedded(archive.clone(), config(&main_identity()), model.clone()).unwrap();
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let before = agent.permissions_view(chat).await.unwrap().revision;
        let result = agent
            .generate_auto_environment(chat, Default::default())
            .await;
        assert_eq!(
            result.is_ok(),
            outcome == RequestOutcome::Accepted,
            "{result:?}"
        );
        let snapshot = agent.snapshot(chat).await.unwrap();
        assert!(snapshot.chat_loop.run_id.is_none());
        assert!(snapshot.messages.is_empty());
        let entries = snapshot.request_usage.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, ModelRequestKind::AutoSetup);
        assert_eq!(entries[0].chat_id, chat);
        assert_eq!(entries[0].loop_id, None);
        assert_eq!(entries[0].run_id, None);
        assert_eq!(entries[0].outcome, outcome);
        assert_eq!(entries[0].usage, counters(12, 3, 0, 0));
        assert_eq!(model.request_count(), 1);
        assert_eq!(agent.permissions_view(chat).await.unwrap().revision, before);
        agent.shutdown().await;
    }
}

#[tokio::test(start_paused = true)]
async fn auto_setup_timeout_retains_observed_usage_and_closes_the_request() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("README.md"), "Project metadata").unwrap();
    let model = Arc::new(UsageModel::new(
        main_identity(),
        vec![ResponseStep::pending(
            counters(17, 2, 0, 0),
            Arc::new(Notify::new()),
        )],
    ));
    let agent = Agent::open_embedded(
        directory.path().join("state"),
        config(&main_identity()),
        model.clone(),
    )
    .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let error = agent
        .generate_auto_environment(chat, Default::default())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("timed out"), "{error}");
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(snapshot.chat_loop.run_id.is_none());
    assert_eq!(snapshot.request_usage.entries().len(), 1);
    let record = &snapshot.request_usage.entries()[0];
    assert_eq!(record.kind, ModelRequestKind::AutoSetup);
    assert_eq!(record.outcome, RequestOutcome::TimedOut);
    assert_eq!(record.usage, counters(17, 2, 0, 0));
    assert_eq!(model.request_count(), 1);
    agent.shutdown().await;
}

#[tokio::test]
async fn auto_setup_shutdown_cancels_the_request_and_retains_usage() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("README.md"), "Project metadata").unwrap();
    let entered = Arc::new(Notify::new());
    let model = Arc::new(UsageModel::new(
        main_identity(),
        vec![ResponseStep::pending(
            counters(17, 2, 0, 0),
            entered.clone(),
        )],
    ));
    let agent = Agent::open_embedded(
        directory.path().join("state"),
        config(&main_identity()),
        model.clone(),
    )
    .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let task_agent = agent.clone();
    let task = tokio::spawn(async move {
        task_agent
            .generate_auto_environment(chat, Default::default())
            .await
    });
    tokio::time::timeout(EVENT_TIMEOUT, entered.notified())
        .await
        .unwrap();
    agent.shutdown().await;
    assert!(task.await.unwrap().is_err());
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(snapshot.chat_loop.run_id.is_none());
    assert_eq!(snapshot.request_usage.entries().len(), 1);
    let record = &snapshot.request_usage.entries()[0];
    assert_eq!(record.kind, ModelRequestKind::AutoSetup);
    assert_eq!(record.outcome, RequestOutcome::Cancelled);
    assert_eq!(record.usage, counters(17, 2, 0, 0));
    assert_eq!(model.request_count(), 1);
}

#[tokio::test]
async fn one_reply_observations_and_final_usage_are_recorded_once() {
    let directory = tempfile::tempdir().unwrap();
    let archive = directory.path().join("archive");
    let expected = counters(120, 12, 30, 0);
    let mut step = ResponseStep::reply(text("done"), expected.clone());
    step.observations = vec![
        Usage {
            input_tokens: Some(120),
            output_tokens: Some(0),
            ..Usage::default()
        },
        expected.clone(),
        expected.clone(),
    ];
    let model = Arc::new(UsageModel::new(main_identity(), vec![step]));
    let agent =
        Agent::open_embedded(archive.clone(), config(&main_identity()), model.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, "Reply".into()).await.unwrap();
    assert_eq!(completed_usage(&mut events, receipt.run_id).await, expected);
    let snapshot = agent.snapshot(chat).await.unwrap();
    let entries = support::model_script_requests(&snapshot);
    assert_eq!(entries.len(), 1);
    assert!(entries[0].sequence.0 > 0);
    assert_eq!(entries[0].chat_id, chat);
    assert_eq!(entries[0].loop_id, Some(receipt.loop_id));
    assert_eq!(entries[0].run_id, Some(receipt.run_id));
    assert_eq!(entries[0].model, main_identity());
    assert_eq!(entries[0].kind, ModelRequestKind::Conversation);
    assert_eq!(entries[0].outcome, RequestOutcome::Accepted);
    assert_eq!(entries[0].usage, expected);
    assert_eq!(model.request_count(), 1);
    agent.shutdown().await;
}

#[tokio::test]
async fn tool_rounds_and_multiple_calls_share_one_entry_per_model_request() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("one.txt"), "one").unwrap();
    std::fs::write(directory.path().join("two.txt"), "two").unwrap();
    let model = Arc::new(UsageModel::new(
        main_identity(),
        vec![
            ResponseStep::reply(
                vec![
                    tool("read-one", "Read", json!({"file_path":"one.txt"})),
                    tool("read-two", "Read", json!({"file_path":"two.txt"})),
                ],
                counters(10, 1, 2, 0),
            ),
            ResponseStep::reply(
                vec![tool("read-again", "Read", json!({"file_path":"one.txt"}))],
                counters(20, 2, 3, 0),
            ),
            ResponseStep::reply(text("Read both sources"), counters(30, 3, 4, 1)),
        ],
    ));
    let agent = Agent::new_embedded(config(&main_identity()), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, "Read sources".into()).await.unwrap();
    assert_eq!(
        completed_usage(&mut events, receipt.run_id).await,
        counters(60, 6, 9, 1)
    );
    let snapshot = agent.snapshot(chat).await.unwrap();
    let entries = support::model_script_requests(&snapshot);
    assert_eq!(model.request_count(), 3);
    assert_eq!(snapshot.operations.len(), 3);
    assert_eq!(entries.len(), 3);
    for (index, record) in entries.iter().enumerate() {
        assert!(record.sequence.0 > index as u64);
        assert_eq!(record.kind, ModelRequestKind::Conversation);
        assert_eq!(record.outcome, RequestOutcome::Accepted);
        assert_eq!(record.run_id, Some(receipt.run_id));
    }
    assert_eq!(entries[0].usage, counters(10, 1, 2, 0));
    assert_eq!(entries[1].usage, counters(20, 2, 3, 0));
    assert_eq!(entries[2].usage, counters(30, 3, 4, 1));
    agent.shutdown().await;
}

#[tokio::test]
async fn overflowing_run_usage_fails_without_losing_accepted_request_counters() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("source.txt"), "source").unwrap();
    let first_usage = counters(u64::MAX, 1, 0, 0);
    let second_usage = counters(1, 2, 0, 0);
    let model = Arc::new(UsageModel::new(
        main_identity(),
        vec![
            ResponseStep::reply(
                vec![tool("read", "Read", json!({"file_path":"source.txt"}))],
                first_usage.clone(),
            ),
            ResponseStep::reply(text("Read complete"), second_usage.clone()),
        ],
    ));
    let agent = Agent::new_embedded(config(&main_identity()), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, "Read source".into()).await.unwrap();
    let terminal = terminal_event(&mut events, receipt.run_id).await;
    match terminal.kind {
        EventKind::Failed(error) => assert!(error.contains("Input usage exceeds")),
        other => panic!("overflow must fail the Run, received {other:?}"),
    }
    let snapshot = agent.snapshot(chat).await.unwrap();
    let entries = support::model_script_requests(&snapshot);
    assert_eq!(model.request_count(), 2);
    assert_eq!(entries.len(), 2);
    for (index, entry) in entries.iter().enumerate() {
        assert!(entry.sequence.0 > index as u64);
        assert_eq!(entry.kind, ModelRequestKind::Conversation);
        assert_eq!(entry.run_id, Some(receipt.run_id));
        assert_eq!(entry.outcome, RequestOutcome::Accepted);
    }
    assert_eq!(entries[0].usage, first_usage);
    assert_eq!(entries[1].usage, second_usage);
    assert_eq!(snapshot.chat_loop.state, LoopState::Failed);
    assert!(!snapshot.events.iter().any(|event| {
        event.run_id == receipt.run_id && matches!(event.kind, EventKind::Completed { .. })
    }));
    assert_eq!(
        snapshot.request_usage.summary().groups[0]
            .input_tokens
            .total,
        CounterTotal::Value(u128::from(u64::MAX) + 1)
    );
    assert_eq!(
        snapshot
            .request_usage
            .run_conversation_usage(receipt.run_id),
        Err(LedgerError::RunUsageOverflow(UsageCounter::Input))
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn a_failed_tool_result_returns_to_the_model_with_each_accepted_request_recorded() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("source.txt"), "short file").unwrap();
    let tool_usage = counters(10, 2, 0, 0);
    let followup_usage = counters(20, 3, 0, 0);
    let model = Arc::new(UsageModel::new(
        main_identity(),
        vec![
            ResponseStep::reply(
                vec![tool("missing", "Read", json!({"file_path":"missing.txt"}))],
                tool_usage.clone(),
            ),
            ResponseStep::reply(text("The file is missing"), followup_usage.clone()),
        ],
    ));
    let agent = Agent::new_embedded(config(&main_identity()), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent
        .submit(chat, "Read the missing file".into())
        .await
        .unwrap();
    assert!(matches!(
        terminal_event(&mut events, receipt.run_id).await.kind,
        EventKind::Completed { .. }
    ));
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(model.request_count(), 2);
    assert!(
        model.requests.lock().unwrap()[1]
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .any(
                |content| matches!(content, Content::ToolResult { tool_use_id, is_error: true, .. }
                if tool_use_id == "missing")
            )
    );
    let entries = support::model_script_requests(&snapshot);
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].usage, tool_usage);
    assert_eq!(entries[1].usage, followup_usage);
    assert!(
        entries
            .iter()
            .all(|entry| entry.outcome == RequestOutcome::Accepted)
    );
    assert_eq!(snapshot.chat_loop.state, LoopState::Idle);
    assert!(snapshot.events.iter().any(|event| {
        matches!(&event.kind, EventKind::ToolCompleted { call_id, result, .. }
            if call_id == "missing" && result.is_error)
    }));
    agent.shutdown().await;
}

#[tokio::test]
async fn manual_compaction_acceptance_and_rejection_have_no_borrowed_run_identity() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(UsageModel::new(
        main_identity(),
        vec![
            ResponseStep::reply(text("Public answer"), counters(10, 2, 0, 0)),
            ResponseStep::reply(text("Public summary"), counters(100, 20, 4, 0)),
            ResponseStep::reply(text("   "), counters(50, 1, 2, 0)),
        ],
    ));
    let agent = Agent::new_embedded(config(&main_identity()), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, "Start".into()).await.unwrap();
    assert_eq!(
        completed_usage(&mut events, receipt.run_id).await,
        counters(10, 2, 0, 0)
    );
    agent.compact(chat).await.unwrap();
    let accepted = agent.snapshot(chat).await.unwrap();
    assert!(agent.compact(chat).await.is_err());
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(
        snapshot.latest_compact_summary(),
        accepted.latest_compact_summary()
    );
    assert_eq!(snapshot.messages, accepted.messages);
    assert_eq!(model.request_count(), 3);
    let entries = support::model_script_requests(&snapshot);
    assert_eq!(entries.len(), 3);
    for record in &entries[1..] {
        assert_eq!(record.kind, ModelRequestKind::CompactionManual);
        assert_eq!(record.loop_id, None);
        assert_eq!(record.run_id, None);
    }
    assert_eq!(entries[1].outcome, RequestOutcome::Accepted);
    assert_eq!(entries[1].usage, counters(100, 20, 4, 0));
    assert_eq!(
        entries[2].outcome,
        RequestOutcome::HostRejected(ResponseRejection::EmptySummary)
    );
    assert_eq!(entries[2].usage, counters(50, 1, 2, 0));
    assert_eq!(
        snapshot
            .request_usage
            .run_conversation_usage(receipt.run_id)
            .unwrap(),
        counters(10, 2, 0, 0)
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn automatic_compaction_is_owned_by_its_run_but_excluded_from_reply_usage() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(UsageModel::new(
        main_identity(),
        vec![
            ResponseStep::reply(text(&"older material ".repeat(2_000)), counters(1, 1, 0, 0)),
            ResponseStep::reply(text("Second response"), counters(2, 2, 0, 0)),
            ResponseStep::reply(text("Third response"), counters(18_000, 3, 0, 0)),
            ResponseStep::reply(text("Compact public summary"), counters(500, 50, 30, 0)),
            ResponseStep::reply(text("Continued"), counters(20, 4, 2, 0)),
        ],
    ));
    let agent = Agent::new_embedded(config(&main_identity()), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let first = agent.submit(chat, "Keep constraints".into()).await.unwrap();
    completed_usage(&mut events, first.run_id).await;
    for prompt in ["Second step", "Third step"] {
        let receipt = agent.submit(chat, prompt.into()).await.unwrap();
        completed_usage(&mut events, receipt.run_id).await;
    }
    agent.set_context_window(chat, 20_000).await.unwrap();
    let receipt = agent.submit(chat, "Continue".into()).await.unwrap();
    assert_eq!(
        completed_usage(&mut events, receipt.run_id).await,
        counters(20, 4, 2, 0)
    );
    let snapshot = agent.snapshot(chat).await.unwrap();
    let entries = support::model_script_requests(&snapshot);
    assert_eq!(entries.len(), 5);
    assert_eq!(entries[3].kind, ModelRequestKind::CompactionAutomatic);
    assert_eq!(entries[3].loop_id, Some(receipt.loop_id));
    assert_eq!(entries[3].run_id, Some(receipt.run_id));
    assert_eq!(entries[3].outcome, RequestOutcome::Accepted);
    assert_eq!(entries[3].usage, counters(500, 50, 30, 0));
    assert_eq!(entries[4].kind, ModelRequestKind::Conversation);
    assert!(model.requests.lock().unwrap()[3].tools.is_empty());
    assert_eq!(
        snapshot.request_usage.summary().groups[0]
            .input_tokens
            .total,
        CounterTotal::Value(18_523)
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn checker_uses_its_actual_identity_and_is_excluded_from_conversation_totals() {
    for same_identity in [true, false] {
        let directory = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let result_path = external.path().join("result.txt");
        let model = Arc::new(UsageModel::new(
            main_identity(),
            vec![
                ResponseStep::reply(
                    vec![tool(
                        "write",
                        "Write",
                        json!({"file_path":result_path,"content":"done"}),
                    )],
                    counters(10, 1, 0, 0),
                ),
                ResponseStep::reply(text("Written"), counters(20, 2, 0, 0)),
            ],
        ));
        let checker_identity = if same_identity {
            main_identity()
        } else {
            identity("checker", Protocol::ChatCompletions, "checker-model")
        };
        let checker = Arc::new(UsageModel::new(
            checker_identity.clone(),
            vec![ResponseStep::reply(
                text(&tickloop_harness::test_support::auto_classification(false).to_string()),
                counters(100, 10, 5, 0),
            )],
        ));
        let agent = Agent::new_embedded(config(&main_identity()), model.clone());
        agent.set_auto_reviewer(checker.clone()).await;
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        agent
            .set_execution_mode(chat, ExecutionMode::Auto)
            .await
            .unwrap();
        let mut events = agent.subscribe();
        let receipt = agent.submit(chat, "Write result.txt".into()).await.unwrap();
        assert_eq!(
            completed_usage(&mut events, receipt.run_id).await,
            counters(30, 3, 0, 0)
        );
        let snapshot = agent.snapshot(chat).await.unwrap();
        let entries = support::model_script_requests(&snapshot);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[1].kind, ModelRequestKind::PermissionCheck);
        assert_eq!(entries[1].model, checker_identity);
        assert_eq!(entries[1].loop_id, Some(receipt.loop_id));
        assert_eq!(entries[1].run_id, Some(receipt.run_id));
        assert_eq!(entries[1].outcome, RequestOutcome::Accepted);
        assert_eq!(entries[1].usage, counters(100, 10, 5, 0));
        assert_eq!(
            snapshot.request_usage.summary().groups.len(),
            if same_identity { 1 } else { 2 }
        );
        assert_eq!(checker.request_count(), 1);
        assert_eq!(model.request_count(), 2);
        assert_eq!(std::fs::read_to_string(&result_path).unwrap(), "done");
        assert!(!snapshot.messages.iter().flat_map(|message| &message.content).any(|content| matches!(content, Content::Text { text } if text.contains("bounded to the workspace"))));
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn invalid_checker_verdict_retains_usage_and_blocks_the_call() {
    let directory = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    let blocked_path = external.path().join("blocked.txt");
    let model = Arc::new(UsageModel::new(
        main_identity(),
        vec![
            ResponseStep::reply(
                vec![tool(
                    "write",
                    "Write",
                    json!({"file_path":blocked_path,"content":"must wait"}),
                )],
                counters(10, 1, 0, 0),
            ),
            ResponseStep::reply(text("Done"), counters(20, 2, 0, 0)),
        ],
    ));
    let checker = Arc::new(UsageModel::new(
        identity("review", Protocol::Responses, "review-model"),
        vec![
            ResponseStep::reply(text("not a verdict"), counters(40, 4, 0, 0)),
            ResponseStep::reply(text("still not a verdict"), counters(40, 4, 0, 0)),
            ResponseStep::reply(text("authorization also invalid"), counters(40, 4, 0, 0)),
        ],
    ));
    let agent = Agent::new_with_options(
        config(&main_identity()),
        model,
        tickloop_harness::protocol::AgentHostOptions {
            prompt_policy: tickloop_harness::protocol::PromptPolicy::DenyOnPrompt,
            ..tickloop_harness::protocol::AgentHostOptions::embedded_defaults()
        },
    );
    agent.set_auto_reviewer(checker.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Auto)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let receipt = agent
        .submit(chat, "Write if approved".into())
        .await
        .unwrap();
    assert!(matches!(
        terminal_event(&mut events, receipt.run_id).await.kind,
        EventKind::Completed { .. }
    ));
    let snapshot = agent.snapshot(chat).await.unwrap();
    for entry in &support::model_script_requests(&snapshot)[1..=3] {
        assert_eq!(entry.kind, ModelRequestKind::PermissionCheck);
        assert_eq!(
            entry.outcome,
            RequestOutcome::HostRejected(ResponseRejection::InvalidVerdict)
        );
        assert_eq!(entry.usage, counters(40, 4, 0, 0));
    }
    assert!(!blocked_path.exists());
    assert_eq!(checker.request_count(), 3);
    agent.shutdown().await;
}

#[tokio::test]
async fn operations_not_needing_review_create_no_auto_review_request_record() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("source.txt"), "source").unwrap();
    let model = Arc::new(UsageModel::new(
        main_identity(),
        vec![
            ResponseStep::reply(
                vec![tool("operation", "Read", json!({"file_path":"source.txt"}))],
                counters(10, 1, 0, 0),
            ),
            ResponseStep::reply(text("Done"), counters(20, 2, 0, 0)),
        ],
    ));
    let reviewer = Arc::new(UsageModel::new(main_identity(), Vec::new()));
    let agent = Agent::new_embedded(config(&main_identity()), model.clone());
    agent.set_auto_reviewer(reviewer.clone()).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, "Read source.txt".into()).await.unwrap();
    completed_usage(&mut events, receipt.run_id).await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(
        support::model_script_requests(&snapshot)
            .iter()
            .all(|record| record.kind == ModelRequestKind::Conversation)
    );
    assert_eq!(
        support::model_script_requests(&snapshot).len(),
        model.request_count()
    );
    assert_eq!(reviewer.request_count(), 0);
    agent.shutdown().await;
}

#[tokio::test]
async fn switching_models_never_relabels_old_requests() {
    let directory = tempfile::tempdir().unwrap();
    let first_model = Arc::new(UsageModel::new(
        main_identity(),
        vec![ResponseStep::reply(text("First"), counters(10, 1, 0, 0))],
    ));
    let models = ChosenModels::new(first_model.clone());
    let agent = embedded_agent(config(&main_identity()), models.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let first = agent.submit(chat, "First request".into()).await.unwrap();
    completed_usage(&mut events, first.run_id).await;
    let next_identity = identity("same-provider-responses", Protocol::Responses, "next-model");
    let next_model = Arc::new(UsageModel::new(
        next_identity.clone(),
        vec![ResponseStep::reply(text("Second"), counters(20, 2, 0, 0))],
    ));
    models.choose(next_model.clone());
    assert_eq!(
        agent.set_model(chat, config(&next_identity)).await.unwrap(),
        ModelChange::Applied
    );
    let second = agent.submit(chat, "Next request".into()).await.unwrap();
    completed_usage(&mut events, second.run_id).await;
    let before = agent.snapshot(chat).await.unwrap();
    assert_eq!(
        support::model_script_requests(&before)[0].model,
        main_identity()
    );
    assert_eq!(
        support::model_script_requests(&before)[1].model,
        next_identity
    );
    assert_eq!(before.request_usage.summary().groups.len(), 2);
    assert_eq!(first_model.request_count(), 1);
    assert_eq!(next_model.request_count(), 1);
    agent.shutdown().await;
}

#[tokio::test]
async fn cancelling_after_usage_was_queued_drains_and_terminalizes_the_same_request() {
    let directory = tempfile::tempdir().unwrap();
    let entered = Arc::new(Notify::new());
    let usage = Usage {
        input_tokens: Some(55),
        cache_read_input_tokens: Some(0),
        ..Usage::default()
    };
    let model = Arc::new(UsageModel::new(
        main_identity(),
        vec![ResponseStep::pending(usage.clone(), entered.clone())],
    ));
    let agent = Agent::new_embedded(config(&main_identity()), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, "Start a request".into()).await.unwrap();
    tokio::time::timeout(EVENT_TIMEOUT, entered.notified())
        .await
        .unwrap();
    agent.interrupt(chat).await.unwrap();
    let terminal = terminal_event(&mut events, receipt.run_id).await;
    assert!(!matches!(terminal.kind, EventKind::Completed { .. }));
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(support::model_script_requests(&snapshot).len(), 1);
    assert_eq!(
        support::model_script_requests(&snapshot)[0].outcome,
        RequestOutcome::Cancelled
    );
    assert_eq!(support::model_script_requests(&snapshot)[0].usage, usage);
    assert_eq!(model.request_count(), 1);
    agent.shutdown().await;
}

#[tokio::test]
async fn host_rejection_and_usage_regression_retain_observed_facts_without_completion() {
    let directory = tempfile::tempdir().unwrap();
    let first_usage = counters(30, 3, 0, 0);
    let mut regression = ResponseStep::reply(text("must not complete"), Usage::default());
    regression.observations = vec![
        Usage {
            input_tokens: Some(100),
            ..Usage::default()
        },
        Usage {
            input_tokens: Some(90),
            output_tokens: Some(20),
            ..Usage::default()
        },
    ];
    let model = Arc::new(UsageModel::new(
        main_identity(),
        vec![
            ResponseStep::reply(
                vec![
                    tool("duplicate", "Read", json!({"file_path":"a.txt"})),
                    tool("duplicate", "Read", json!({"file_path":"b.txt"})),
                ],
                first_usage.clone(),
            ),
            regression,
        ],
    ));
    let agent = Agent::new_embedded(config(&main_identity()), model);
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    for prompt in ["Invalid tool response", "Regressed usage"] {
        let receipt = agent.submit(chat, prompt.into()).await.unwrap();
        assert!(matches!(
            terminal_event(&mut events, receipt.run_id).await.kind,
            EventKind::Failed(_)
        ));
    }
    let snapshot = agent.snapshot(chat).await.unwrap();
    let entries = support::model_script_requests(&snapshot);
    assert_eq!(entries.len(), 2);
    assert_eq!(
        entries[0].outcome,
        RequestOutcome::HostRejected(ResponseRejection::InvalidToolCall)
    );
    assert_eq!(entries[0].usage, first_usage);
    assert_eq!(
        entries[1].outcome,
        RequestOutcome::Failed(RequestFailure::Protocol(ProtocolFailure::UsageRegression(
            UsageCounter::Input
        )))
    );
    assert_eq!(entries[1].usage.input_tokens, Some(100));
    assert_eq!(entries[1].usage.output_tokens, Some(20));
    assert!(snapshot.operations.is_empty());
    assert!(
        !snapshot
            .events
            .iter()
            .any(|event| matches!(event.kind, EventKind::Completed { .. }))
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn failed_requests_keep_reported_usage_and_leave_missing_counters_unknown() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(UsageModel::new(
        main_identity(),
        vec![
            ResponseStep {
                observations: Vec::new(),
                result: Err(ModelError::Http { status: 400 }),
                wait_for_cancel: None,
                usage_separated_text_chunks: false,
            },
            ResponseStep {
                observations: vec![Usage {
                    input_tokens: Some(50),
                    ..Usage::default()
                }],
                result: Err(ModelError::ProviderFailure),
                wait_for_cancel: None,
                usage_separated_text_chunks: false,
            },
        ],
    ));
    let agent = Agent::new_embedded(config(&main_identity()), model);
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    for prompt in ["HTTP failure", "Provider failure"] {
        let receipt = agent.submit(chat, prompt.into()).await.unwrap();
        assert!(matches!(
            terminal_event(&mut events, receipt.run_id).await.kind,
            EventKind::Failed(_)
        ));
    }
    let snapshot = agent.snapshot(chat).await.unwrap();
    let entries = support::model_script_requests(&snapshot);
    assert_eq!(
        entries[0].outcome,
        RequestOutcome::Failed(RequestFailure::Http { status: 400 })
    );
    assert_eq!(entries[0].usage, Usage::default());
    assert_eq!(
        entries[1].outcome,
        RequestOutcome::Failed(RequestFailure::ProviderFailure)
    );
    assert_eq!(
        entries[1].usage,
        Usage {
            input_tokens: Some(50),
            ..Usage::default()
        }
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn evicting_old_completed_events_preserves_all_request_records_and_totals() {
    let directory = tempfile::tempdir().unwrap();
    let archive = directory.path().join("archive");
    let mut noisy = ResponseStep::reply(text(&"x".repeat(1_100)), counters(20, 2, 0, 0));
    noisy.usage_separated_text_chunks = true;
    let model = Arc::new(UsageModel::new(
        main_identity(),
        vec![
            ResponseStep::reply(text("First"), counters(10, 1, 0, 0)),
            noisy,
        ],
    ));
    let agent =
        Agent::open_embedded(archive.clone(), config(&main_identity()), model.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let first = agent.submit(chat, "First".into()).await.unwrap();
    let first_completed = terminal_event(&mut events, first.run_id).await;
    assert!(matches!(first_completed.kind, EventKind::Completed { .. }));
    let second = agent.submit(chat, "Noisy reply".into()).await.unwrap();
    completed_usage(&mut events, second.run_id).await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.events.len(), 1_024);
    assert!(
        snapshot
            .events
            .iter()
            .all(|event| event.sequence > first_completed.sequence)
    );
    assert_eq!(support::model_script_requests(&snapshot).len(), 2);
    assert_eq!(
        snapshot.request_usage.summary().groups[0]
            .input_tokens
            .total,
        CounterTotal::Value(30)
    );
    assert_eq!(model.request_count(), 2);
    agent.shutdown().await;
}
