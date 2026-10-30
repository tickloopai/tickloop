use std::{
    io::{self, Write},
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use serde_json::json;
use tickloop_code_access::{
    AccessError, AccessErrorCode, AgentAccess, AgentCommand, CommandEnvelope, CommandReceipt,
    CommandResult, Cursor, DisplayReceipt, DisplayUpdate, ExecutionProfile, ExecutionRef,
    LaunchSpec, LiveChatSnapshot, Observation, ObservationChange, ObservationRequest,
    ObservationUpdate, ObservationValue, PersistenceState, ReceiptState, ResetReason,
};
use tickloop_code_client::managed::ManagedClient;
use tickloop_code_print::{BoundedOutput, MAX_OUTPUT_BYTES, OutputFormat, parse, run_with_client};
use tickloop_harness::protocol::{
    ChatSnapshot, Content, Event, EventKind, LoopSnapshot, LoopState, Message, OperationRecord,
    OperationState, QuestionRequest, Receipt, Role, ToolResult, Usage,
};
use tokio::sync::mpsc;

#[derive(Clone, Copy)]
enum Behavior {
    Success,
    Reset,
    UnknownSubmit,
    Wait,
    Question,
    Failed,
    CleanupFailure,
    UnknownOperation,
    DeniedTool,
    ExpiredEvents,
    Disconnected,
    UnknownSettle,
    OversizedFinal,
    DaemonLost,
    HangingCancel,
}

struct Fake {
    execution: ExecutionRef,
    receipt: Receipt,
    snapshot: Mutex<LiveChatSnapshot>,
    calls: Mutex<Vec<&'static str>>,
    sender: Mutex<Option<mpsc::Sender<ObservationUpdate>>>,
    behavior: Behavior,
}

impl Fake {
    fn new(behavior: Behavior) -> Arc<Self> {
        let execution = ExecutionRef {
            daemon_epoch: tickloop_harness::ids::generate(),
            agent_id: tickloop_harness::ids::generate(),
            generation: 1,
        };
        let receipt = Receipt {
            chat_id: execution.agent_id.into(),
            loop_id: tickloop_harness::ids::generate().into(),
            run_id: tickloop_harness::ids::generate().into(),
        };
        let conversation: ChatSnapshot = serde_json::from_value(
            json!({"id": receipt.chat_id, "folder": "/tmp", "messages": [], "chat_loop": LoopSnapshot::new(receipt.loop_id)}),
        )
        .unwrap();
        Arc::new(Self {
            execution,
            receipt,
            snapshot: Mutex::new(LiveChatSnapshot::from_snapshot(
                execution,
                conversation,
                vec![],
            )),
            calls: Mutex::new(Vec::new()),
            sender: Mutex::new(None),
            behavior,
        })
    }

    fn event(&self, sequence: u64, kind: EventKind) -> Event {
        Event {
            sequence,
            chat_id: self.receipt.chat_id,
            loop_id: self.receipt.loop_id,
            run_id: self.receipt.run_id,
            kind,
        }
    }

    fn snapshot(&self) -> LiveChatSnapshot {
        self.snapshot.lock().unwrap().clone()
    }

    fn send(&self, sequence: u64, change: ObservationChange) {
        self.sender
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .try_send(ObservationUpdate {
                cursor: Cursor {
                    daemon_epoch: self.execution.daemon_epoch,
                    execution: Some(self.execution),
                    sequence,
                },
                change,
            })
            .unwrap();
    }

    fn client(self: &Arc<Self>) -> ManagedClient {
        ManagedClient::new(
            self.clone(),
            self.execution.daemon_epoch,
            tickloop_harness::ids::generate(),
        )
    }

    fn called(&self, call: &str) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|name| **name == call)
            .count()
    }
}

#[async_trait]
impl AgentAccess for Fake {
    async fn observe(&self, query: ObservationRequest) -> Result<Observation, AccessError> {
        self.calls.lock().unwrap().push("observe");
        let cursor = Cursor {
            daemon_epoch: self.execution.daemon_epoch,
            execution: Some(self.execution),
            sequence: 0,
        };
        let initial = ObservationValue::Agent(Box::new(self.snapshot()));
        if matches!(query, ObservationRequest::Agent { after: None, .. })
            && self.sender.lock().unwrap().is_none()
        {
            let (sender, receiver) = mpsc::channel(32);
            *self.sender.lock().unwrap() = Some(sender);
            Ok(Observation::new(
                initial,
                cursor,
                None,
                Some(receiver),
                None,
            ))
        } else {
            Ok(Observation::single(initial, cursor, None))
        }
    }

