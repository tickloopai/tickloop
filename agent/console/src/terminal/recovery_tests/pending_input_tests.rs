use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use tickloop_agent_protocol::Content;

const QUEUE_FIXTURE: &str = "TICKLOOP_PENDING_INPUT_FIXTURE";
const QUEUE_ROUTE: &str = "TICKLOOP_PENDING_INPUT_ROUTE";
const QUEUE_STAGE: &str = "queue-stage";
const QUEUE_PROOF: &str = "queue-proof.json";
const OTHER_VIEW_SUBMIT: &str = "other-view-submit";
const QUEUE_DISCONNECTED: &str = "queue-disconnected";
const QUEUED_MESSAGE: &str = "Hello";
const INITIAL_MESSAGE: &str = "Queue initial input";

struct QueueModel {
    calls: AtomicUsize,
    requests: Mutex<Vec<ModelRequest>>,
}

struct QueueAccess {
    daemon: EmbeddedAgentAccess,
}

#[async_trait]
impl AgentAccess for QueueAccess {
    async fn observe(&self, query: ObservationRequest) -> Result<Observation, AccessError> {
        if std::path::Path::new(QUEUE_DISCONNECTED).exists()
            && matches!(
                query,
                ObservationRequest::Agent {
                    detail: AgentDetailQuery::Snapshot | AgentDetailQuery::Status,
                    ..
                }
            )
        {
            return Err(AccessError::unavailable("owned queue snapshot unavailable"));
        }
        self.daemon.observe(query).await
    }
    async fn execute(&self, command: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
        self.daemon.execute(command).await
    }
    async fn show_view(&self, update: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
        self.daemon.show_view(update).await
    }
}

#[async_trait]
impl ModelClient for QueueModel {
    async fn generate_title(
        &self,
        request: tickloop_agent_model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
        Ok(tickloop_agent_model::fixture_title(request))
    }

    fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            provider: ModelProvider::Api,
            profile: "queue-fixture".into(),
            protocol: Protocol::Messages,
            requested_model: "queue-fixture".into(),
        }
    }

    async fn complete(
        &self,
        request: ModelRequest,
        events: mpsc::UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        self.requests.lock().unwrap().push(request);
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        while !std::fs::read_to_string(QUEUE_STAGE)
            .is_ok_and(|stage| stage == format!("release-{call}"))
        {
            tokio::select! {
                _ = cancel.cancelled() => return Err(ModelError::Cancelled),
                _ = tokio::time::sleep(Duration::from_millis(5)) => {},
            }
        }
        let content = if call <= 2
            && matches!(
                std::env::var(QUEUE_ROUTE).as_deref(),
                Ok("tool" | "external")
            ) {
            tickloop_agent_test_support::call(
                &format!("queue-read-{call}"),
                "Read",
                serde_json::json!({"file_path":"queue-input.txt"}),
            )
        } else {
            events
                .send(ModelDelta::Text(format!("Queue reply {call}")))
                .unwrap();
            tickloop_agent_test_support::text(&format!("Queue reply {call}"))
        };
        Ok(ModelOutput {
            private_source: None,
            content,
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

#[test]
#[ignore = "owned PTY producer for pending input consumption"]
fn pending_input_pty_fixture() {
    if std::env::var(QUEUE_FIXTURE).as_deref() != Ok("1") {
        return;
    }
    std::thread::Builder::new()
        .name("pending-input-fixture".into())
        .stack_size(FIXTURE_STACK_BYTES)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            tokio::task::LocalSet::new().block_on(&runtime, async {
                let model = Arc::new(QueueModel {
                    calls: AtomicUsize::new(0),
                    requests: Mutex::new(Vec::new()),
                });
                let core = tickloop_agent_runtime::Agent::new_embedded(tickloop_agent_test_support::config(), model.clone());
                let access = Arc::new(QueueAccess {daemon:EmbeddedAgentAccess::new(core.clone())});
                let workdir = std::env::current_dir().unwrap();
                let mut launch = LaunchSpec::embedded(workdir.clone());
                launch.model = Some(tickloop_agent_test_support::config());
                launch.session_persistence = false;
                let agent = Agent::from_access_for_test(
                    access.clone(),
                    access.daemon.daemon_epoch(),
                    ViewRef {
                        session_id: Uuid::new_v4(),
                        view_id: Uuid::new_v4(),
                    },
                    launch.clone(),
                    None,
                );
                let other_view=Agent::from_access_for_test(access.clone(),access.daemon.daemon_epoch(),ViewRef {session_id:Uuid::new_v4(),view_id:Uuid::new_v4()},launch,None);
                let proof_core = core.clone();
                let proof_agent = agent.clone();
                tokio::task::spawn_local(async move {
                    let mut submitted_elsewhere=false;
                    loop {
                        if let Some(snapshot) = proof_core.list_chats().await.into_iter().next() {
                            if !submitted_elsewhere && std::path::Path::new(OTHER_VIEW_SUBMIT).exists() && !snapshot.loops.is_empty() {
                                other_view.display_chat(Some(snapshot.id)).await.unwrap();
                                for _ in 0..2 {
                                    other_view.submit(snapshot.id,Some(snapshot.loops[0].id),QUEUED_MESSAGE.into()).await.unwrap();
                                }
                                submitted_elsewhere=true;
                            }
                            let hello_messages = snapshot.messages.iter().flat_map(|message| &message.content)
                                .filter(|content| matches!(content, Content::Text { text } if text == QUEUED_MESSAGE)).count();
                            let request_hellos = model.requests.lock().unwrap().last().map(|request| request.messages.iter().flat_map(|message| &message.content)
                                .filter(|content| matches!(content, Content::Text { text } if text == QUEUED_MESSAGE)).count()).unwrap_or(0);
                            let proof = serde_json::json!({
                                "calls": model.calls.load(Ordering::SeqCst),
                                "pending": snapshot.queued_inputs().len(),
                                "hello_messages": hello_messages,
                                "request_hellos": request_hellos,
                                "pending_receipts": snapshot.pending_inputs.iter().map(|pending| serde_json::json!({
                                    "run":pending.receipt.run_id,"loop":pending.receipt.loop_id,
                                })).collect::<Vec<_>>(),
                                "started": snapshot.events.iter().filter(|event| matches!(event.kind,EventKind::Started)).map(|event|event.run_id).collect::<Vec<_>>(),
                                "completed": snapshot.events.iter().filter(|event| matches!(event.kind,EventKind::Completed {..})).map(|event|event.run_id).collect::<Vec<_>>(),
                                "interrupted": snapshot.events.iter().filter(|event| matches!(event.kind,EventKind::Interrupted)).map(|event|event.run_id).collect::<Vec<_>>(),
                                "execution": proof_agent.reference(snapshot.id).await.unwrap(),
                                "merges": snapshot.events.iter().filter_map(|event| match event.kind {
                                    EventKind::InputMerged {into_run_id} => Some(serde_json::json!({"input":event.run_id,"into":into_run_id})),
                                    _=>None,
                                }).collect::<Vec<_>>(),
                            });
                            std::fs::write("queue-proof.tmp", proof.to_string()).unwrap();
                            std::fs::rename("queue-proof.tmp", QUEUE_PROOF).unwrap();
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                });
                let (console, input) = Console::start(true).unwrap();
                initialize_startup_console(
                    &console,
                    &tickloop_agent_test_support::config(),
                    &workdir,
                    ExecutionMode::AcceptEdits,
                    Vec::new(),
                ).unwrap();
                interact(&agent, workdir, &console, input, StartupState {
                    launch_arguments: Vec::new(),
                    chat: StartupChat::New,
                                user_settings: None,
                }, None).await.unwrap();
                core.shutdown().await;
            });
        })
        .unwrap()
        .join()
        .unwrap();
}

fn start_queue_fixture(route: &str) -> (RecoveryPty, tempfile::TempDir) {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    std::fs::write(directory.path().join("queue-input.txt"), "owned input").unwrap();
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = CommandBuilder::new(std::env::current_exe().unwrap());
    command.args([
        "--ignored",
        "--exact",
        "terminal::recovery_tests::pending_input_tests::pending_input_pty_fixture",
        "--nocapture",
    ]);
    command.env(QUEUE_FIXTURE, "1");
    command.env(QUEUE_ROUTE, route);
    command.env("TICKLOOP_AGENT_TUI", "fullscreen");
    command.env("TERM", "xterm-256color");
    command.env("HOME", directory.path());
    command.env("TICKLOOP_TMPDIR", directory.path());
    command.cwd(directory.path());
    let child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let writer = pair.master.take_writer().unwrap();
    let output = Arc::new(Mutex::new(Vec::new()));
    let captured = output.clone();
    std::thread::spawn(move || {
        let mut bytes = [0; 8192];
        while let Ok(count) = reader.read(&mut bytes) {
            if count == 0 {
                break;
            }
            captured.lock().unwrap().extend_from_slice(&bytes[..count]);
        }
    });
    (
        RecoveryPty {
            child,
            writer,
            output,
            _master: pair.master,
            timeout: RECOVERY_TIMEOUT,
        },
        directory,
    )
}

fn wait_queue_proof(
    fixture: &RecoveryPty,
    directory: &std::path::Path,
    calls: usize,
    pending: usize,
) -> serde_json::Value {
    let deadline = Instant::now() + RECOVERY_TIMEOUT;
    loop {
        if let Ok(bytes) = std::fs::read(directory.join(QUEUE_PROOF))
            && let Ok(proof) = serde_json::from_slice::<serde_json::Value>(&bytes)
            && proof["calls"] == calls
            && proof["pending"] == pending
        {
            return proof;
        }
        assert!(
            Instant::now() < deadline,
            "no queue proof {calls}/{pending}: {}",
            fixture.raw_output()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn submit_two_pending(fixture: &mut RecoveryPty, directory: &std::path::Path) -> String {
    fixture.wait_for("Tickloop");
    fixture.send(format!("{INITIAL_MESSAGE}\r").as_bytes());
    wait_queue_proof(fixture, directory, 1, 0);
    fixture.wait_for(INITIAL_MESSAGE);
    fixture.send(b"Hello\rHello\r");
    let proof = wait_queue_proof(fixture, directory, 1, 2);
    assert_eq!(proof["hello_messages"], 0);
    fixture.repaint_after_recovery("two pending messages", |frame| {
        frame.contains("Queued 2") || screen::queued_inputs(frame).len() == 2
    })
}

#[test]
fn busy_messages_only_preview_until_the_actual_tool_boundary_consumes_them() {
    let (mut fixture, directory) = start_queue_fixture("tool");
    let frame = submit_two_pending(&mut fixture, directory.path());
    let (body, preview) = frame
        .split_once("Queued 2")
        .or_else(|| screen::split_at_queue(&frame))
        .unwrap();
    assert_eq!(body.matches(QUEUED_MESSAGE).count(), 0, "{frame}");
    assert_eq!(preview.matches(QUEUED_MESSAGE).count(), 2, "{frame}");
    assert_eq!(screen::queued_inputs(&frame).len(), 2, "{frame}");
    std::fs::write(directory.path().join(QUEUE_STAGE), "release-1").unwrap();
    let proof = wait_queue_proof(&fixture, directory.path(), 2, 0);
    assert_eq!(proof["hello_messages"], 2);
    assert_eq!(proof["request_hellos"], 2);
    let consumed = fixture.repaint_after_recovery("two messages consumed", |frame| {
        screen::message_flow(frame).matches(QUEUED_MESSAGE).count() == 2
            && screen::queued_inputs(frame).is_empty()
    });
    assert_eq!(
        screen::message_flow(&consumed)
            .matches(QUEUED_MESSAGE)
            .count(),
        2,
        "{consumed}"
    );
    fixture.send(b"Hello\rHello\r");
    let second_batch = wait_queue_proof(&fixture, directory.path(), 2, 2);
    assert_eq!(second_batch["hello_messages"], 2);
    std::fs::write(directory.path().join(QUEUE_STAGE), "release-2").unwrap();
    let second_consumed = wait_queue_proof(&fixture, directory.path(), 3, 0);
    assert_eq!(second_consumed["hello_messages"], 4);
    assert_eq!(second_consumed["request_hellos"], 4);
    let merges = second_consumed["merges"].as_array().unwrap();
    assert_eq!(merges.len(), 4);
    assert!(
        merges
            .iter()
            .all(|merge| merge["into"] == merges[0]["into"])
    );
    assert_eq!(
        merges
            .iter()
            .map(|merge| merge["input"].as_str().unwrap())
            .collect::<HashSet<_>>()
            .len(),
        4
    );
    let frame = fixture.repaint_after_recovery("second tool boundary input batch", |frame| {
        screen::message_flow(frame).matches(QUEUED_MESSAGE).count() == 4
            && screen::queued_inputs(frame).is_empty()
    });
    assert_eq!(
        screen::message_flow(&frame).matches(QUEUED_MESSAGE).count(),
        4,
        "{frame}"
    );
}

#[test]
fn text_only_completion_consumes_pending_messages_in_one_follow_up_run() {
    let (mut fixture, directory) = start_queue_fixture("plain");
    let queued = submit_two_pending(&mut fixture, directory.path());
    assert_eq!(
        screen::split_at_queue(&queued)
            .unwrap()
            .0
            .matches(QUEUED_MESSAGE)
            .count(),
        0,
        "{queued}"
    );
    assert_eq!(
        screen::split_at_queue(&queued)
            .unwrap()
            .1
            .matches(QUEUED_MESSAGE)
            .count(),
        2,
        "both equal-text submissions remain independently queued: {queued}"
    );
    let before = wait_queue_proof(&fixture, directory.path(), 1, 2);
    let receipts = before["pending_receipts"].as_array().unwrap();
    assert_eq!(receipts.len(), 2);
    assert_ne!(receipts[0]["run"], receipts[1]["run"]);
    assert_eq!(receipts[0]["loop"], receipts[1]["loop"]);
    std::fs::write(directory.path().join(QUEUE_STAGE), "release-1").unwrap();
    let second = wait_queue_proof(&fixture, directory.path(), 2, 0);
    assert_eq!(second["hello_messages"], 2);
    assert_eq!(second["request_hellos"], 2);
    assert_eq!(second["started"].as_array().unwrap().len(), 2);
    assert_eq!(second["started"][1], receipts[0]["run"]);
    assert_eq!(second["completed"].as_array().unwrap().len(), 1);
    assert_eq!(second["merges"].as_array().unwrap().len(), 1);
    assert_eq!(second["merges"][0]["input"], receipts[1]["run"]);
    assert_eq!(second["merges"][0]["into"], receipts[0]["run"]);
    let frame = fixture.repaint_after_recovery("one follow-up consumes both messages", |frame| {
        screen::message_flow(frame).matches(QUEUED_MESSAGE).count() == 2
            && screen::queued_inputs(frame).is_empty()
    });
    assert_eq!(
        screen::message_flow(&frame).matches(QUEUED_MESSAGE).count(),
        2,
        "{frame}"
    );
    std::fs::write(directory.path().join(QUEUE_STAGE), "release-2").unwrap();
    let frame = fixture.repaint_after_recovery("the batched follow-up is answered once", |frame| {
        frame.contains("Queue reply 2")
            && screen::message_flow(frame).matches(QUEUED_MESSAGE).count() == 2
            && screen::queued_inputs(frame).is_empty()
    });
    assert_eq!(
        screen::message_flow(&frame).matches(QUEUED_MESSAGE).count(),
        2,
        "{frame}"
    );
    let settled = wait_queue_proof(&fixture, directory.path(), 2, 0);
    assert_eq!(settled["request_hellos"], 2);
}

#[test]
fn escape_interrupts_the_current_run_without_losing_or_double_echoing_the_queue() {
    let (mut fixture, directory) = start_queue_fixture("plain");
    submit_two_pending(&mut fixture, directory.path());
    let before = wait_queue_proof(&fixture, directory.path(), 1, 2);
    let receipts = before["pending_receipts"].as_array().unwrap();
    fixture.wait_for("esc to interrupt");
    fixture.send(b"\x1b[27u");
    let proof = wait_queue_proof(&fixture, directory.path(), 2, 0);
    assert_eq!(proof["hello_messages"], 2);
    assert_eq!(proof["request_hellos"], 2);
    assert_eq!(proof["interrupted"].as_array().unwrap().len(), 1);
    assert_eq!(proof["started"].as_array().unwrap().len(), 2);
    assert_eq!(proof["started"][1], receipts[0]["run"]);
    assert_eq!(proof["merges"].as_array().unwrap().len(), 1);
    assert_eq!(proof["merges"][0]["input"], receipts[1]["run"]);
    assert_eq!(proof["merges"][0]["into"], receipts[0]["run"]);
    let frame = fixture.repaint_after_recovery(
        "interrupt preserves and consumes the waiting batch",
        |frame| {
            screen::message_flow(frame).matches(QUEUED_MESSAGE).count() == 2
                && screen::queued_inputs(frame).is_empty()
        },
    );
    assert_eq!(
        screen::message_flow(&frame).matches(QUEUED_MESSAGE).count(),
        2,
        "{frame}"
    );
}

#[test]
fn up_takes_back_pending_messages_without_creating_sent_body_echoes() {
    let (mut fixture, directory) = start_queue_fixture("plain");
    submit_two_pending(&mut fixture, directory.path());
    fixture.send(b"\x1b[A");
    let proof = wait_queue_proof(&fixture, directory.path(), 1, 0);
    assert_eq!(proof["hello_messages"], 0);
    let frame = fixture.repaint_after_recovery("pending messages restored for editing", |frame| {
        frame.contains("Hello") && screen::queued_inputs(frame).is_empty()
    });
    let body = frame.split('─').next().unwrap();
    assert_eq!(body.matches(QUEUED_MESSAGE).count(), 0, "{frame}");
    assert_eq!(
        screen::message_flow(&frame).matches(QUEUED_MESSAGE).count(),
        2,
        "{frame}"
    );
}

#[test]
fn messages_submitted_by_another_view_echo_once_when_the_daemon_consumes_them() {
    let (mut fixture, directory) = start_queue_fixture("external");
    fixture.wait_for("Tickloop");
    fixture.send(format!("{INITIAL_MESSAGE}\r").as_bytes());
    wait_queue_proof(&fixture, directory.path(), 1, 0);
    std::fs::write(directory.path().join(OTHER_VIEW_SUBMIT), "").unwrap();
    let proof = wait_queue_proof(&fixture, directory.path(), 1, 2);
    assert_eq!(proof["hello_messages"], 0);
    let queued = fixture.repaint_after_recovery("another View's pending messages", |frame| {
        screen::queued_inputs(frame).len() == 2
    });
    let (body, preview) = screen::split_at_queue(&queued).unwrap();
    assert_eq!(body.matches(QUEUED_MESSAGE).count(), 0, "{queued}");
    assert_eq!(preview.matches(QUEUED_MESSAGE).count(), 2, "{queued}");
    std::fs::write(directory.path().join(QUEUE_STAGE), "release-1").unwrap();
    let proof = wait_queue_proof(&fixture, directory.path(), 2, 0);
    assert_eq!(proof["hello_messages"], 2);
    let consumed = fixture.repaint_after_recovery("another View's consumed messages", |frame| {
        screen::message_flow(frame).matches(QUEUED_MESSAGE).count() == 2
            && screen::queued_inputs(frame).is_empty()
    });
    assert_eq!(
        screen::message_flow(&consumed)
            .matches(QUEUED_MESSAGE)
            .count(),
        2,
        "{consumed}"
    );
    assert_eq!(
        screen::message_flow(&consumed)
            .lines()
            .filter(|line| line.trim_start().starts_with("❯ Hello"))
            .count(),
        2,
        "{consumed}"
    );
}

#[test]
fn reconnect_restores_consumed_messages_whose_snapshots_were_unavailable_at_the_event() {
    let (mut fixture, directory) = start_queue_fixture("tool");
    submit_two_pending(&mut fixture, directory.path());
    std::fs::write(directory.path().join(QUEUE_DISCONNECTED), "").unwrap();
    std::fs::write(directory.path().join(QUEUE_STAGE), "release-1").unwrap();
    let proof = wait_queue_proof(&fixture, directory.path(), 2, 0);
    assert_eq!(proof["hello_messages"], 2);
    fixture.wait_for("Disconnected");
    std::fs::remove_file(directory.path().join(QUEUE_DISCONNECTED)).unwrap();
    let frame = fixture.repaint_after_recovery("reconnected consumed input echoes", |frame| {
        screen::queued_inputs(frame).is_empty()
            && screen::message_flow(frame).matches(QUEUED_MESSAGE).count() == 2
    });
    assert_eq!(
        screen::message_flow(&frame).matches(QUEUED_MESSAGE).count(),
        2,
        "{frame}"
    );
}
