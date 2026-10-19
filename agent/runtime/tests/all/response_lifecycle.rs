//! Answer lifecycle records, the input ledger and the display projection derived from Lines.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use image::{DynamicImage, ImageFormat, Rgb, RgbImage};
use tickloop_agent_base::image::prepare;
use tickloop_agent_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_agent_protocol::{
    AgentHostOptions, ChatId, Content, DirectedSummaryRequest, Event, EventKind, HostCallContext,
    ImageContext, ImageOrigin, InputContext, LoopState, Message, ModelIdentity, Protocol, Receipt,
    RewindMode, Role, SummaryDirection, Trigger, TriggerPromptSource, Usage,
};
use tickloop_agent_runtime::transcript::{
    AppendBatch, BatchId, ChatPlacement, JsonlTranscriptStore, LoadRange, StorePosition,
    TranscriptStore,
};
use tickloop_agent_runtime::{
    Agent, AgentError, Delivery, HarnessOptions, HarnessSeams, ParentSelection, ToolPolicy, TurnOp,
    TurnRequest, UserMessage, environment::LocalEnvironment,
};
use tickloop_agent_test_support::transcripts::InMemoryTranscriptStore;
use tickloop_agent_test_support::{call, config, next_event, text};
use tickloop_agent_transcript::display::{
    DisplayBlock, DisplayKind, DisplayNode, DisplayProjector, DisplayStatus, project,
};
use tickloop_agent_transcript::{
    DiscardReason, InputEntry, InputOp, InputState, InterruptedBy, Line, ResponseEnding,
    ResponseEntry, ResponseState, SettledOutcome,
};
use tokio::sync::{Notify, mpsc::UnboundedSender};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

enum Step {
    Reply(Vec<Content>),
    /// Streams `delta` when given, then waits until cancelled.
    Hang {
        delta: Option<String>,
    },
    /// Waits for `release` before replying.
    Gate(Vec<Content>),
}

/// A model that follows a script of steps and announces each request it takes.
struct StagedModel {
    steps: Mutex<VecDeque<Step>>,
    requests: Mutex<Vec<ModelRequest>>,
    reached: Notify,
    release: Notify,
}

impl StagedModel {
    fn new(steps: Vec<Step>) -> Arc<Self> {
        Arc::new(Self {
            steps: Mutex::new(steps.into()),
            requests: Mutex::new(Vec::new()),
            reached: Notify::new(),
            release: Notify::new(),
        })
    }