    async fn execute(&self, envelope: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
        let result = match envelope.command {
            AgentCommand::CreateManaged {
                launch, initial, ..
            } => {
                assert_eq!(launch.profile, ExecutionProfile::OneShot);
                assert!(initial.is_none());
                self.calls.lock().unwrap().push("create");
                CommandResult::Created {
                    execution: self.execution,
                    snapshot: Box::new(self.snapshot()),
                    submitted: None,
                    shell: None,
                }
            }
            AgentCommand::ManagedSubmit { .. } => {
                assert!(
                    self.sender.lock().unwrap().is_some(),
                    "observe must precede submit"
                );
                self.calls.lock().unwrap().push("submit");
                if matches!(self.behavior, Behavior::UnknownSubmit) {
                    return Err(AccessError::new(
                        AccessErrorCode::OutcomeUnknown,
                        "submission outcome unknown",
                    ));
                }
                if matches!(self.behavior, Behavior::Disconnected) {
                    self.sender.lock().unwrap().take();
                }
                if !matches!(
                    self.behavior,
                    Behavior::Wait
                        | Behavior::Disconnected
                        | Behavior::DaemonLost
                        | Behavior::HangingCancel
                ) {
                    let wrong = Event {
                        run_id: tickloop_harness::ids::generate().into(),
                        ..self.event(
                            1,
                            EventKind::Completed {
                                usage: Usage::default(),
                            },
                        )
                    };
                    self.send(1, ObservationChange::Event(wrong));
                    let content = if matches!(self.behavior, Behavior::DeniedTool) {
                        EventKind::ToolCompleted {
                            sources: Vec::new(),
                            call_id: "denied".into(),
                            result: ToolResult {
                                content: "permission denied".into(),
                                is_error: true,
                                output_ref: None,
                            },
                        }
                    } else {
                        EventKind::Text("intermediate".into())
                    };
                    let text = self.event(2, content);
                    self.send(2, ObservationChange::Event(text.clone()));
                    self.send(2, ObservationChange::Event(text.clone()));
                    let terminal = match self.behavior {
                        Behavior::Question => EventKind::QuestionAsked {
                            request: QuestionRequest {
                                id: tickloop_harness::ids::generate(),
                                chat_id: self.receipt.chat_id,
                                loop_id: self.receipt.loop_id,
                                run_id: self.receipt.run_id,
                                call_id: "question".into(),
                                questions: vec![],
                            },
                        },
                        Behavior::Failed => EventKind::Failed("model failed".into()),
                        _ => EventKind::Completed {
                            usage: Usage::default(),
                        },
                    };
                    let terminal = self.event(3, terminal);
                    {
                        let mut snapshot = self.snapshot.lock().unwrap();
                        snapshot.events = vec![text, terminal.clone()];
                        if matches!(self.behavior, Behavior::UnknownOperation) {
                            snapshot.operations.push(OperationRecord {
                                input_message_id: None,
                                parent_call_id: None,
                                parent_call_index: None,
                                task_binding: None,
                                id: tickloop_harness::ids::generate(),
                                loop_id: self.receipt.loop_id,
                                run_id: self.receipt.run_id,
                                call_id: "unknown".into(),
                                name: "Bash".into(),
                                input: json!({}),
                                file_change: None,
                                state: OperationState::Unknown,
                                result: None,
                            });
                        }
                        if matches!(self.behavior, Behavior::ExpiredEvents) {
                            snapshot.events.clear();
                            snapshot.conversation.chat_loop = LoopSnapshot {
                                id: self.receipt.loop_id,
                                receiving: Default::default(),
                                subscriptions: Vec::new(),
                                state: LoopState::Idle,
                                run_id: Some(self.receipt.run_id),
                                run_started_at: None,
                                state_since: None,
                                event_sources: vec![],
                                runs_started: None,
                                compactions: None,
                            };
                        }
                        // Providers such as Gemini stream one Text block per chunk; the
                        // final answer is their concatenation, as the deltas streamed it.
                        snapshot.conversation.messages.push(Message {
                            role: Role::Assistant,
                            content: if matches!(self.behavior, Behavior::OversizedFinal) {
                                vec![Content::Text {
                                    text: "x".repeat(MAX_OUTPUT_BYTES + 1),
                                }]
                            } else {
                                vec![
                                    Content::Text {
                                        text: "final ".into(),
                                    },
                                    Content::Text {
                                        text: "answer".into(),
                                    },
                                ]
                            },
                        });
                        snapshot
                            .conversation
                            .message_runs
                            .insert(0, self.receipt.run_id);
                    }
                    if matches!(self.behavior, Behavior::Reset) {
                        self.send(
                            3,
                            ObservationChange::Reset {
                                reason: tickloop_code_access::ResetReason::Lagged,
                                snapshot: Box::new(self.snapshot()),
                            },
                        );
                    } else if matches!(self.behavior, Behavior::ExpiredEvents) {
                        self.send(
                            3,
                            ObservationChange::Resync {
                                reason: ResetReason::CursorExpired,
                            },
                        );
                    } else {
                        self.send(3, ObservationChange::Event(terminal));
                    }
                }
                CommandResult::Submitted(self.receipt.clone())
            }
            AgentCommand::ManagedHeartbeat { .. } => {
                if matches!(self.behavior, Behavior::DaemonLost) {
                    return Err(AccessError::new(
                        AccessErrorCode::OutcomeUnknown,
                        "daemon disconnected",
                    ));
                }
                CommandResult::Done
            }
            AgentCommand::ManagedSettle { cancel, .. } => {
                self.calls
                    .lock()
                    .unwrap()
                    .push(if cancel { "cancel" } else { "settle" });
                if cancel && matches!(self.behavior, Behavior::HangingCancel) {
                    std::future::pending::<()>().await;
                }
                if !cancel && matches!(self.behavior, Behavior::UnknownSettle) {
                    return Err(AccessError::new(
                        AccessErrorCode::OutcomeUnknown,
                        "settlement unknown",
                    ));
                }
                CommandResult::ManagedSettled {
                    snapshot: Box::new(self.snapshot()),
                }
            }
            AgentCommand::ManagedRelease { .. } => {
                self.calls.lock().unwrap().push("release");
                if matches!(self.behavior, Behavior::CleanupFailure) {
                    return Err(AccessError::unavailable("release failed"));
                }
                CommandResult::Done
            }
            _ => panic!("unexpected mutation"),
        };
        Ok(CommandReceipt {
            daemon_epoch: self.execution.daemon_epoch,
            command_id: envelope.command_id,
            target: Some(self.execution),
            state: ReceiptState::Completed,
            result: Some(result),
            error: None,
            persistence: PersistenceState::NotApplicable,
        })
    }

