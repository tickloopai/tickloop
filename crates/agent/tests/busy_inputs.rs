//! Three inputs accepted while an Agent is busy, through the public `AgentAccess`: which model
//! request first carries each one, and whether the runtime merged it into the active run. These
//! pin the current execution semantics; they change nothing about them.
mod support;

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use serde_json::json;
use support::{ScriptedModel, call, config, text};
use tickloop_agent::{
    agent_access::{
        AfterTurnAction, AgentAccess, AgentCommand, AgentDetailQuery, CommandEnvelope,
        CommandReceipt, CommandResult, ConfigurationChange, EmbeddedAgentAccess, ExecutionRef,
        LaunchSpec, LiveChatSnapshot, ObservationRequest, ObservationValue, ReceiptState,
        UserInput,
    },
    model::{ModelClient, ModelDelta, ModelError, ModelIdentity, ModelOutput, ModelRequest},
    runtime::{Agent, EventKind, LoopState, Receipt},
};
use tokio::sync::{Notify, mpsc::UnboundedSender};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const TEST_DEADLINE: Duration = Duration::from_secs(5);
const INPUTS: [&str; 3] = ["busy input alpha", "busy input bravo", "busy input charlie"];
const SOURCE_CONTENT: &str = "SOURCE-ATTACHED-TO-ALPHA";

/// Holds the first model request until released, so inputs arrive while it is in flight.
struct GatedFirstRequest {
    model: Arc<ScriptedModel>,
    entered: Notify,
    release: Notify,
    calls: AtomicUsize,
}

#[async_trait]
impl ModelClient for GatedFirstRequest {
    fn identity(&self) -> ModelIdentity {
        self.model.identity()
    }

    async fn complete(
        &self,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.entered.notify_one();
            tokio::select! {
                _ = cancel.cancelled() => return Err(ModelError::Cancelled),
                _ = self.release.notified() => {}
            }
        }
        self.model.complete(request, events, cancel).await
    }
}

struct Busy {
    runtime: Agent,
    access: EmbeddedAgentAccess,
    model: Arc<ScriptedModel>,
    gate: Arc<GatedFirstRequest>,
    execution: ExecutionRef,
    active: Receipt,
    source: PathBuf,
}

impl Busy {
    /// An Agent whose first run is waiting on its first model request.
    async fn start(directory: &Path, replies: Vec<Vec<tickloop_agent::model::Content>>) -> Self {
        let source = directory.canonicalize().unwrap().join("source.txt");
        std::fs::write(&source, SOURCE_CONTENT).unwrap();
        let model = Arc::new(ScriptedModel::new(replies));
        let gate = Arc::new(GatedFirstRequest {
            model: model.clone(),
            entered: Notify::new(),
            release: Notify::new(),
            calls: AtomicUsize::new(0),
        });
        let runtime = Agent::new_embedded(config(), gate.clone());
        let access = EmbeddedAgentAccess::new(runtime.clone());
        let created = access
            .execute(envelope(
                &access,
                AgentCommand::Create {
                    launch: LaunchSpec::embedded(directory.to_path_buf()),
                    initial: Some(text_input("start the busy work", &[])),
                    origin: None,
                },
            ))
            .await
            .unwrap();
        let Some(CommandResult::Created {
            execution,
            submitted: Some(active),
            ..
        }) = created.result
        else {
            panic!("Create submits the initial input: {created:?}");
        };
        tokio::time::timeout(TEST_DEADLINE, gate.entered.notified())
            .await
            .expect("the first model request is in flight");
        Self {
            runtime,
            access,
            model,
            gate,
            execution,
            active,
            source,
        }
    }

    async fn submit(&self, command: &CommandEnvelope) -> (CommandReceipt, Receipt) {
        let receipt = self.access.execute(command.clone()).await.unwrap();
        assert!(
            matches!(receipt.state, ReceiptState::Completed),
            "{receipt:?}"
        );
        assert_eq!(receipt.command_id, command.command_id);
        assert_eq!(receipt.owner_epoch, self.access.owner_epoch());
        assert_eq!(receipt.target, Some(self.execution));
        let Some(CommandResult::Submitted(accepted)) = receipt.result.clone() else {
            panic!("a busy Submit is accepted with its own run: {receipt:?}");
        };
        (receipt, accepted)
    }

    fn submit_command(&self, text: &str, target_loop: Option<Uuid>) -> CommandEnvelope {
        let paths = if text == INPUTS[0] {
            vec![self.source.clone()]
        } else {
            Vec::new()
        };
        envelope(
            &self.access,
            AgentCommand::Submit {
                execution: self.execution,
                target_loop,
                input: text_input(text, &paths),
                origin: None,
            },
        )
    }