    fn requests(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

#[async_trait]
impl ModelClient for StagedModel {
    async fn generate_title(
        &self,
        request: ModelRequest,
        _: UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        Ok(tickloop_agent_test_support::title_output(request))
    }

    fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            provider: tickloop_agent_protocol::ModelProvider::Api,
            profile: "fixture".into(),
            protocol: Protocol::Messages,
            requested_model: "fixture".into(),
        }
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
            .ok_or_else(|| ModelError::InvalidResponse("script exhausted".into()))?;
        let content = match step {
            Step::Reply(content) => content,
            Step::Hang { delta } => {
                if let Some(delta) = delta {
                    let _ = events.send(ModelDelta::Text(delta));
                }
                self.reached.notify_one();
                cancel.cancelled().await;
                return Err(ModelError::Cancelled);
            }
            Step::Gate(content) => {
                self.reached.notify_one();
                tokio::select! {
                    _ = self.release.notified() => {}
                    _ = cancel.cancelled() => return Err(ModelError::Cancelled),
                }
                content
            }
        };
        for block in &content {
            if let Content::Text { text } = block {
                let _ = events.send(ModelDelta::Text(text.clone()));
            }
        }
        Ok(ModelOutput {
            private_source: None,
            content,
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

struct Fixture {
    folder: PathBuf,
    _directory: tempfile::TempDir,
}

fn fixture() -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let folder = directory.path().join("folder");
    std::fs::create_dir_all(&folder).unwrap();
    Fixture {
        folder: folder.canonicalize().unwrap(),
        _directory: directory,
    }
}

fn agent(store: Arc<dyn TranscriptStore>, model: Arc<dyn ModelClient>) -> Agent {
    Agent::with_seams(
        HarnessSeams {
            transcripts: Some(store),
            environment: Some(Arc::new(LocalEnvironment::bare())),
            models: tickloop_agent_test_support::harness::ChosenModels::new(model),
        },
        HarnessOptions {
            model: config(),
            settings: None,
            host: AgentHostOptions::embedded_defaults(),
            tools: ToolPolicy::local(),
            host_rules: Vec::new(),
            task_tools: None,
            min_schedule_interval: tickloop_agent_runtime::LOCAL_MIN_SCHEDULE_INTERVAL,
            waits: tickloop_agent_runtime::WaitHosting::Process,
        },
    )
    .unwrap()
}

/// Queues a prompt after the current leaf under the host's input id.
#[allow(clippy::too_many_arguments)]
async fn submit_input(
    agent: &Agent,
    chat: ChatId,
    input: Uuid,
    goal: Option<Uuid>,
    text: String,
    contexts: Vec<InputContext>,
    paths: Vec<PathBuf>,
    host_context: Option<HostCallContext>,
) -> Result<Receipt, AgentError> {
    let turn = agent
        .advance(
            chat,
            TurnRequest {
                input_id: input,
                op: TurnOp::Continue {
                    after: ParentSelection::CurrentLeaf,
                    target_loop: goal,
                    message: UserMessage {
                        text,
                        contexts,
                        paths,
                    },
                },
                delivery: Delivery::Queue,
                host_context,
            },
        )
        .await?;
    Ok(turn.receipt().clone())
}

async fn submit(agent: &Agent, chat: ChatId, input: Uuid, goal: Option<Uuid>, prompt: &str) {
    submit_input(
        agent,
        chat,
        input,
        goal,
        prompt.into(),
        Vec::new(),
        Vec::new(),
        None,
    )
    .await
    .unwrap();
}

async fn until_ended(
    events: &mut tokio::sync::broadcast::Receiver<Event>,
    run: Uuid,
) -> Vec<Event> {
    let mut seen = Vec::new();
    loop {
        let event = next_event(events).await;
        if event.run_id != run {
            continue;
        }
        let ended = matches!(
            event.kind,
            EventKind::Completed { .. } | EventKind::Failed(_) | EventKind::Interrupted
        );
        seen.push(event);
        if ended {
            return seen;
        }
    }
}

fn responses(lines: &[Line]) -> Vec<ResponseEntry> {
    lines
        .iter()
        .filter_map(|line| match line {
            Line::Response(entry) => Some(entry.clone()),
            _ => None,
        })
        .collect()
}

fn inputs(lines: &[Line], id: Uuid) -> Vec<InputEntry> {
    lines
        .iter()
        .filter_map(|line| match line {
            Line::Input(entry) if entry.input_id == id => Some(entry.clone()),
            _ => None,
        })
        .collect()
}

fn kind_name(kind: &EventKind) -> String {
    serde_json::to_value(kind)
        .unwrap()
        .as_object()
        .map(|object| object.keys().next().unwrap().clone())
        .unwrap_or_else(|| serde_json::to_value(kind).unwrap().as_str().unwrap().into())
}

fn pasted_image(id: u32) -> InputContext {
    let pixels = RgbImage::from_fn(4, 2, |x, y| Rgb([x as u8 * 60, y as u8 * 90, 30]));
    let mut bytes = Vec::new();
    DynamicImage::ImageRgb8(pixels)
        .write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Png)
        .unwrap();
    InputContext::Image(ImageContext {
        origin: ImageOrigin::Pasted { id },
        image: prepare(&bytes).unwrap(),
    })
}

fn prompts_and_answers(nodes: &[DisplayNode]) -> (Vec<&DisplayNode>, Vec<&DisplayNode>) {
    nodes
        .iter()
        .partition(|node| matches!(node.kind, DisplayKind::Prompt { .. }))
}

fn texts(node: &DisplayNode) -> Vec<String> {
    node.blocks
        .iter()
        .filter_map(|block| match block {
            DisplayBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_turn_adds_only_response_started_to_its_events_and_records_its_lifecycle() {
    let fixture = fixture();
    let store = Arc::new(InMemoryTranscriptStore::new());
    let agent = agent(
        store.clone(),
        StagedModel::new(vec![Step::Reply(text("hello"))]),
    );
    let chat = agent.create_chat(fixture.folder.clone()).await.unwrap();
    let mut events = agent.subscribe();
    let input = Uuid::new_v4();
    submit(&agent, chat, input, None, "greet me").await;
    let state = agent.input_state(chat, input).await.unwrap().unwrap();
    let seen = until_ended(&mut events, state.entry.run_id).await;
    assert_eq!(
        seen.iter()
            .map(|event| kind_name(&event.kind))
            .collect::<Vec<_>>(),
        [
            "ResponseStarted",
            "Started",
            "Text",
            "AssistantMessageCommitted",
            "Completed"
        ]
    );
    assert!(
        seen.iter()
            .all(|event| event.kind.is_transient() == matches!(event.kind, EventKind::Text(_)))
    );
    let EventKind::ResponseStarted {
        response_id,
        attempt,
        display_parent,
    } = seen[0].kind
    else {
        unreachable!()
    };
    assert_eq!((attempt, display_parent), (1, Some(input)));
    let started_sequence = seen[0].sequence;

    assert_eq!(
        agent.settled_input(chat, input).await.unwrap(),
        SettledOutcome::Completed {
            usage: Usage::default()
        }
    );
    agent.sync().await.unwrap();
    // The batch carrying the start covers its event, so a publisher releases the answer's
    // deltas only after the start is committed.
    let carrying = store
        .received(chat)
        .into_iter()
        .find(|batch| {
            batch.lines.iter().any(|line| {
                matches!(line, Line::Response(entry) if entry.response_id == response_id
                    && entry.state == ResponseState::Started)
            })
        })
        .unwrap();
    assert!(carrying.events_through >= started_sequence);
    let lines = store.lines(chat);
    let records = responses(&lines);
    assert_eq!(records.last().unwrap().response_id, response_id);
    assert_eq!(
        records.last().unwrap().state,
        ResponseState::Ended {
            ending: ResponseEnding::Completed
        }
    );
    assert_eq!(
        inputs(&lines, input).last().unwrap().state,
        InputState::Settled {
            outcome: SettledOutcome::Completed {
                usage: Usage::default()
            }
        }
    );
    let prompt = lines
        .iter()
        .find_map(|line| match line {
            Line::User(entry) if entry.uuid == input => Some(entry),
            _ => None,
        })
        .expect("the prompt's id is its input id");
    assert_eq!(prompt.input_id, Some(input));
    assert!(lines.iter().any(
        |line| matches!(line, Line::Assistant(entry) if entry.response_id == Some(response_id))
    ));
    agent.shutdown().await;
}

/// Starts a turn whose first model request hangs, after streaming `delta` when given, and
/// returns what the store holds at that moment, as a crash then would leave it.
async fn crash_while_streaming(delta: Option<&str>) {
    let fixture = fixture();
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = StagedModel::new(vec![Step::Hang {
        delta: delta.map(str::to_owned),
    }]);
    let first = agent(store.clone(), model.clone());
    let chat = first.create_chat(fixture.folder.clone()).await.unwrap();
    let mut events = first.subscribe();
    let input = Uuid::new_v4();
    submit(&first, chat, input, None, "a question").await;
    if delta.is_some() {
        loop {
            if matches!(next_event(&mut events).await.kind, EventKind::Text(_)) {
                break;
            }
        }
    } else {
        model.reached.notified().await;
    }
    let committed = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while responses(&store.lines(chat)).is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(
        committed.is_ok(),
        "the start reaches the store without a flush interval"
    );
    let crashed = store.lines(chat);
    let started = responses(&crashed);
    assert_eq!(
        started.len(),
        1,
        "the start is stored before the answer streams"
    );
    assert_eq!(started[0].state, ResponseState::Started);
    assert!(
        !crashed
            .iter()
            .any(|line| matches!(line, Line::Assistant(_)))
    );

    let recovered = Arc::new(InMemoryTranscriptStore::new());
    recovered.seed(chat, crashed, StorePosition(1));
    let next = agent(recovered.clone(), StagedModel::new(Vec::new()));
    next.resume_chat(chat).await.unwrap();
    let state = next.input_state(chat, input).await.unwrap().unwrap();
    assert_eq!(state.entry.state, InputState::Accepted);
    assert!(!state.is_live);
    assert!(state.prompt_in_conversation);
    assert_eq!(
        state.responses[0].state,
        ResponseState::Ended {
            ending: ResponseEnding::Interrupted {
                by: InterruptedBy::Host
            }
        }
    );
    let lines = recovered.lines(chat);
    let nodes = project(&lines);
    let (prompts, answers) = prompts_and_answers(&nodes);
    assert_eq!(prompts.len(), 1);
    assert_eq!(answers.len(), 1);
    assert_eq!(answers[0].parent, Some(input));
    assert_eq!(
        answers[0].status,
        DisplayStatus::Interrupted {
            by: InterruptedBy::Host
        }
    );
    next.shutdown().await;
    first.shutdown().await;
}

#[tokio::test]
async fn a_crash_before_the_first_delta_ends_the_started_answer_on_load() {
    crash_while_streaming(None).await;
}

#[tokio::test]
async fn a_crash_after_a_delta_ends_the_started_answer_on_load() {
    crash_while_streaming(Some("partial")).await;
}

#[tokio::test]
async fn a_crash_after_content_was_committed_keeps_that_content_in_the_interrupted_answer() {
    let fixture = fixture();
    std::fs::write(fixture.folder.join("notes.txt"), "the notes").unwrap();
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = StagedModel::new(vec![
        Step::Reply(call(
            "read",
            "Read",
            serde_json::json!({"file_path": "notes.txt"}),
        )),
        Step::Hang { delta: None },
    ]);
    let first = agent(store.clone(), model.clone());
    let chat = first.create_chat(fixture.folder.clone()).await.unwrap();
    let input = Uuid::new_v4();
    submit(&first, chat, input, None, "read the notes").await;
    model.reached.notified().await;
    first.flush_transcripts().await;
    let crashed = store.lines(chat);

    let recovered = Arc::new(InMemoryTranscriptStore::new());
    recovered.seed(chat, crashed, StorePosition(1));
    let next = agent(recovered.clone(), StagedModel::new(Vec::new()));
    next.resume_chat(chat).await.unwrap();
    let nodes = project(&recovered.lines(chat));
    let (_, answers) = prompts_and_answers(&nodes);
    assert_eq!(answers.len(), 1);
    assert!(matches!(
        &answers[0].blocks[..],
        [DisplayBlock::ToolCall { name, result: Some(result), .. }]
            if name == "Read" && result.text.contains("the notes")
    ));
    assert_eq!(
        answers[0].status,
        DisplayStatus::Interrupted {
            by: InterruptedBy::Host
        }
    );
    assert_eq!(
        next.input_state(chat, input)
            .await
            .unwrap()
            .unwrap()
            .entry
            .state,
        InputState::Accepted
    );
    next.shutdown().await;
    first.shutdown().await;
}

#[tokio::test]
async fn an_input_settled_before_a_crash_is_not_run_again() {
    let fixture = fixture();
    let store = Arc::new(InMemoryTranscriptStore::new());
    let first = agent(
        store.clone(),
        StagedModel::new(vec![Step::Reply(text("done"))]),
    );
    let chat = first.create_chat(fixture.folder.clone()).await.unwrap();
    let input = Uuid::new_v4();
    submit(&first, chat, input, None, "do it once").await;
    first.settled_input(chat, input).await.unwrap();
    let original = first.input_state(chat, input).await.unwrap().unwrap();
    first.sync().await.unwrap();

    let model = StagedModel::new(Vec::new());
    let next = agent(store.clone(), model.clone());
    next.resume_chat(chat).await.unwrap();
    let receipt = submit_input(
        &next,
        chat,
        input,
        None,
        "do it once".into(),
        Vec::new(),
        Vec::new(),
        None,
    )
    .await
    .unwrap();
    assert_eq!(receipt.run_id, original.entry.run_id);
    assert!(matches!(
        next.input_state(chat, input)
            .await
            .unwrap()
            .unwrap()
            .entry
            .state,
        InputState::Settled {
            outcome: SettledOutcome::Completed { .. }
        }
    ));
    assert!(matches!(
        submit_input(
            &next,
            chat,
            input,
            None,
            "something else".into(),
            Vec::new(),
            Vec::new(),
            None
        )
        .await,
        Err(AgentError::Invalid(_))
    ));
    assert_eq!(model.requests(), 0);
    next.shutdown().await;
    first.shutdown().await;
}

#[tokio::test]
async fn a_taken_back_input_settles_withdrawn() {
    let fixture = fixture();
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = StagedModel::new(vec![Step::Hang { delta: None }]);
    let agent = agent(store.clone(), model.clone());
    let chat = agent.create_chat(fixture.folder.clone()).await.unwrap();
    let active = Uuid::new_v4();
    submit(&agent, chat, active, None, "hold the execution slot").await;
    model.reached.notified().await;
    let loop_id = agent
        .input_state(chat, active)
        .await
        .unwrap()
        .unwrap()
        .entry
        .loop_id;
    let queued = Uuid::new_v4();
    submit(&agent, chat, queued, Some(loop_id), "edit this message").await;
    let waiting = {
        let agent = agent.clone();
        tokio::spawn(async move { agent.settled_input(chat, queued).await })
    };

    let taken = agent.take_queued_inputs(chat, true).await.unwrap();
    assert_eq!(taken.len(), 1);
    assert_eq!(waiting.await.unwrap().unwrap(), SettledOutcome::Withdrawn);
    agent.sync().await.unwrap();
    assert_eq!(
        inputs(&store.lines(chat), queued).last().unwrap().state,
        InputState::Settled {
            outcome: SettledOutcome::Withdrawn
        }
    );
    // Submitting the same input again returns the withdrawn outcome instead of queuing it.
    submit(&agent, chat, queued, Some(loop_id), "edit this message").await;
    assert!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .queued_inputs()
            .is_empty()
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn stopping_a_loop_discards_its_queue_and_interrupting_settles_the_running_input() {
    let fixture = fixture();
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = StagedModel::new(vec![Step::Hang { delta: None }, Step::Hang { delta: None }]);
    let agent = agent(store.clone(), model.clone());
    let chat = agent.create_chat(fixture.folder.clone()).await.unwrap();

    let interrupted = Uuid::new_v4();
    submit(&agent, chat, interrupted, None, "interrupt me").await;
    model.reached.notified().await;
    agent.interrupt(chat).await.unwrap();
    assert_eq!(
        agent.settled_input(chat, interrupted).await.unwrap(),
        SettledOutcome::Interrupted {
            by: InterruptedBy::User
        }
    );

    let running = Uuid::new_v4();
    submit(&agent, chat, running, None, "stop me").await;
    model.reached.notified().await;
    let loop_id = agent
        .input_state(chat, running)
        .await
        .unwrap()
        .unwrap()
        .entry
        .loop_id;
    let queued = Uuid::new_v4();
    submit(&agent, chat, queued, Some(loop_id), "never runs").await;
    agent.stop(chat, loop_id).await.unwrap();
    assert_eq!(
        agent.settled_input(chat, queued).await.unwrap(),
        SettledOutcome::Discarded {
            reason: DiscardReason::Stopped
        }
    );
    assert_eq!(
        agent.settled_input(chat, running).await.unwrap(),
        SettledOutcome::Interrupted {
            by: InterruptedBy::User
        }
    );
    agent.sync().await.unwrap();
    let mut latest = std::collections::HashMap::new();
    for response in responses(&store.lines(chat)) {
        latest.insert(response.response_id, response.state);
    }
    assert_eq!(latest.len(), 2);
    assert!(latest.values().all(|state| *state
        == ResponseState::Ended {
            ending: ResponseEnding::Interrupted {
                by: InterruptedBy::User
            }
        }));
    assert_eq!(model.requests(), 2);
    agent.shutdown().await;
}

#[tokio::test]
async fn prompts_coalesced_before_a_start_chain_and_the_answer_hangs_under_the_last() {
    let fixture = fixture();
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = StagedModel::new(vec![
        Step::Hang { delta: None },
        Step::Reply(text("one answer for both")),
    ]);
    let agent = agent(store.clone(), model.clone());
    let chat = agent.create_chat(fixture.folder.clone()).await.unwrap();
    let holder = Uuid::new_v4();
    submit(&agent, chat, holder, None, "hold the slot").await;
    model.reached.notified().await;
    let first = Uuid::new_v4();
    submit(&agent, chat, first, None, "first half").await;
    let loop_id = agent
        .input_state(chat, first)
        .await
        .unwrap()
        .unwrap()
        .entry
        .loop_id;
    let second = Uuid::new_v4();
    submit(&agent, chat, second, Some(loop_id), "second half").await;
    agent.interrupt(chat).await.unwrap();
    let first_run = agent
        .input_state(chat, first)
        .await
        .unwrap()
        .unwrap()
        .entry
        .run_id;
    assert!(matches!(
        agent.settled_input(chat, first).await.unwrap(),
        SettledOutcome::Completed { .. }
    ));
    assert_eq!(
        agent.settled_input(chat, second).await.unwrap(),
        SettledOutcome::Merged { into: first_run }
    );
    agent.sync().await.unwrap();

    let nodes = project(&store.lines(chat));
    let first_node = nodes.iter().find(|node| node.id == first).unwrap();
    let second_node = nodes.iter().find(|node| node.id == second).unwrap();
    assert_eq!(second_node.parent, Some(first));
    let answer = nodes
        .iter()
        .find(|node| texts(node) == ["one answer for both"])
        .unwrap();
    assert_eq!(answer.parent, Some(second));
    assert_eq!(answer.status, DisplayStatus::Complete);
    let holder_answer = nodes
        .iter()
        .find(|node| node.parent == Some(holder))
        .unwrap();
    assert_eq!(first_node.parent, Some(holder_answer.id));
    agent.shutdown().await;
}

#[tokio::test]
async fn a_prompt_merged_at_a_tool_boundary_is_a_follow_up_inside_the_answer() {
    let fixture = fixture();
    std::fs::write(fixture.folder.join("notes.txt"), "the notes").unwrap();
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = StagedModel::new(vec![
        Step::Gate(call(
            "read",
            "Read",
            serde_json::json!({"file_path": "notes.txt"}),
        )),
        Step::Reply(vec![
            Content::Thinking {
                thinking: "visible reasoning".into(),
                signature: Some("signature".into()),
            },
            Content::ProtocolData {
                protocol: Protocol::Responses,
                value: serde_json::json!({"encrypted_content": "private state"}),
            },
            Content::Text {
                text: "read it, and noted the follow-up".into(),
            },
        ]),
    ]);
    let agent = agent(store.clone(), model.clone());
    let chat = agent.create_chat(fixture.folder.clone()).await.unwrap();
    let first = Uuid::new_v4();
    submit(&agent, chat, first, None, "read the notes").await;
    model.reached.notified().await;
    let state = agent.input_state(chat, first).await.unwrap().unwrap();
    let follow_up = Uuid::new_v4();
    let image = pasted_image(1);
    submit_input(
        &agent,
        chat,
        follow_up,
        Some(state.entry.loop_id),
        "also mention the date".into(),
        vec![image.clone()],
        Vec::new(),
        None,
    )
    .await
    .unwrap();
    model.release.notify_one();
    assert_eq!(
        agent.settled_input(chat, follow_up).await.unwrap(),
        SettledOutcome::Merged {
            into: state.entry.run_id
        }
    );
    agent.settled_input(chat, first).await.unwrap();
    agent.sync().await.unwrap();

    let lines = store.lines(chat);
    let nodes = project(&lines);
    let (prompts, answers) = prompts_and_answers(&nodes);
    assert_eq!(prompts.len(), 1);
    assert_eq!(answers.len(), 1);
    let answer = answers[0];
    assert_eq!(answer.parent, Some(first));
    assert!(matches!(
        &answer.blocks[..],
        [
            DisplayBlock::ToolCall { result: Some(_), .. },
            DisplayBlock::FollowUp { prompt_id, input_id, text, images },
            DisplayBlock::Thinking { text: thinking },
            DisplayBlock::Text { text: final_text },
        ] if *prompt_id == follow_up
            && *input_id == Some(follow_up)
            && text == "also mention the date"
            && images.iter().map(|image| InputContext::Image(image.clone())).collect::<Vec<_>>() == [image.clone()]
            && thinking == "visible reasoning"
            && final_text == "read it, and noted the follow-up"
    ));
    let serialized = serde_json::to_string(&nodes).unwrap();
    assert!(!serialized.contains("private state"));
    assert!(
        !serialized.contains("signature"),
        "thinking signatures stay out"
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn compaction_keeps_display_ids_stable_and_projects_nothing_twice() {
    let fixture = fixture();
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = StagedModel::new(vec![
        Step::Reply(text("first answer")),
        Step::Reply(text("full summary")),
        Step::Reply(text("second answer")),
        Step::Reply(text("third answer")),
        Step::Reply(text("directed summary")),
    ]);
    let agent = agent(store.clone(), model.clone());
    let chat = agent.create_chat(fixture.folder.clone()).await.unwrap();
    let prompts = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
    submit(&agent, chat, prompts[0], None, "first prompt").await;
    agent.settled_input(chat, prompts[0]).await.unwrap();
    agent.compact(chat).await.unwrap();
    submit(&agent, chat, prompts[1], None, "second prompt").await;
    agent.settled_input(chat, prompts[1]).await.unwrap();
    submit(&agent, chat, prompts[2], None, "third prompt").await;
    agent.settled_input(chat, prompts[2]).await.unwrap();
    agent.sync().await.unwrap();
    let before = project(&store.lines(chat));
    let third_answer = before
        .iter()
        .find(|node| texts(node) == ["third answer"])
        .unwrap()
        .id;
    let third_endpoint = agent.physical_endpoint(chat, prompts[2]).await.unwrap();
    assert_eq!(third_endpoint, Some(prompts[2]));

    agent
        .summarize_checkpoint(
            chat,
            DirectedSummaryRequest {
                message_id: prompts[2],
                direction: SummaryDirection::UpToHere,
                guidance: None,
            },
        )
        .await
        .unwrap();
    agent.sync().await.unwrap();
    let rewritten = agent
        .physical_endpoint(chat, prompts[2])
        .await
        .unwrap()
        .unwrap();
    assert_ne!(rewritten, prompts[2], "the prompt was rewritten");
    assert!(
        agent
            .physical_endpoint(chat, third_answer)
            .await
            .unwrap()
            .is_some()
    );

    let lines = store.lines(chat);
    let after = project(&lines);
    assert_eq!(after.len(), before.len(), "{after:#?}");
    for node in &before {
        let same = after.iter().find(|other| other.id == node.id).unwrap();
        assert_eq!(same.blocks, node.blocks);
        assert_eq!(same.parent, node.parent);
    }
    let (display_prompts, answers) = prompts_and_answers(&after);
    assert_eq!(
        display_prompts
            .iter()
            .map(|node| node.id)
            .collect::<Vec<_>>(),
        prompts
    );
    assert_eq!(answers.len(), 3);
    for answer in answers {
        assert!(
            responses(&lines)
                .iter()
                .any(|response| response.response_id == answer.id)
        );
    }

    // A load from the latest boundary holds the lifecycle of every answer on the chain it loads
    // (H09 §4.6): an answer a full compaction summarized is left to a Full read, which still
    // shows it.
    let tail = store
        .load(chat, LoadRange::FromLatestBoundary)
        .await
        .unwrap()
        .unwrap();
    assert!(tail.lines.len() < lines.len());
    let tail_nodes = project(
        &tail
            .retained
            .into_iter()
            .chain(tail.lines)
            .collect::<Vec<_>>(),
    );
    let first_answer = before
        .iter()
        .find(|node| texts(node) == ["first answer"])
        .unwrap();
    let kept = after
        .iter()
        .find(|node| node.id == first_answer.id)
        .expect("a Full read still shows the ended answer before the full compaction");
    assert_eq!(kept.parent, Some(prompts[0]));
    assert_eq!(kept.status, DisplayStatus::Complete);
    let kept = tail_nodes
        .iter()
        .find(|node| node.id == third_answer)
        .expect("the answer the directed summary kept on the chain keeps its lifecycle");
    assert_eq!(kept.status, DisplayStatus::Complete);
    let third = tail_nodes
        .iter()
        .find(|node| node.id == prompts[2])
        .unwrap();
    assert_eq!(
        third.blocks,
        after
            .iter()
            .find(|node| node.id == prompts[2])
            .unwrap()
            .blocks
    );
    agent.shutdown().await;
    drop(agent);

    let next = agent_for_reload(store.clone());
    next.resume_chat(chat).await.unwrap();
    for prompt in prompts {
        assert!(matches!(
            next.input_state(chat, prompt)
                .await
                .unwrap()
                .unwrap()
                .entry
                .state,
            InputState::Settled {
                outcome: SettledOutcome::Completed { .. }
            }
        ));
    }
    next.shutdown().await;
}

fn agent_for_reload(store: Arc<InMemoryTranscriptStore>) -> Agent {
    agent(store, StagedModel::new(Vec::new()))
}

/// A local Transcript larger than the full-read limit is read from its latest boundary; the
/// lifecycle and ledger records written before the boundary come along.
#[tokio::test]
async fn a_large_jsonl_tail_load_keeps_lifecycle_and_ledger_records() {
    let directory = tempfile::tempdir().unwrap();
    let folder = directory.path().join("folder");
    std::fs::create_dir_all(&folder).unwrap();
    let store = JsonlTranscriptStore::open(directory.path().join("root")).unwrap();
    let chat = Uuid::new_v4();
    let expected = store
        .register(chat, &ChatPlacement::Folder(folder.canonicalize().unwrap()))
        .await
        .unwrap();
    let input = InputEntry {
        input_id: Uuid::new_v4(),
        op: InputOp::Continue,
        digest: "digest".into(),
        display_parent: None,
        loop_id: Uuid::new_v4(),
        run_id: Uuid::new_v4(),
        timestamp: "2026-01-01T00:00:00.000Z".into(),
        state: InputState::Accepted,
        queued: None,
    };
    let response = ResponseEntry {
        response_id: Uuid::new_v4(),
        input_id: Some(input.input_id),
        attempt: 1,
        run_id: input.run_id,
        loop_id: input.loop_id,
        display_parent: Some(input.input_id),
        timestamp: "2026-01-01T00:00:00.000Z".into(),
        state: ResponseState::Ended {
            ending: ResponseEnding::Completed,
        },
        without_messages: false,
    };
    let big = Uuid::new_v4();
    let boundary = Uuid::new_v4();
    let entry = |uuid: Uuid, parent: Option<Uuid>, boundary: bool, text: String| -> Line {
        let mut value = serde_json::json!({
            "type": if boundary { "system" } else { "user" },
            "uuid": uuid,
            "parentUuid": parent,
            "chatId": chat,
            "timestamp": "2026-01-01T00:00:00.000Z",
            "version": "test",
            "message": Message {
                role: if boundary { Role::System } else { Role::User },
                content: vec![Content::Text { text }],
            },
            "humanFamilyOrigin": null,
        });
        if boundary {
            value["subtype"] = "compact_boundary".into();
            value["compactMetadata"] = serde_json::json!({"trigger": "manual", "preTokens": 1});
        }
        serde_json::from_value(value).unwrap()
    };
    let lines = vec![
        Line::Input(input.clone()),
        Line::Response(response.clone()),
        entry(big, None, false, "x".repeat(6 * 1024 * 1024)),
        entry(boundary, None, true, "boundary".into()),
        entry(Uuid::new_v4(), Some(boundary), false, "after".into()),
    ];
    store
        .append(
            chat,
            &AppendBatch {
                id: BatchId::new(),
                expected: Some(expected),
                lines,
                events_through: 0,
                display_leaf: None,
                durable: true,
            },
        )
        .await
        .unwrap();
    let tail = store
        .load(chat, LoadRange::FromLatestBoundary)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !tail
            .lines
            .iter()
            .any(|line| line.chain_entry().is_some_and(|entry| entry.uuid == big)),
        "the tail starts at the boundary"
    );
    assert!(
        tail.retained
            .iter()
            .any(|line| matches!(line, Line::Input(entry) if *entry == input))
    );
    assert!(
        tail.retained
            .iter()
            .any(|line| matches!(line, Line::Response(entry) if *entry == response))
    );
}

/// The projection a host keeps from a tail load, applied line by line as the Chat goes on,
/// matches the projection of the whole history across a second directed compaction.
#[tokio::test]
async fn a_tail_initialized_projection_aliases_a_later_copy_of_the_same_answer() {
    let fixture = fixture();
    let store = Arc::new(InMemoryTranscriptStore::new());
    let first = agent(
        store.clone(),
        StagedModel::new(vec![
            Step::Reply(text("first answer")),
            Step::Reply(text("second answer")),
            Step::Reply(text("third answer")),
            Step::Reply(text("summary of the first turn")),
        ]),
    );
    let chat = first.create_chat(fixture.folder.clone()).await.unwrap();
    let prompts = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
    for (prompt, text) in prompts
        .iter()
        .zip(["first prompt", "second prompt", "third prompt"])
    {
        submit(&first, chat, *prompt, None, text).await;
        first.settled_input(chat, *prompt).await.unwrap();
    }
    summarize_up_to(&first, chat, "second prompt").await;
    first.sync().await.unwrap();
    first.shutdown().await;
    drop(first);

    let tail = store
        .load(chat, LoadRange::FromLatestBoundary)
        .await
        .unwrap()
        .unwrap();
    let mut projector = DisplayProjector::new();
    for line in tail.retained.iter().chain(&tail.lines) {
        projector.apply(line);
    }
    let applied = store.lines(chat).len();

    let next = agent(
        store.clone(),
        StagedModel::new(vec![Step::Reply(text("summary of the second turn"))]),
    );
    next.resume_chat(chat).await.unwrap();
    summarize_up_to(&next, chat, "third prompt").await;
    next.sync().await.unwrap();
    let lines = store.lines(chat);
    for line in &lines[applied..] {
        projector.apply(line);
    }

    let full = project(&lines);
    let third_answer = full
        .iter()
        .find(|node| texts(node) == ["third answer"])
        .expect("the third answer is projected once");
    // A tail projection meets a copy before its source, so only the time it first saw a node
    // may differ.
    let content = |node: &DisplayNode| {
        (
            node.id,
            node.parent,
            node.kind.clone(),
            node.blocks.clone(),
            node.status.clone(),
        )
    };
    for id in [third_answer.id, prompts[2]] {
        assert_eq!(
            projector.node(id).map(content),
            full.iter().find(|node| node.id == id).map(content)
        );
    }
    assert_eq!(
        projector
            .nodes()
            .filter(|node| texts(node) == ["third answer"])
            .count(),
        1
    );
    next.shutdown().await;
}

async fn summarize_up_to(agent: &Agent, chat: ChatId, prompt: &str) {
    let point = agent
        .snapshot(chat)
        .await
        .unwrap()
        .rewind_points()
        .into_iter()
        .find(|point| point.prompt == prompt)
        .unwrap();
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
}

#[tokio::test]
async fn replaying_an_input_after_rewind_removed_its_loop_returns_its_receipt() {
    let fixture = fixture();
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = StagedModel::new(vec![
        Step::Reply(text("first answer")),
        Step::Reply(text("second answer")),
    ]);
    let agent = agent(store.clone(), model.clone());
    let chat = agent.create_chat(fixture.folder.clone()).await.unwrap();
    let opening = Uuid::new_v4();
    submit(&agent, chat, opening, None, "open a loop").await;
    agent.settled_input(chat, opening).await.unwrap();
    let loop_id = agent
        .input_state(chat, opening)
        .await
        .unwrap()
        .unwrap()
        .entry
        .loop_id;
    let continued = Uuid::new_v4();
    submit(&agent, chat, continued, Some(loop_id), "continue the loop").await;
    agent.settled_input(chat, continued).await.unwrap();
    let original = agent.input_state(chat, continued).await.unwrap().unwrap();

    agent
        .rewind(chat, opening, RewindMode::Conversation)
        .await
        .unwrap();
    assert!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .loops
            .iter()
            .all(|goal| goal.id != loop_id),
        "the rewind removed the Loop"
    );
    let receipt = submit_input(
        &agent,
        chat,
        continued,
        Some(loop_id),
        "continue the loop".into(),
        Vec::new(),
        Vec::new(),
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        (receipt.loop_id, receipt.run_id),
        (original.entry.loop_id, original.entry.run_id)
    );
    assert!(matches!(
        submit_input(
            &agent,
            chat,
            continued,
            Some(loop_id),
            "another payload".into(),
            Vec::new(),
            Vec::new(),
            None
        )
        .await,
        Err(AgentError::Invalid(_))
    ));
    assert_eq!(model.requests(), 2);
    agent.shutdown().await;
}

#[tokio::test]
async fn replaying_an_input_after_its_attachment_was_deleted_returns_its_receipt() {
    let fixture = fixture();
    let attachment = fixture.folder.join("notes.txt");
    std::fs::write(&attachment, "the notes").unwrap();
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = StagedModel::new(vec![Step::Reply(text("read"))]);
    let agent = agent(store.clone(), model.clone());
    let chat = agent.create_chat(fixture.folder.clone()).await.unwrap();
    let input = Uuid::new_v4();
    let submit_attached = || {
        submit_input(
            &agent,
            chat,
            input,
            None,
            "summarize @notes.txt".into(),
            Vec::new(),
            vec![attachment.clone()],
            None,
        )
    };
    let original = submit_attached().await.unwrap();
    agent.settled_input(chat, input).await.unwrap();
    std::fs::remove_file(&attachment).unwrap();
    let replayed = submit_attached().await.unwrap();
    assert_eq!(
        (replayed.loop_id, replayed.run_id),
        (original.loop_id, original.run_id)
    );
    assert_eq!(model.requests(), 1);
    agent.shutdown().await;
}

/// A queued input whose Loop expires before it starts settles as discarded, once, and stays
/// settled across a reload and a replay.
#[tokio::test(start_paused = true)]
async fn an_input_queued_on_an_expiring_loop_settles_discarded_loop_ended() {
    let fixture = fixture();
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = StagedModel::new(vec![
        Step::Reply(text("watching")),
        Step::Hang { delta: None },
    ]);
    let first = agent(store.clone(), model.clone());
    let chat = first.create_chat(fixture.folder.clone()).await.unwrap();
    let watcher = Uuid::new_v4();
    submit(&first, chat, watcher, None, "watch until the deadline").await;
    first.settled_input(chat, watcher).await.unwrap();
    let loop_id = first
        .input_state(chat, watcher)
        .await
        .unwrap()
        .unwrap()
        .entry
        .loop_id;
    let lifetime = std::time::Duration::from_secs(60);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    first
        .configure_trigger(
            chat,
            loop_id,
            Some(Trigger {
                id: Uuid::new_v4(),
                prompt: "check".into(),
                prompt_source: TriggerPromptSource::Explicit,
                schedule: None,
                armed_at_ms: None,
                last_fired_at_ms: None,
                expires_at_ms: Some(now + lifetime.as_millis() as u64),
                accepts_events: true,
                armed: true,
                subscriptions: Vec::new(),
            }),
        )
        .await
        .unwrap();

    let holder = Uuid::new_v4();
    submit(&first, chat, holder, None, "hold the execution slot").await;
    model.reached.notified().await;
    let queued = Uuid::new_v4();
    submit(&first, chat, queued, Some(loop_id), "too late").await;
    let waiting = {
        let agent = first.clone();
        tokio::spawn(async move { agent.settled_input(chat, queued).await })
    };
    tokio::time::advance(lifetime).await;
    assert_eq!(
        waiting.await.unwrap().unwrap(),
        SettledOutcome::Discarded {
            reason: DiscardReason::LoopEnded
        }
    );
    let snapshot = first.snapshot(chat).await.unwrap();
    assert!(snapshot.queued_inputs().is_empty());
    assert_eq!(
        snapshot
            .loops
            .iter()
            .find(|goal| goal.id == loop_id)
            .unwrap()
            .state,
        LoopState::Expired
    );
    first.interrupt(chat).await.unwrap();
    first.settled_input(chat, holder).await.unwrap();
    first.sync().await.unwrap();
    let settled = inputs(&store.lines(chat), queued)
        .into_iter()
        .filter(|entry| matches!(entry.state, InputState::Settled { .. }))
        .collect::<Vec<_>>();
    assert_eq!(settled.len(), 1);
    first.shutdown().await;
    drop(first);

    let next = agent(store.clone(), StagedModel::new(Vec::new()));
    next.resume_chat(chat).await.unwrap();
    assert_eq!(
        next.input_state(chat, queued)
            .await
            .unwrap()
            .unwrap()
            .entry
            .state,
        InputState::Settled {
            outcome: SettledOutcome::Discarded {
                reason: DiscardReason::LoopEnded
            }
        }
    );
    submit_input(
        &next,
        chat,
        queued,
        Some(loop_id),
        "too late".into(),
        Vec::new(),
        Vec::new(),
        None,
    )
    .await
    .unwrap();
    assert!(
        next.snapshot(chat)
            .await
            .unwrap()
            .queued_inputs()
            .is_empty()
    );
    assert_eq!(model.requests(), 2);
    next.shutdown().await;
}