    async fn show_view(&self, _: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
        panic!("print must not register a View")
    }
}

/// A closed channel never delivers a signal.
fn no_signals() -> mpsc::Receiver<i32> {
    mpsc::channel(1).1
}

fn options(format: OutputFormat) -> tickloop_code_print::Options {
    let mut options = parse(&["-p".into(), "hello".into()]).unwrap();
    options.format = format;
    options.timeout = Duration::from_secs(1);
    options
}

fn launch() -> LaunchSpec {
    let mut launch = LaunchSpec::embedded("/tmp".into());
    launch.profile = ExecutionProfile::OneShot;
    launch
}

#[tokio::test]
async fn text_uses_authoritative_final_reply_not_stream_deltas() {
    let fake = Fake::new(Behavior::Success);
    let mut output = Vec::new();
    let result = run_with_client(
        &options(OutputFormat::Text),
        launch(),
        "hello".into(),
        &fake.client(),
        &mut output,
        no_signals(),
    )
    .await;
    assert_eq!(result.exit_code, 0, "{result:?}");
    assert_eq!(output, b"final answer\n");
    assert_eq!(fake.called("submit"), 1);
    assert_eq!(fake.called("settle"), 1);
    assert_eq!(fake.called("release"), 1);
}

#[tokio::test]
async fn json_is_one_versioned_result_with_ids() {
    let fake = Fake::new(Behavior::Success);
    let mut output = Vec::new();
    let result = run_with_client(
        &options(OutputFormat::Json),
        launch(),
        "hello".into(),
        &fake.client(),
        &mut output,
        no_signals(),
    )
    .await;
    assert_eq!(result.exit_code, 0);
    let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["status"], "success");
    assert_eq!(value["run_id"], fake.receipt.run_id.uuid().to_string());
    assert_eq!(value["text"], "final answer");
}