    async fn snapshot(&self) -> LiveChatSnapshot {
        match self
            .access
            .observe(ObservationRequest::Agent {
                execution: self.execution,
                detail: AgentDetailQuery::Snapshot,
                after: None,
            })
            .await
            .unwrap()
            .initial
        {
            ObservationValue::Agent(snapshot) => *snapshot,
            other => panic!("unexpected snapshot: {other:?}"),
        }
    }

    /// Releases the first request and waits until no Loop is still working.
    async fn release_and_settle(&self) -> LiveChatSnapshot {
        self.gate.release.notify_one();
        tokio::time::timeout(TEST_DEADLINE, async {
            loop {
                let snapshot = self.snapshot().await;
                if snapshot.pending_inputs.is_empty()
                    && snapshot
                        .conversation
                        .loops
                        .iter()
                        .all(|goal| !matches!(goal.state, LoopState::Running | LoopState::Queued))
                {
                    break snapshot;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the busy work settles")
    }

    /// For each input, the index of the first model request that carries it.
    fn first_requests(&self) -> Vec<Option<usize>> {
        let requests = self.model.requests.lock().unwrap();
        INPUTS
            .iter()
            .map(|input| {
                requests
                    .iter()
                    .position(|request| format!("{:?}", request.messages).contains(input))
            })
            .collect()
    }

    fn sends(&self) -> usize {
        self.model.requests.lock().unwrap().len()
    }
}

fn envelope(access: &EmbeddedAgentAccess, command: AgentCommand) -> CommandEnvelope {
    CommandEnvelope {
        owner_epoch: access.owner_epoch(),
        command_id: Uuid::new_v4(),
        command,
    }
}

fn text_input(text: &str, paths: &[PathBuf]) -> UserInput {
    UserInput::Text {
        text: text.into(),
        contexts: Vec::new(),
        paths: paths.to_vec(),
    }
}

/// The runs of the events `matches` accepts, in event order.
fn runs_with(snapshot: &LiveChatSnapshot, matches: impl Fn(&EventKind) -> bool) -> Vec<Uuid> {
    snapshot
        .events
        .iter()
        .filter(|event| matches(&event.kind))
        .map(|event| event.run_id)
        .collect()
}

#[tokio::test]
async fn busy_inputs_accepted_before_a_tool_boundary_join_the_active_run_in_one_request() {
    let directory = tempfile::tempdir().unwrap();
    let busy = Busy::start(
        directory.path(),
        vec![
            call("read", "Read", json!({"file_path": "source.txt"})),
            text("read and answered all three"),
        ],
    )
    .await;
    let commands = INPUTS.map(|input| busy.submit_command(input, Some(busy.active.loop_id)));
    let mut accepted = Vec::new();
    for command in &commands {
        accepted.push(busy.submit(command).await);
    }
    let runs = accepted
        .iter()
        .map(|(_, run)| run.run_id)
        .collect::<Vec<_>>();
    assert!(
        accepted
            .iter()
            .all(|(_, run)| run.loop_id == busy.active.loop_id && run.run_id != busy.active.run_id),
        "each accepted input continues the active Loop with its own run: {accepted:?}"
    );
    assert_eq!(
        runs.iter().collect::<std::collections::HashSet<_>>().len(),
        3
    );
    let pending = busy.snapshot().await.pending_inputs;
    assert_eq!(pending.len(), 3, "{pending:?}");

    let (replayed, replayed_run) = busy.submit(&commands[0]).await;
    assert_eq!(
        replayed_run.run_id, runs[0],
        "a replayed command is the same acceptance"
    );
    assert_eq!(replayed.command_id, commands[0].command_id);
    assert_eq!(busy.snapshot().await.pending_inputs.len(), 3);

    let settled = busy.release_and_settle().await;
    assert_eq!(
        busy.sends(),
        2,
        "one tool round, then one request for all three"
    );
    assert_eq!(busy.first_requests(), vec![Some(1); 3]);
    let second = format!("{:?}", busy.model.requests.lock().unwrap()[1].messages);
    let positions = INPUTS.map(|input| second.find(input).unwrap());
    assert!(
        positions.windows(2).all(|pair| pair[0] < pair[1])
            && INPUTS
                .iter()
                .all(|input| second.matches(input).count() == 1)
            && second.contains(SOURCE_CONTENT),
        "each input once, in acceptance order, with alpha's attachment: {second}"
    );
    let merged = settled
        .events
        .iter()
        .filter_map(|event| match event.kind {
            EventKind::InputMerged { into_run_id } => Some((event.run_id, into_run_id)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        merged,
        runs.iter()
            .map(|run| (*run, busy.active.run_id))
            .collect::<Vec<_>>(),
        "the three inputs merged into the active run, in order"
    );
    let started = runs_with(&settled, |kind| matches!(kind, EventKind::Started));
    let completed = runs_with(&settled, |kind| matches!(kind, EventKind::Completed { .. }));
    assert!(
        runs.iter()
            .all(|run| !started.contains(run) && !completed.contains(run))
            && completed == vec![busy.active.run_id],
        "merged inputs never start or complete runs of their own: started {started:?}, completed {completed:?}"
    );
    busy.runtime.shutdown().await;
}

#[tokio::test]
async fn busy_inputs_after_a_text_only_reply_are_answered_one_request_each() {
    let directory = tempfile::tempdir().unwrap();
    let busy = Busy::start(
        directory.path(),
        vec![
            text("first reply"),
            text("alpha reply"),
            text("bravo reply"),
            text("charlie reply"),
        ],
    )
    .await;
    let mut runs = Vec::new();
    for input in INPUTS {
        let command = busy.submit_command(input, Some(busy.active.loop_id));
        runs.push(busy.submit(&command).await.1.run_id);
    }
    let settled = busy.release_and_settle().await;
    assert_eq!(
        busy.sends(),
        4,
        "a text-only reply admits one pending input at a time"
    );
    assert_eq!(busy.first_requests(), vec![Some(1), Some(2), Some(3)]);
    assert!(
        !settled
            .events
            .iter()
            .any(|event| matches!(event.kind, EventKind::InputMerged { .. })),
        "nothing was merged"
    );
    let started = runs_with(&settled, |kind| matches!(kind, EventKind::Started));
    let completed = runs_with(&settled, |kind| matches!(kind, EventKind::Completed { .. }));
    assert!(
        runs.iter()
            .all(|run| started.contains(run) && completed.contains(run)),
        "each input ran as its own run: started {started:?}, completed {completed:?}"
    );
    busy.runtime.shutdown().await;
}

#[tokio::test]
async fn a_command_barrier_or_another_loop_keeps_an_input_out_of_the_merge() {
    let directory = tempfile::tempdir().unwrap();
    let busy = Busy::start(
        directory.path(),
        vec![
            call("read", "Read", json!({"file_path": "source.txt"})),
            text("answered alpha and bravo"),
            text("answered charlie"),
        ],
    )
    .await;
    let mut runs = Vec::new();
    for input in &INPUTS[..2] {
        let command = busy.submit_command(input, Some(busy.active.loop_id));
        runs.push(busy.submit(&command).await.1);
    }
    let barrier = busy
        .access
        .execute(envelope(
            &busy.access,
            AgentCommand::EnqueueAfterTurn {
                execution: busy.execution,
                target_loop: Some(busy.active.loop_id),
                action: AfterTurnAction::Configure {
                    change: ConfigurationChange::ThinkingEnabled { enabled: true },
                },
            },
        ))
        .await
        .unwrap();
    assert!(
        matches!(barrier.state, ReceiptState::Completed),
        "{barrier:?}"
    );
    let command = busy.submit_command(INPUTS[2], Some(busy.active.loop_id));
    runs.push(busy.submit(&command).await.1);
    let settled = busy.release_and_settle().await;
    let first = busy.first_requests();
    assert_eq!(first[..2], [Some(1), Some(1)]);
    assert!(
        first[2].is_some_and(|index| index > 1),
        "an input behind a queued command waits for that command: {first:?}"
    );
    let merged = runs_with(&settled, |kind| {
        matches!(kind, EventKind::InputMerged { .. })
    });
    assert_eq!(merged, vec![runs[0].run_id, runs[1].run_id]);
    busy.runtime.shutdown().await;

    let directory = tempfile::tempdir().unwrap();
    let busy = Busy::start(
        directory.path(),
        vec![
            call("read", "Read", json!({"file_path": "source.txt"})),
            text("answered alpha"),
            text("answered the other goal"),
        ],
    )
    .await;
    let same_loop = busy
        .submit(&busy.submit_command(INPUTS[0], Some(busy.active.loop_id)))
        .await
        .1;
    let other_loop = busy.submit(&busy.submit_command(INPUTS[2], None)).await.1;
    assert_ne!(other_loop.loop_id, busy.active.loop_id);
    let settled = busy.release_and_settle().await;
    let merged = runs_with(&settled, |kind| {
        matches!(kind, EventKind::InputMerged { .. })
    });
    assert_eq!(
        merged,
        vec![same_loop.run_id],
        "an input for another Loop is never merged into this run"
    );
    busy.runtime.shutdown().await;
}
