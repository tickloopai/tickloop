//! The queue a Chat's input ledger carries from one host to the next: every queued input is
//! accepted with its content, so a load under durable wait hosting continues the answers a lost
//! host left, ahead of the inputs it queues again, and fails an input whose host was lost three
//! times in a row. Process wait hosting also preserves unconsumed family messages and their
//! delivery state, while other inputs the Agent queued for itself are lost.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tickloop_agent_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_agent_protocol::{AgentHostOptions, ChatId, Content, PromptPolicy, Usage};
use tickloop_agent_runtime::transcript::{StorePosition, TranscriptStore};
use tickloop_agent_runtime::{
    Agent, Delivery, HarnessOptions, HarnessSeams, ParentSelection, ToolPolicy, TurnOp,
    TurnRequest, UserMessage, WaitHosting,
};
use tickloop_agent_test_support::harness::ChosenModels;
use tickloop_agent_test_support::transcripts::InMemoryTranscriptStore;
use tickloop_agent_test_support::{call, config, text};
use tickloop_agent_transcript::{
    InputState, InterruptedBy, Line, ResponseEnding, ResponseState, SettledOutcome,
};
use tokio::sync::{Notify, mpsc::UnboundedSender};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub(super) const WAIT: std::time::Duration = std::time::Duration::from_secs(10);

pub(super) enum Step {
    Reply(Vec<Content>),
    /// Holds the request until it is cancelled.
    Hang,
    /// Fails the request as a misconfigured model does, which no retry mends.
    Fail,
}

/// Follows its steps, then answers "answer"; records every request.
pub(super) struct Script {
    steps: Mutex<VecDeque<Step>>,
    requests: Mutex<Vec<ModelRequest>>,
    holding: Notify,
}

impl Script {
    pub(super) fn new(steps: Vec<Step>) -> Arc<Self> {
        Arc::new(Self {
            steps: Mutex::new(steps.into()),
            requests: Mutex::new(Vec::new()),
            holding: Notify::new(),
        })
    }

    pub(super) async fn until_holding(&self) {
        tokio::time::timeout(WAIT, self.holding.notified())
            .await
            .expect("the model holds a request");
    }

    pub(super) fn conversation_requests(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|request| {
                format!(
                    "{:?}",
                    request
                        .messages
                        .iter()
                        .filter(|message| message.role != tickloop_agent_protocol::Role::System)
                        .collect::<Vec<_>>()
                )
            })
            .collect()
    }

    pub(super) fn requests(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|request| format!("{:?}", request.messages))
            .collect()
    }
}

#[async_trait]
impl ModelClient for Script {
    async fn generate_title(
        &self,
        request: ModelRequest,
        _: UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        Ok(tickloop_agent_test_support::title_output(request))
    }

    fn identity(&self) -> tickloop_agent_protocol::ModelIdentity {
        tickloop_agent_protocol::ModelIdentity {
            provider: tickloop_agent_protocol::ModelProvider::Api,
            profile: "fixture".into(),
            protocol: tickloop_agent_protocol::Protocol::Messages,
            requested_model: "fixture".into(),
        }
    }