#[tokio::test]
async fn stream_replays_deduplicate_and_reset_snapshot_is_reconciled() {
    let fake = Fake::new(Behavior::Reset);
    let mut output = Vec::new();
    let result = run_with_client(
        &options(OutputFormat::StreamJson),
        launch(),
        "hello".into(),
        &fake.client(),
        &mut output,
        no_signals(),
    )
    .await;
    assert_eq!(result.exit_code, 0);
    let records: Vec<serde_json::Value> = String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        records
            .iter()
            .filter(|record| record["type"] == "text.delta")
            .count(),
        1
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| record["type"] == "run.completed")
            .count(),
        1
    );
    assert_eq!(records.last().unwrap()["type"], "result");
    let delta = records
        .iter()
        .find(|record| record["type"] == "text.delta")
        .unwrap();
    assert_eq!(delta["data"], json!({"text": "intermediate"}));
    let completed = records
        .iter()
        .find(|record| record["type"] == "run.completed")
        .unwrap();
    assert_eq!(
        completed["data"],
        json!({"usage": {
            "input_tokens": null, "output_tokens": null,
            "cache_read_input_tokens": null, "cache_write_input_tokens": null,
        }})
    );
    assert_eq!(
        records
            .iter()
            .find(|record| record["type"] == "stream.reset")
            .unwrap()["data"]["reason"],
        "lagged"
    );
    assert_eq!(
        records
            .iter()
            .find(|record| record["type"] == "response.snapshot")
            .unwrap()["data"]["text"],
        "final answer"
    );
    for (index, record) in records.iter().enumerate() {
        assert_eq!(record["schema_version"], 1);
        assert_eq!(record["sequence"], index + 1);
    }
}

#[tokio::test]
async fn unknown_submit_is_never_replayed_and_is_cancelled_and_released() {
    let fake = Fake::new(Behavior::UnknownSubmit);
    let result = run_with_client(
        &options(OutputFormat::Text),
        launch(),
        "hello".into(),
        &fake.client(),
        &mut Vec::new(),
        no_signals(),
    )
    .await;
    assert_eq!(result.exit_code, 1);
    assert_eq!(fake.called("submit"), 1);
    assert_eq!(fake.called("cancel"), 1);
    assert_eq!(fake.called("release"), 1);
}

#[tokio::test]
async fn actual_question_is_human_required_not_a_success() {
    let fake = Fake::new(Behavior::Question);
    let result = run_with_client(
        &options(OutputFormat::Text),
        launch(),
        "hello".into(),
        &fake.client(),
        &mut Vec::new(),
        no_signals(),
    )
    .await;
    assert_eq!(result.exit_code, 3);
    assert_eq!(fake.called("cancel"), 1);
    assert_eq!(fake.called("release"), 1);
}

#[tokio::test]
async fn timeout_cancels_and_releases_its_execution() {
    let fake = Fake::new(Behavior::Wait);
    let mut options = options(OutputFormat::Text);
    options.timeout = Duration::from_millis(10);
    let result = run_with_client(
        &options,
        launch(),
        "hello".into(),
        &fake.client(),
        &mut Vec::new(),
        no_signals(),
    )
    .await;
    assert_eq!(result.exit_code, 124, "{result:?}");
    assert_eq!(fake.called("cancel"), 1);
    assert_eq!(fake.called("release"), 1);
}

#[tokio::test]
async fn signals_cancel_and_release() {
    for code in [130, 143] {
        let fake = Fake::new(Behavior::Wait);
        let (sender, shutdown) = mpsc::channel(1);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            sender.send(code).await.unwrap();
        });
        let result = run_with_client(
            &options(OutputFormat::Text),
            launch(),
            "hello".into(),
            &fake.client(),
            &mut Vec::new(),
            shutdown,
        )
        .await;
        assert_eq!(result.exit_code, code, "{result:?}");
        assert_eq!(fake.called("cancel"), 1);
        assert_eq!(fake.called("release"), 1);
    }
}

