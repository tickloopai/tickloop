//! Busy inputs through public `AgentAccess`: which model request consumes each input and
//! which queue boundaries keep accepted work in separate runs.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use serde_json::json;
use tickloop_agent_daemon::EmbeddedAgentAccess;
use tickloop_agent_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_agent_protocol::{
    AfterTurnAction, AgentAccess, AgentCommand, AgentDetailQuery, CommandEnvelope, CommandReceipt,
    CommandResult, ConfigurationChange, EventKind, ExecutionRef, LaunchSpec, LiveChatSnapshot,
    LoopState, ModelIdentity, ObservationRequest, ObservationValue, Receipt, ReceiptState,
    UserInput,
};
use tickloop_agent_runtime::Agent;
use tickloop_agent_test_support::{ScriptedModel, call, config, text};
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
    hold_next: AtomicBool,
    next_entered: Notify,
    next_release: Notify,
}

#[async_trait]
impl ModelClient for GatedFirstRequest {
    async fn generate_title(
        &self,
        request: tickloop_agent_model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
        Ok(tickloop_agent_test_support::title_output(request))
    }

    fn identity(&self) -> ModelIdentity {
        self.model.identity()
    }

    async fn complete(
        &self,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        let gate = match self.calls.fetch_add(1, Ordering::SeqCst) {
            0 => Some((&self.entered, &self.release)),
            1 if self.hold_next.load(Ordering::SeqCst) => {
                Some((&self.next_entered, &self.next_release))
            }
            _ => None,
        };
        if let Some((entered, release)) = gate {
            entered.notify_one();
            tokio::select! {
                _ = cancel.cancelled() => return Err(ModelError::Cancelled),
                _ = release.notified() => {}
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
    async fn start(directory: &Path, replies: Vec<Vec<tickloop_agent_protocol::Content>>) -> Self {
        Self::start_with_archive(directory, replies, None).await
    }

    async fn start_with_archive(
        directory: &Path,
        replies: Vec<Vec<tickloop_agent_protocol::Content>>,
        archive: Option<PathBuf>,
    ) -> Self {
        let source = directory.canonicalize().unwrap().join("source.txt");
        std::fs::write(&source, SOURCE_CONTENT).unwrap();
        let model = Arc::new(ScriptedModel::new(replies));
        let gate = Arc::new(GatedFirstRequest {
            model: model.clone(),
            entered: Notify::new(),
            release: Notify::new(),
            calls: AtomicUsize::new(0),
            hold_next: AtomicBool::new(false),
            next_entered: Notify::new(),
            next_release: Notify::new(),
        });
        let runtime = match &archive {
            Some(archive) => Agent::open_embedded(archive.clone(), config(), gate.clone()).unwrap(),
            None => Agent::new_embedded(config(), gate.clone()),
        };
        runtime.install_own_tickhost();
        let access = EmbeddedAgentAccess::new(runtime.clone());
        let mut launch = LaunchSpec::embedded(directory.to_path_buf());
        launch.config_root = archive;
        let created = access
            .execute(envelope(
                &access,
                AgentCommand::Create {
                    launch,
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
        assert_eq!(receipt.daemon_epoch, self.access.daemon_epoch());
        assert_eq!(receipt.target, Some(self.execution));
        let Some(CommandResult::Submitted(accepted)) = receipt.result.clone() else {
            panic!("a busy Submit is accepted with its own run: {receipt:?}");
        };
        (receipt, accepted)
    }

    fn submit_command(&self, text: &str) -> CommandEnvelope {
        let paths = if text == INPUTS[0] {
            vec![self.source.clone()]
        } else {
            Vec::new()
        };
        envelope(
            &self.access,
            AgentCommand::Submit {
                execution: self.execution,
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
        self.settle().await
    }

    async fn settle(&self) -> LiveChatSnapshot {
        tokio::time::timeout(TEST_DEADLINE, async {
            loop {
                let snapshot = self.snapshot().await;
                if snapshot.pending_inputs.is_empty()
                    && !matches!(
                        snapshot.conversation.chat_loop.state,
                        LoopState::Running | LoopState::Queued
                    )
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
                requests.iter().position(|request| {
                    request.messages.iter().any(|message| {
                        message.role == tickloop_agent_protocol::Role::User
                            && message.content.iter().any(|content| {
                                matches!(content, tickloop_agent_protocol::Content::Text { text } if text == input)
                            })
                    })
                })
            })
            .collect()
    }

    fn sends(&self) -> usize {
        self.model.requests.lock().unwrap().len()
    }
}

fn envelope(access: &EmbeddedAgentAccess, command: AgentCommand) -> CommandEnvelope {
    CommandEnvelope {
        daemon_epoch: access.daemon_epoch(),
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
    let commands = INPUTS.map(|input| busy.submit_command(input));
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

async fn assert_text_only_batch(count: usize) {
    let directory = tempfile::tempdir().unwrap();
    let busy = Busy::start(
        directory.path(),
        vec![
            text("first reply"),
            text("one reply for the queued batch"),
            text("unexpected separate reply"),
            text("unexpected separate reply"),
        ],
    )
    .await;
    let commands = INPUTS[..count]
        .iter()
        .map(|input| busy.submit_command(input))
        .collect::<Vec<_>>();
    let mut accepted = Vec::new();
    for command in &commands {
        accepted.push(busy.submit(command).await.1);
    }
    assert_eq!(
        accepted
            .iter()
            .map(|receipt| receipt.run_id)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        count,
    );
    assert!(
        accepted
            .iter()
            .all(|receipt| receipt.loop_id == busy.active.loop_id)
    );
    let replayed = busy.submit(&commands[0]).await.1;
    assert_eq!(replayed.run_id, accepted[0].run_id);
    assert_eq!(busy.snapshot().await.pending_inputs.len(), count);

    let settled = busy.release_and_settle().await;
    assert_eq!(
        busy.sends(),
        2,
        "all {count} queued inputs share the next model request"
    );
    assert_eq!(&busy.first_requests()[..count], &vec![Some(1); count]);
    let second = format!("{:?}", busy.model.requests.lock().unwrap()[1].messages);
    let positions = INPUTS[..count]
        .iter()
        .map(|input| second.find(input).unwrap())
        .collect::<Vec<_>>();
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(
        INPUTS[..count]
            .iter()
            .all(|input| second.matches(input).count() == 1)
    );
    assert!(
        second.contains(SOURCE_CONTENT),
        "the first queued input keeps its attachment"
    );
    for (input, receipt) in INPUTS[..count].iter().zip(&accepted) {
        let index = settled.conversation.messages.iter().position(|message| {
            message.content.iter().any(|content| matches!(content, tickloop_agent_protocol::Content::Text { text } if text == input))
        }).unwrap();
        assert_eq!(
            settled.conversation.message_runs.get(&index),
            Some(&receipt.run_id)
        );
        assert_eq!(
            settled
                .events
                .iter()
                .find(|event| event.run_id == receipt.run_id)
                .map(|event| event.loop_id),
            Some(receipt.loop_id)
        );
        assert!(
            settled
                .conversation
                .prompt_execution_modes
                .contains_key(&index)
        );
    }
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
        accepted[1..]
            .iter()
            .map(|receipt| (receipt.run_id, accepted[0].run_id))
            .collect::<Vec<_>>()
    );
    let started = runs_with(&settled, |kind| matches!(kind, EventKind::Started));
    let completed = runs_with(&settled, |kind| matches!(kind, EventKind::Completed { .. }));
    assert_eq!(started, vec![busy.active.run_id, accepted[0].run_id]);
    assert_eq!(completed, started);
    assert_eq!(busy.submit(&commands[0]).await.1.run_id, accepted[0].run_id);
    assert_eq!(
        busy.sends(),
        2,
        "an accepted CommandId is never resent after the batch completes"
    );
    assert!(busy.snapshot().await.pending_inputs.is_empty());
    busy.runtime.shutdown().await;
}

#[tokio::test]
async fn two_busy_inputs_after_a_text_only_reply_share_the_next_run() {
    assert_text_only_batch(2).await;
}

#[tokio::test]
async fn three_busy_inputs_after_a_text_only_reply_share_the_next_run() {
    assert_text_only_batch(3).await;
}

#[tokio::test]
async fn a_started_input_batch_is_recoverable_before_its_model_reply() {
    let directory = tempfile::tempdir().unwrap();
    let archive = directory.path().join("archive");
    let busy = Busy::start_with_archive(
        directory.path(),
        vec![text("first reply"), text("queued batch reply")],
        Some(archive.clone()),
    )
    .await;
    busy.gate.hold_next.store(true, Ordering::SeqCst);
    let mut accepted = Vec::new();
    for input in INPUTS {
        accepted.push(busy.submit(&busy.submit_command(input)).await.1);
    }
    let mut events = busy.runtime.subscribe();
    busy.gate.release.notify_one();
    tokio::time::timeout(TEST_DEADLINE, async {
        loop {
            let event = events.recv().await.unwrap();
            if event.run_id == accepted[0].run_id && matches!(event.kind, EventKind::Started) {
                break;
            }
        }
    })
    .await
    .expect("the queued batch starts");
    tokio::time::timeout(TEST_DEADLINE, busy.gate.next_entered.notified())
        .await
        .expect("the next model request is held before replying");
    let started = busy.snapshot().await;
    assert!(started.pending_inputs.is_empty());
    assert_eq!(busy.gate.calls.load(Ordering::SeqCst), 2);
    assert_eq!(busy.sends(), 1, "the queued reply has not completed");
    assert!(!started.events.iter().any(|event| {
        event.run_id == accepted[0].run_id && matches!(event.kind, EventKind::Completed { .. })
    }));
    for (input, receipt) in INPUTS.iter().zip(&accepted) {
        let index = started.conversation.messages.iter().position(|message| {
            message.content.iter().any(|content| matches!(content, tickloop_agent_protocol::Content::Text { text } if text == input))
        }).expect("each accepted input is already in the started batch");
        assert_eq!(
            started.conversation.message_runs.get(&index),
            Some(&receipt.run_id)
        );
    }
    busy.runtime.flush_transcripts().await;
    let recovery_model = Arc::new(ScriptedModel::new(Vec::new()));
    let recovered = Agent::open_embedded(archive, config(), recovery_model.clone()).unwrap();
    let history = recovered.resume_chat(busy.active.chat_id).await.unwrap();
    for (input, receipt) in INPUTS.iter().zip(&accepted) {
        let indexes = history.messages.iter().enumerate().filter_map(|(index, message)| {
            message.content.iter().any(|content| matches!(content, tickloop_agent_protocol::Content::Text { text } if text == input)).then_some(index)
        }).collect::<Vec<_>>();
        assert_eq!(
            indexes.len(),
            1,
            "each accepted input is recoverable exactly once"
        );
        assert_eq!(history.message_runs.get(&indexes[0]), Some(&receipt.run_id));
        assert!(history.prompt_execution_modes.contains_key(&indexes[0]));
    }
    assert!(format!("{:?}", history.message_contexts).contains(SOURCE_CONTENT));
    assert!(
        recovery_model.requests.lock().unwrap().is_empty(),
        "opening saved history does not execute the batch"
    );
    recovered.shutdown().await;
    busy.gate.next_release.notify_one();
    busy.settle().await;
    assert_eq!(busy.sends(), 2);
    busy.runtime.shutdown().await;
}

async fn assert_direct_skill_boundary(tool_boundary: bool) {
    let directory = tempfile::tempdir().unwrap();
    let skill_directory = directory.path().join(".tickloop/skills/queued-boundary");
    std::fs::create_dir_all(&skill_directory).unwrap();
    std::fs::write(
        skill_directory.join("SKILL.md"),
        "---\nname: queued-boundary\ndescription: Owned queued Skill fixture\n---\nRead the supplied context and answer.\n",
    )
    .unwrap();
    let replies = if tool_boundary {
        vec![
            call("read", "Read", json!({"file_path": "source.txt"})),
            text("answered the active tool turn"),
        ]
    } else {
        vec![
            text("first reply"),
            text("before the Skill"),
            text("Skill reply"),
            text("after the Skill"),
        ]
    };
    let busy = Busy::start(directory.path(), replies).await;
    let first = busy.submit(&busy.submit_command(INPUTS[0])).await.1;
    let skill_command = envelope(
        &busy.access,
        AgentCommand::Submit {
            execution: busy.execution,
            input: UserInput::Skill {
                name: "queued-boundary".into(),
                arguments: "typed Skill input".into(),
            },
            origin: None,
        },
    );
    let skill = busy.submit(&skill_command).await.1;
    assert_eq!(busy.submit(&skill_command).await.1.run_id, skill.run_id);
    let second = busy.submit(&busy.submit_command(INPUTS[1])).await.1;
    let last = busy.submit(&busy.submit_command(INPUTS[2])).await.1;
    assert_eq!(busy.snapshot().await.pending_inputs.len(), 4);
    let settled = busy.release_and_settle().await;
    let skill_request = busy
        .model
        .requests
        .lock()
        .unwrap()
        .iter()
        .position(|request| {
            request.messages.iter().any(|message| {
                message.role == tickloop_agent_protocol::Role::User
                    && message.content.iter().any(|content| {
                        matches!(content, tickloop_agent_protocol::Content::Text { text } if text == "/queued-boundary typed Skill input")
                    })
            })
        });
    let merged = settled
        .events
        .iter()
        .filter_map(|event| match event.kind {
            EventKind::InputMerged { into_run_id } => Some((event.run_id, into_run_id)),
            _ => None,
        })
        .collect::<Vec<_>>();
    if tool_boundary {
        assert_eq!(busy.sends(), 2);
        assert_eq!(busy.first_requests(), vec![Some(1); 3]);
        assert_eq!(skill_request, Some(1));
        assert_eq!(
            merged,
            [&first, &skill, &second, &last]
                .iter()
                .map(|receipt| (receipt.run_id, busy.active.run_id))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            runs_with(&settled, |kind| matches!(kind, EventKind::Started)),
            vec![busy.active.run_id]
        );
    } else {
        assert_eq!(busy.sends(), 4);
        assert_eq!(busy.first_requests(), vec![Some(1), Some(3), Some(3)]);
        assert_eq!(skill_request, Some(2));
        assert_eq!(merged, vec![(last.run_id, second.run_id)]);
        assert_eq!(
            runs_with(&settled, |kind| matches!(kind, EventKind::Started)),
            vec![
                busy.active.run_id,
                first.run_id,
                skill.run_id,
                second.run_id
            ]
        );
    }
    busy.runtime.shutdown().await;
}

#[tokio::test]
async fn a_direct_skill_keeps_text_only_input_batches_separate() {
    assert_direct_skill_boundary(false).await;
}

#[tokio::test]
async fn a_direct_skill_still_joins_the_active_tool_turn() {
    assert_direct_skill_boundary(true).await;
}

#[tokio::test]
async fn a_command_barrier_splits_text_only_input_batches() {
    let directory = tempfile::tempdir().unwrap();
    let busy = Busy::start(
        directory.path(),
        vec![text("first"), text("before command"), text("after command")],
    )
    .await;
    let first = busy.submit(&busy.submit_command(INPUTS[0])).await.1;
    let second = busy.submit(&busy.submit_command(INPUTS[1])).await.1;
    let barrier = busy
        .access
        .execute(envelope(
            &busy.access,
            AgentCommand::EnqueueAfterTurn {
                execution: busy.execution,
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
    let last = busy.submit(&busy.submit_command(INPUTS[2])).await.1;
    let settled = busy.release_and_settle().await;
    assert_eq!(busy.first_requests(), vec![Some(1), Some(1), Some(2)]);
    assert_eq!(busy.sends(), 3);
    assert!(settled.pending_commands.is_empty());
    assert!(
        settled.queue_failures.is_empty(),
        "{:?}",
        settled.queue_failures
    );
    assert!(settled.thinking_enabled);
    assert_eq!(
        runs_with(&settled, |kind| matches!(
            kind,
            EventKind::InputMerged { .. }
        )),
        vec![second.run_id]
    );
    assert_eq!(
        runs_with(&settled, |kind| matches!(kind, EventKind::Started)),
        vec![busy.active.run_id, first.run_id, last.run_id]
    );
    busy.runtime.shutdown().await;
}

#[tokio::test]
async fn a_foreground_shell_barrier_splits_text_only_input_batches() {
    let directory = tempfile::tempdir().unwrap();
    let busy = Busy::start(
        directory.path(),
        vec![text("first"), text("before shell"), text("after shell")],
    )
    .await;
    let first = busy.submit(&busy.submit_command(INPUTS[0])).await.1;
    let shell = busy
        .access
        .execute(envelope(
            &busy.access,
            AgentCommand::Submit {
                execution: busy.execution,
                input: UserInput::Shell {
                    command: "printf 'NEXT_BATCH_SHELL_MARKER'".into(),
                    background: false,
                },
                origin: None,
            },
        ))
        .await
        .unwrap();
    let Some(CommandResult::ShellSubmitted(shell)) = shell.result else {
        panic!("Shell was accepted")
    };
    let second = busy.submit(&busy.submit_command(INPUTS[1])).await.1;
    let last = busy.submit(&busy.submit_command(INPUTS[2])).await.1;
    let settled = busy.release_and_settle().await;
    assert_eq!(busy.first_requests(), vec![Some(1), Some(2), Some(2)]);
    assert_eq!(busy.sends(), 3);
    let record = settled
        .user_shells
        .iter()
        .find(|record| record.id == shell.command_id)
        .unwrap();
    assert_eq!(
        record.state,
        tickloop_agent_protocol::UserShellState::Completed
    );
    let third = {
        let requests = busy.model.requests.lock().unwrap();
        assert!(!format!("{:?}", requests[1].messages).contains("NEXT_BATCH_SHELL_MARKER"));
        format!("{:?}", requests[2].messages)
    };
    assert!(third.find("NEXT_BATCH_SHELL_MARKER").unwrap() < third.find(INPUTS[1]).unwrap());
    assert_eq!(
        runs_with(&settled, |kind| matches!(
            kind,
            EventKind::InputMerged { .. }
        )),
        vec![last.run_id]
    );
    assert_eq!(
        runs_with(&settled, |kind| matches!(kind, EventKind::Started)),
        vec![busy.active.run_id, first.run_id, second.run_id]
    );
    busy.runtime.shutdown().await;
}

#[tokio::test]
async fn a_command_barrier_keeps_an_input_out_of_the_merge() {
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
        let command = busy.submit_command(input);
        runs.push(busy.submit(&command).await.1);
    }
    let barrier = busy
        .access
        .execute(envelope(
            &busy.access,
            AgentCommand::EnqueueAfterTurn {
                execution: busy.execution,
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
    let command = busy.submit_command(INPUTS[2]);
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
}

/// Worker submissions go through `advance`: each is a ledger input whose prompt id is its input
/// id, settled with the run that answered it.
#[tokio::test]
async fn worker_submissions_are_ledger_inputs_settled_by_their_answers() {
    let directory = tempfile::tempdir().unwrap();
    let busy = Busy::start(
        directory.path(),
        vec![text("started"), text("answered with the attachment")],
    )
    .await;
    let (_, accepted) = busy.submit(&busy.submit_command(INPUTS[0])).await;
    busy.release_and_settle().await;
    assert!(
        format!("{:?}", busy.model.requests.lock().unwrap()[1].messages).contains(SOURCE_CONTENT)
    );
    let chat = busy.active.chat_id;
    let snapshot = busy.runtime.snapshot(chat).await.unwrap();
    let mut settled = Vec::new();
    for (index, id) in &snapshot.message_ids {
        if !snapshot.prompt_execution_modes.contains_key(index) {
            continue;
        }
        let state = busy.runtime.input_state(chat, *id).await.unwrap().unwrap();
        settled.push((state.entry.run_id, state.entry.state));
    }
    assert_eq!(
        settled.iter().map(|(run, _)| *run).collect::<Vec<_>>(),
        vec![busy.active.run_id, accepted.run_id]
    );
    assert!(settled.iter().all(|(_, state)| matches!(
        state,
        tickloop_agent_transcript::InputState::Settled {
            outcome: tickloop_agent_transcript::SettledOutcome::Completed { .. }
        }
    )));
}