    async fn complete(
        &self,
        request: ModelRequest,
        _: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        self.requests.lock().unwrap().push(request);
        let step = self.steps.lock().unwrap().pop_front();
        let content = match step {
            Some(Step::Reply(content)) => content,
            Some(Step::Hang) => {
                self.holding.notify_one();
                cancel.cancelled().await;
                return Err(ModelError::Cancelled);
            }
            Some(Step::Fail) => {
                return Err(ModelError::Configuration("the model is unavailable".into()));
            }
            None => text("answer"),
        };
        Ok(ModelOutput {
            private_source: None,
            content,
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

pub(super) fn harness(
    store: Arc<dyn TranscriptStore>,
    model: Arc<Script>,
    waits: WaitHosting,
) -> Agent {
    Agent::with_seams(
        HarnessSeams {
            transcripts: Some(store),
            environment: None,
            models: ChosenModels::new(model),
        },
        HarnessOptions {
            host: AgentHostOptions {
                prompt_policy: PromptPolicy::DenyOnPrompt,
                ..AgentHostOptions::product_defaults()
            },
            tools: match waits {
                WaitHosting::Durable => ToolPolicy {
                    questions: true,
                    ..ToolPolicy::web_fetch_only()
                },
                // Nothing of a Chat without a Folder outlives a process that keeps its waits:
                // such a harness only loads what a durable one wrote.
                WaitHosting::Process => ToolPolicy::web_fetch_only(),
            },
            waits,
            ..HarnessOptions::local(config())
        },
    )
    .unwrap()
}

pub(super) fn prompt(text: &str, after: ParentSelection) -> TurnRequest {
    TurnRequest {
        input_id: Uuid::new_v4(),
        op: TurnOp::Continue {
            after,
            message: UserMessage {
                text: text.into(),
                ..Default::default()
            },
        },
        delivery: Delivery::Queue,
        host_context: None,
    }
}

/// What the store holds once the holder is lost without shutting down: a new store with its
/// Lines.
pub(super) async fn lost_holder(
    agent: &Agent,
    store: &InMemoryTranscriptStore,
    chat: ChatId,
) -> Arc<InMemoryTranscriptStore> {
    agent.sync().await.unwrap();
    let next = Arc::new(InMemoryTranscriptStore::new());
    next.seed(chat, store.lines(chat), StorePosition(1));
    next
}

pub(super) fn input_state(lines: &[Line], input: Uuid) -> Option<InputState> {
    lines.iter().rev().find_map(|line| match line {
        Line::Input(entry) if entry.input_id == input => Some(entry.state.clone()),
        _ => None,
    })
}

pub(super) async fn settled(agent: &Agent, chat: ChatId, input: Uuid) -> SettledOutcome {
    tokio::time::timeout(WAIT, agent.settled_input(chat, input))
        .await
        .expect("the input settles")
        .unwrap()
}

#[tokio::test]
async fn a_lost_answer_continues_before_the_inputs_queued_behind_it() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = Script::new(vec![Step::Hang]);
    let first = harness(store.clone(), model.clone(), WaitHosting::Durable);
    let chat = first.create_chat_without_folder().await.unwrap();
    let lost = prompt("first prompt", ParentSelection::CurrentLeaf);
    first.advance(chat, lost.clone()).await.unwrap();
    model.until_holding().await;
    let queued = prompt("second prompt", ParentSelection::CurrentLeaf);
    first.advance(chat, queued.clone()).await.unwrap();
    let store = lost_holder(&first, &store, chat).await;
    assert_eq!(
        input_state(&store.lines(chat), queued.input_id),
        Some(InputState::Accepted)
    );

    let model = Script::new(vec![
        Step::Reply(text("first answer")),
        Step::Reply(text("second answer")),
    ]);
    let next = harness(store.clone(), model.clone(), WaitHosting::Durable);
    next.resume_chat(chat).await.unwrap();
    assert!(matches!(
        settled(&next, chat, queued.input_id).await,
        SettledOutcome::Completed { .. }
    ));
    assert!(matches!(
        settled(&next, chat, lost.input_id).await,
        SettledOutcome::Completed { .. }
    ));
    let requests = model.conversation_requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].contains("first prompt") && !requests[0].contains("second prompt"));
    assert!(requests[1].contains("first answer") && requests[1].contains("second prompt"));
    // Waking a Chat this instance holds loads nothing again.
    next.resume_chat(chat).await.unwrap();
    assert_eq!(model.requests().len(), 2);
    next.shutdown().await;
    first.shutdown().await;
}

#[tokio::test]
async fn the_third_lost_host_in_a_row_fails_the_input_and_a_handover_does_not_count() {
    for with_handover in [false, true] {
        let mut store = Arc::new(InMemoryTranscriptStore::new());
        let model = Script::new(vec![Step::Hang]);
        let first = harness(store.clone(), model.clone(), WaitHosting::Durable);
        let chat = first.create_chat_without_folder().await.unwrap();
        let input = prompt("a long task", ParentSelection::CurrentLeaf);
        first.advance(chat, input.clone()).await.unwrap();
        model.until_holding().await;
        store = lost_holder(&first, &store, chat).await;
        let mut holders = vec![first];
        // Each load continues it in another attempt, which loses its host again.
        for _ in 0..2 {
            let model = Script::new(vec![Step::Hang]);
            let holder = harness(store.clone(), model.clone(), WaitHosting::Durable);
            holder.resume_chat(chat).await.unwrap();
            model.until_holding().await;
            store = lost_holder(&holder, &store, chat).await;
            holders.push(holder);
        }
        if with_handover {
            // One of the losses was a voluntary handover's, which neither counts nor breaks the
            // run of host losses.
            let lines = store
                .lines(chat)
                .into_iter()
                .map(|line| match line {
                    Line::Response(mut entry) if entry.attempt == 2 && entry.state.is_ended() => {
                        entry.state = ResponseState::Ended {
                            ending: ResponseEnding::Interrupted {
                                by: InterruptedBy::Handover,
                            },
                        };
                        Line::Response(entry)
                    }
                    line => line,
                })
                .collect();
            store = Arc::new(InMemoryTranscriptStore::new());
            store.seed(chat, lines, StorePosition(1));
        }
        let model = Script::new(vec![Step::Reply(text("done at last"))]);
        let last = harness(store.clone(), model.clone(), WaitHosting::Durable);
        last.resume_chat(chat).await.unwrap();
        let outcome = settled(&last, chat, input.input_id).await;
        if with_handover {
            assert!(
                matches!(outcome, SettledOutcome::Completed { .. }),
                "{outcome:?}"
            );
            assert_eq!(model.requests().len(), 1);
        } else {
            assert_eq!(
                outcome,
                SettledOutcome::Failed {
                    error: "lost its host 3 times".into()
                }
            );
            assert!(model.requests().is_empty());
        }
        last.shutdown().await;
        for holder in holders {
            holder.shutdown().await;
        }
    }
}

#[tokio::test]
async fn a_repeated_input_still_finds_its_ledger_record_after_folding() {
    const TURNS: usize = 140;
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = Script::new(Vec::new());
    let agent = harness(store.clone(), model.clone(), WaitHosting::Durable);
    let chat = agent.create_chat_without_folder().await.unwrap();
    let first = prompt("turn 0", ParentSelection::CurrentLeaf);
    agent.advance(chat, first.clone()).await.unwrap();
    settled(&agent, chat, first.input_id).await;
    for turn in 1..TURNS {
        let request = prompt(&format!("turn {turn}"), ParentSelection::CurrentLeaf);
        agent.advance(chat, request.clone()).await.unwrap();
        settled(&agent, chat, request.input_id).await;
    }
    agent.sync().await.unwrap();
    agent.shutdown().await;
    let lines = store.lines(chat);
    assert!(
        lines
            .iter()
            .any(|line| matches!(line, Line::RetainedSnapshot(_))),
        "a threshold snapshot folded the retained Lines"
    );
    let stripped = Arc::new(InMemoryTranscriptStore::new());
    stripped.seed(
        chat,
        lines
            .iter()
            .filter(|line| !matches!(line, Line::RetainedSnapshot(_)))
            .cloned()
            .collect(),
        StorePosition(1),
    );

    // The same Chat loaded from its snapshot and from every retained Line.
    let mut loaded = Vec::new();
    for store in [store.clone(), stripped] {
        let model = Script::new(vec![Step::Reply(text("next answer"))]);
        let next = harness(store.clone(), model.clone(), WaitHosting::Durable);
        let repeated = next.advance(chat, first.clone()).await.unwrap();
        assert!(matches!(
            repeated.outcome().await.unwrap(),
            tickloop_agent_runtime::TurnOutcome::Completed { .. }
        ));
        assert!(
            model.requests().is_empty(),
            "the repeated input runs nothing"
        );
        let snapshot = next.snapshot(chat).await.unwrap();
        let following = prompt("one more", ParentSelection::CurrentLeaf);
        next.advance(chat, following.clone()).await.unwrap();
        settled(&next, chat, following.input_id).await;
        loaded.push((
            serde_json::to_string(&snapshot.messages).unwrap(),
            serde_json::to_string(&snapshot.chat_loop).unwrap(),
            model.requests(),
        ));
        next.shutdown().await;
    }
    assert_eq!(loaded[0].0, loaded[1].0, "the conversation");
    assert_eq!(loaded[0].1, loaded[1].1, "the Loop");
    assert_eq!(loaded[0].2.len(), 1);
    let normalize = |request: &str| {
        request
            .split(|character: char| !character.is_ascii_alphanumeric() && character != '-')
            .filter(|word| Uuid::parse_str(word).is_err() && word.parse::<u64>().is_err())
            .collect::<Vec<_>>()
            .join(" ")
    };
    assert_eq!(
        normalize(&loaded[0].2[0]),
        normalize(&loaded[1].2[0]),
        "the next model request"
    );
}

#[tokio::test]
async fn inputs_queued_behind_a_suspended_run_are_queued_again_in_their_order() {
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = Script::new(vec![Step::Reply(call(
        "question",
        tickloop_agent_tools::ASK_USER_QUESTION_TOOL_NAME,
        tickloop_agent_test_support::single_question("Which report?"),
    ))]);
    let asking = harness(store.clone(), model, WaitHosting::Durable);
    let chat = asking.create_chat_without_folder().await.unwrap();
    let turn = asking
        .advance(
            chat,
            prompt("prepare the report", ParentSelection::CurrentLeaf),
        )
        .await
        .unwrap();
    let request = loop {
        if let tickloop_agent_runtime::TurnOutcome::Waiting(
            tickloop_agent_runtime::TurnWait::Question(request),
        ) = turn.outcome().await.unwrap()
        {
            break request;
        }
    };
    let follow_ups = ["also keep it brief", "and add a chart"].map(|text| TurnRequest {
        op: TurnOp::Continue {
            after: ParentSelection::CurrentLeaf,
            message: UserMessage {
                text: text.into(),
                ..Default::default()
            },
        },
        ..prompt(text, ParentSelection::CurrentLeaf)
    });
    for follow_up in &follow_ups {
        asking.advance(chat, follow_up.clone()).await.unwrap();
    }
    asking.shutdown().await;
    asking.sync().await.unwrap();

    let model = Script::new(vec![Step::Reply(text("brief report with a chart"))]);
    let answering = harness(store.clone(), model.clone(), WaitHosting::Durable);
    answering
        .advance(
            chat,
            TurnRequest {
                input_id: Uuid::new_v4(),
                op: TurnOp::Answer {
                    request: request.id,
                    reply: tickloop_agent_runtime::QuestionReply::Answered(vec![
                        tickloop_agent_protocol::QuestionAnswer::other("Weekly"),
                    ]),
                },
                delivery: Delivery::Queue,
                host_context: None,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        settled(&answering, chat, turn.input_id()).await,
        SettledOutcome::Completed { .. }
    ));
    for follow_up in &follow_ups {
        assert_eq!(
            settled(&answering, chat, follow_up.input_id).await,
            SettledOutcome::Merged {
                into: turn.receipt().run_id
            }
        );
    }
    let requests = model.requests();
    assert_eq!(requests.len(), 1);
    let (weekly, brief, chart) = (
        requests[0].find("Weekly").unwrap(),
        requests[0].find("also keep it brief").unwrap(),
        requests[0].find("and add a chart").unwrap(),
    );
    assert!(weekly < brief && brief < chart, "{}", requests[0]);
    answering.shutdown().await;
}