#[tokio::test]
async fn denied_tool_can_finish_successfully_and_expired_events_reconcile() {
    for behavior in [Behavior::DeniedTool, Behavior::ExpiredEvents] {
        let fake = Fake::new(behavior);
        let mut output = Vec::new();
        let result = run_with_client(
            &options(OutputFormat::Text),
            launch(),
            "hello".into(),
            &fake.client(),
            &mut output,
            no_signals(),
        )
        .await;
        assert_eq!(result.exit_code, 0, "{result:?}");
        assert_eq!(output, b"final answer\n");
        assert_eq!(fake.called("settle"), 1);
        assert_eq!(fake.called("release"), 1);
    }
}

#[tokio::test]
async fn tool_stream_uses_explicit_public_fields() {
    let fake = Fake::new(Behavior::DeniedTool);
    let mut output = Vec::new();
    let result = run_with_client(
        &options(OutputFormat::StreamJson),
        launch(),
        "hello".into(),
        &fake.client(),
        &mut output,
        no_signals(),
    )
    .await;
    assert_eq!(result.exit_code, 0);
    let records: Vec<serde_json::Value> = String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        records
            .iter()
            .find(|record| record["type"] == "tool.completed")
            .unwrap()["data"],
        json!({"call_id": "denied", "content": "permission denied", "is_error": true})
    );
}

#[tokio::test]
async fn unknown_operations_or_settlement_or_disconnection_never_succeed() {
    for behavior in [
        Behavior::UnknownOperation,
        Behavior::UnknownSettle,
        Behavior::Disconnected,
    ] {
        let fake = Fake::new(behavior);
        let result = run_with_client(
            &options(OutputFormat::Text),
            launch(),
            "hello".into(),
            &fake.client(),
            &mut Vec::new(),
            no_signals(),
        )
        .await;
        assert_eq!(result.exit_code, 1, "{result:?}");
        assert_eq!(fake.called("submit"), 1);
        assert_eq!(fake.called("release"), 1);
        assert_eq!(fake.called("cancel"), 1);
    }
}

struct BrokenPipe;
impl Write for BrokenPipe {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::from(io::ErrorKind::BrokenPipe))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn broken_pipe_stops_owned_execution() {
    let fake = Fake::new(Behavior::Success);
    let result = run_with_client(
        &options(OutputFormat::StreamJson),
        launch(),
        "hello".into(),
        &fake.client(),
        &mut BrokenPipe,
        no_signals(),
    )
    .await;
    assert_eq!(result.exit_code, 1);
    assert_eq!(fake.called("cancel"), 1);
    assert_eq!(fake.called("release"), 1);
    assert_eq!(fake.called("submit"), 0);
}

#[tokio::test]
async fn full_unread_open_pipe_fails_without_blocking_and_cleans_up() {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    let mut descriptors = [-1; 2];
    assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
    // SAFETY: pipe returned two fresh descriptors; each is converted to one owner.
    let _reader = unsafe { OwnedFd::from_raw_fd(descriptors[0]) };
    let writer = unsafe { OwnedFd::from_raw_fd(descriptors[1]) };
    let inspect = writer.try_clone().unwrap();
    let flags = unsafe { libc::fcntl(inspect.as_raw_fd(), libc::F_GETFL) };
    let mut output = BoundedOutput::new(writer);
    let started = std::time::Instant::now();
    // Keep the read end open, but never consume bytes. This is not BrokenPipe.
    let mut filled = 0;
    loop {
        match output.write(&[0; 8192]) {
            Ok(written) => {
                assert!(written > 0);
                filled += written;
                assert!(filled <= MAX_OUTPUT_BYTES);
            }
            Err(error) => {
                assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
                break;
            }
        }
    }
    let fake = Fake::new(Behavior::Wait);
    let result = run_with_client(
        &options(OutputFormat::StreamJson),
        launch(),
        "hello".into(),
        &fake.client(),
        &mut output,
        no_signals(),
    )
    .await;
    assert_eq!(result.exit_code, 1, "{result:?}");
    assert_eq!(fake.called("submit"), 0);
    assert_eq!(fake.called("cancel"), 1);
    assert_eq!(fake.called("release"), 1);
    assert!(started.elapsed() < Duration::from_secs(2));
    // The open file description is shared with the caller: never mark it nonblocking, even
    // while the writer is alive (a concurrent invocation or SIGKILL would inherit the change).
    assert_eq!(
        unsafe { libc::fcntl(inspect.as_raw_fd(), libc::F_GETFL) } & libc::O_NONBLOCK,
        0
    );
    assert_eq!(flags & libc::O_NONBLOCK, 0);
    drop(output);
}

#[test]
fn large_output_succeeds_with_a_concurrently_draining_pipe() {
    use std::{
        fs::File,
        io::Read,
        os::fd::{FromRawFd, OwnedFd},
    };
    let mut descriptors = [-1; 2];
    assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
    // SAFETY: each newly returned descriptor gets exactly one owner.
    let reader = unsafe { OwnedFd::from_raw_fd(descriptors[0]) };
    let writer = unsafe { OwnedFd::from_raw_fd(descriptors[1]) };
    let bytes = vec![b'x'; 256 * 1024];
    let draining = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        File::from(reader).read_to_end(&mut bytes).unwrap();
        bytes
    });
    let mut output = BoundedOutput::new(writer);
    output.write_all(&bytes).unwrap();
    drop(output);
    assert_eq!(draining.join().unwrap(), bytes);
}

#[tokio::test]
async fn final_output_limit_fails_after_releasing_execution() {
    let fake = Fake::new(Behavior::OversizedFinal);
    let mut output = Vec::new();
    let result = run_with_client(
        &options(OutputFormat::Text),
        launch(),
        "hello".into(),
        &fake.client(),
        &mut output,
        no_signals(),
    )
    .await;
    assert_eq!(result.exit_code, 1);
    assert!(result.error.unwrap().contains("16 MiB"));
    assert!(output.is_empty());
    assert_eq!(fake.called("release"), 1);
}

#[tokio::test]
async fn run_failure_or_cleanup_failure_never_reports_success() {
    for behavior in [Behavior::Failed, Behavior::CleanupFailure] {
        let fake = Fake::new(behavior);
        let mut output = Vec::new();
        let result = run_with_client(
            &options(OutputFormat::Json),
            launch(),
            "hello".into(),
            &fake.client(),
            &mut output,
            no_signals(),
        )
        .await;
        assert_eq!(result.exit_code, 1);
        assert_eq!(fake.called("release"), 1);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&output).unwrap()["status"],
            "failed"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn lost_daemon_ends_the_run_once_the_lease_expires() {
    let fake = Fake::new(Behavior::DaemonLost);
    let mut options = options(OutputFormat::Json);
    options.timeout = Duration::from_secs(86_400);
    let started = tokio::time::Instant::now();
    let mut output = Vec::new();
    let result = run_with_client(
        &options,
        launch(),
        "hello".into(),
        &fake.client(),
        &mut output,
        no_signals(),
    )
    .await;
    assert_eq!(result.exit_code, 1, "{result:?}");
    assert_eq!(
        result.error.as_deref(),
        Some("Agent daemon stopped; managed execution lost")
    );
    assert!(started.elapsed() < Duration::from_secs(120));
    // The daemon already owns the execution's cleanup.
    assert_eq!(fake.called("cancel"), 0);
    assert_eq!(fake.called("release"), 0);
    let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(value["status"], "failed");
    assert_eq!(value["run_id"], fake.receipt.run_id.uuid().to_string());
}

#[tokio::test]
async fn another_signal_skips_cleanup_and_exits_with_its_code() {
    for code in [130, 143] {
        let fake = Fake::new(Behavior::HangingCancel);
        let (sender, shutdown) = mpsc::channel(1);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            sender.send(130).await.unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            sender.send(code).await.unwrap();
        });
        let started = std::time::Instant::now();
        let mut options = options(OutputFormat::Text);
        options.timeout = Duration::from_secs(600);
        let result = run_with_client(
            &options,
            launch(),
            "hello".into(),
            &fake.client(),
            &mut Vec::new(),
            shutdown,
        )
        .await;
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(result.exit_code, code, "{result:?}");
        assert_eq!(result.status, "cancelled");
        assert!(result.error.unwrap().contains("cleanup skipped"));
        assert_eq!(fake.called("cancel"), 1);
        assert_eq!(fake.called("release"), 0);
    }
}
