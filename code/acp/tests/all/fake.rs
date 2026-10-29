use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use tickloop_code_access::*;
use tickloop_code_client::managed::ManagedClient;
use tickloop_harness::protocol::*;
use tokio::sync::{Notify, mpsc};
use uuid::Uuid;

pub struct Fake {
    pub epoch: Uuid,
    entries: Mutex<HashMap<Uuid, Entry>>,
    pub releases: AtomicUsize,
    pub submissions: AtomicUsize,
    pub decisions: Mutex<Vec<(RequestRef, bool)>>,
    pub launches: Mutex<Vec<LaunchSpec>>,
    pub started: Notify,
    pub inputs: Mutex<Vec<UserInput>>,
    pub history_requests: AtomicUsize,
    /// The daemon dropped the leases: renewals are rejected and releases never answered.
    pub lease_lost: AtomicBool,
    records: Mutex<HashMap<Uuid, Record>>,
}
struct Record {
    cwd: std::path::PathBuf,
    pages: Vec<Vec<serde_json::Value>>,
}
struct Entry {
    snapshot: LiveChatSnapshot,
    feeds: Vec<mpsc::Sender<ObservationUpdate>>,
    run: Option<Receipt>,
    mode: String,
    configuration: ManagedConfiguration,
    released: bool,
}
fn configuration() -> ManagedConfiguration {
    ManagedConfiguration {
        model_id: "fixture/model".into(),
        mode: ExecutionMode::Auto,
        models: vec![
            ManagedModelInfo {
                id: "fixture/model".into(),
                name: "Fixture".into(),
                provider: "Fixture provider".into(),
            },
            ManagedModelInfo {
                id: "fixture/second".into(),
                name: "Second".into(),
                provider: "Fixture provider".into(),
            },
        ],
        modes: vec![ExecutionMode::Auto, ExecutionMode::AcceptEdits],
        reasoning_effort: None,
        efforts: Vec::new(),
    }
}
impl Fake {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            epoch: tickloop_harness::ids::generate(),
            entries: Mutex::new(HashMap::new()),
            releases: AtomicUsize::new(0),
            submissions: AtomicUsize::new(0),
            decisions: Mutex::new(Vec::new()),
            launches: Mutex::new(Vec::new()),
            started: Notify::new(),
            inputs: Mutex::new(Vec::new()),
            history_requests: AtomicUsize::new(0),
            lease_lost: AtomicBool::new(false),
            records: Mutex::new(HashMap::new()),
        })
    }
    pub fn record(&self, cwd: std::path::PathBuf, pages: Vec<Vec<serde_json::Value>>) -> Uuid {
        let id = tickloop_harness::ids::generate();
        self.records
            .lock()
            .unwrap()
            .insert(id, Record { cwd, pages });
        id
    }
    pub fn chat_ids(&self) -> Vec<Uuid> {
        self.entries
            .lock()
            .unwrap()
            .values()
            .map(|entry| entry.snapshot.conversation.id.uuid())
            .collect()
    }
    pub fn client(self: &Arc<Self>) -> ManagedClient {
        ManagedClient::new(self.clone(), self.epoch, tickloop_harness::ids::generate())
    }
}
impl Entry {
    fn cursor(&self) -> Cursor {
        Cursor {
            daemon_epoch: self.snapshot.execution.daemon_epoch,
            execution: Some(self.snapshot.execution),
            // Connection updates include non-event changes. Keep this deliberately different
            // from the Chat's event watermark so all adapter tests exercise the distinction.
            sequence: self.snapshot.last_sequence + 1_000,
        }
    }
    fn emit(&mut self, kind: EventKind) {
        let receipt = self.run.as_ref().expect("submitted");
        self.snapshot.last_sequence += 1;
        {
            let goal = &mut self.snapshot.conversation.chat_loop;
            if goal.id == receipt.loop_id {
                goal.state = match &kind {
                    EventKind::Completed { .. } => LoopState::Idle,
                    EventKind::Failed(_) => LoopState::Failed,
                    EventKind::Interrupted => LoopState::Interrupted,
                    _ => goal.state.clone(),
                };
            }
        }
        let event = Event {
            sequence: self.snapshot.last_sequence,
            chat_id: receipt.chat_id,
            loop_id: receipt.loop_id,
            run_id: receipt.run_id,
            kind,
        };
        self.snapshot.events.push(event.clone());
        let update = ObservationUpdate {
            cursor: self.cursor(),
            change: ObservationChange::Event(event),
        };
        self.feeds.retain(|feed| !feed.is_closed());
        for feed in &self.feeds {
            let _ = feed.try_send(update.clone());
        }
    }
    fn commit(&mut self, text: &str) {
        self.commit_with(&[text], Vec::new());
    }
    fn commit_with(&mut self, blocks: &[&str], tool_calls: Vec<ProposedToolCall>) {
        let run_id = self.run.as_ref().unwrap().run_id;
        let index = self.snapshot.conversation.messages.len();
        self.snapshot
            .conversation
            .message_runs
            .insert(index, run_id);
        self.snapshot.conversation.messages.push(Message {
            role: Role::Assistant,
            content: blocks
                .iter()
                .map(|text| Content::Text {
                    text: (*text).into(),
                })
                .collect(),
        });
        self.emit(EventKind::AssistantMessageCommitted {
            message_id: tickloop_harness::ids::generate(),
            tool_calls,
        });
    }
    fn finish(&mut self, text: &str) {
        self.emit(EventKind::Text(text.into()));
        self.commit(text);
        self.emit(EventKind::Completed {
            usage: Usage::default(),
        });
    }
}

#[async_trait]
impl AgentAccess for Fake {
    async fn observe(&self, query: ObservationRequest) -> Result<Observation, AccessError> {
        if let ObservationRequest::ManagedSessions { launch, cursor } = &query {
            let mut sessions: Vec<_> = self
                .records
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, record)| record.cwd == launch.folder)
                .map(|(id, record)| ManagedSessionInfo {
                    session_id: *id,
                    cwd: record.cwd.clone(),
                    title: Some("Public fixture".into()),
                    updated_at: "2026-10-22T00:00:00Z".into(),
                })
                .collect();
            sessions.sort_by_key(|session| session.session_id);
            let offset = cursor
                .as_deref()
                .unwrap_or("0")
                .parse::<usize>()
                .map_err(|_| AccessError::invalid("fake list cursor"))?;
            let next_cursor = (offset + 1 < sessions.len()).then(|| (offset + 1).to_string());
            return Ok(Observation::new(
                ObservationValue::ManagedSessions(ManagedSessionPage {
                    sessions: sessions.into_iter().skip(offset).take(1).collect(),
                    next_cursor,
                }),
                Cursor {
                    daemon_epoch: self.epoch,
                    execution: None,
                    sequence: 0,
                },
                None,
                None,
                None,
            ));
        }
        let ObservationRequest::Agent {
            execution, detail, ..
        } = query
        else {
            return Err(AccessError::invalid("fake query"));
        };
        let mut entries = self.entries.lock().unwrap();
        let entry = entries.get_mut(&execution.agent_id).unwrap();
        let (sender, receiver) = mpsc::channel(128);
        entry.feeds.push(sender);
        let initial = match detail {
            AgentDetailQuery::Snapshot => ObservationValue::Agent(Box::new(entry.snapshot.clone())),
            AgentDetailQuery::ManagedConfiguration { .. } => {
                ObservationValue::ManagedConfiguration(entry.configuration.clone())
            }
            AgentDetailQuery::ManagedHistory { cursor, .. } => {
                self.history_requests.fetch_add(1, Ordering::SeqCst);
                let records = self.records.lock().unwrap();
                let record = records
                    .get(&entry.snapshot.conversation.id.uuid())
                    .ok_or_else(|| AccessError::invalid("fake history missing"))?;
                let pages = &record.pages;
                let offset = cursor
                    .as_deref()
                    .unwrap_or("0")
                    .parse::<usize>()
                    .map_err(|_| AccessError::invalid("fake history cursor"))?;
                ObservationValue::ManagedHistory(ManagedHistoryPage {
                    session_id: entry.snapshot.conversation.id.into(),
                    items: pages.get(offset).cloned().unwrap_or_default(),
                    next_cursor: (offset + 1 < pages.len()).then(|| (offset + 1).to_string()),
                })
            }
            AgentDetailQuery::Skills { .. } => ObservationValue::Skills {
                available: None,
                warnings: vec![],
                commands: vec![],
            },
            AgentDetailQuery::OperationReview { .. } => ObservationValue::OperationReview(None),
            AgentDetailQuery::Events { after } => ObservationValue::Events(
                entry
                    .snapshot
                    .events
                    .iter()
                    .filter(|event| event.sequence > after)
                    .cloned()
                    .collect(),
            ),
            _ => return Err(AccessError::invalid("fake detail")),
        };
        Ok(Observation::new(
            initial,
            entry.cursor(),
            None,
            Some(receiver),
            None,
        ))
    }
    async fn execute(&self, envelope: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
        if self.lease_lost.load(Ordering::SeqCst)
            && matches!(envelope.command, AgentCommand::ManagedRelease { .. })
        {
            std::future::pending::<()>().await;
        }
        let mut entries = self.entries.lock().unwrap();
        let mut target = None;
        let result = match envelope.command {
            AgentCommand::CreateManaged { launch, .. } => {
                assert_eq!(launch.profile, ExecutionProfile::Acp);
                self.launches.lock().unwrap().push(launch.clone());
                let execution = ExecutionRef {
                    daemon_epoch: self.epoch,
                    agent_id: tickloop_harness::ids::generate(),
                    generation: 1,
                };
                target = Some(execution);
                let conversation: ChatSnapshot = serde_json::from_value(serde_json::json!({
                    "id":tickloop_harness::ids::generate(), "folder":launch.folder, "messages":[], "chat_loop":LoopSnapshot::new(LoopId::new())
                }))
                .unwrap();
                let snapshot = LiveChatSnapshot::from_snapshot(execution, conversation, Vec::new());
                entries.insert(
                    execution.agent_id,
                    Entry {
                        snapshot: snapshot.clone(),
                        feeds: Vec::new(),
                        run: None,
                        mode: String::new(),
                        configuration: configuration(),
                        released: false,
                    },
                );
                CommandResult::Created {
                    execution,
                    snapshot: Box::new(snapshot),
                    submitted: None,
                    shell: None,
                }
            }
            AgentCommand::ManagedLoad {
                session_uuid,
                launch,
                ..
            } => {
                if entries
                    .values()
                    .any(|entry| entry.snapshot.conversation.id == session_uuid && !entry.released)
                {
                    return Err(AccessError::invalid("fake session is live"));
                }
                let records = self.records.lock().unwrap();
                let record = records
                    .get(&session_uuid)
                    .ok_or_else(|| AccessError::invalid("fake record missing"))?;
                let cwd = &record.cwd;
                if *cwd != launch.folder {
                    return Err(AccessError::invalid("fake cwd mismatch"));
                }
                let execution = ExecutionRef {
                    daemon_epoch: self.epoch,
                    agent_id: tickloop_harness::ids::generate(),
                    generation: 1,
                };
                let conversation = serde_json::from_value(serde_json::json!({"id":session_uuid,"folder":cwd,"chat_loop":LoopSnapshot::new(LoopId::new()),"messages":[{"role":"system","content":[{"type":"text","text":"MODEL_ONLY_SYSTEM_SECRET"}]}]})).unwrap();
                let snapshot = LiveChatSnapshot::from_snapshot(execution, conversation, Vec::new());
                entries.insert(
                    execution.agent_id,
                    Entry {
                        snapshot: snapshot.clone(),
                        feeds: Vec::new(),
                        run: None,
                        mode: String::new(),
                        configuration: configuration(),
                        released: false,
                    },
                );
                target = Some(execution);
                CommandResult::Loaded {
                    execution,
                    snapshot: Box::new(snapshot),
                }
            }
            AgentCommand::ManagedConfigure {
                execution, change, ..
            } => {
                let entry = entries.get_mut(&execution.agent_id).unwrap();
                if entry.snapshot.conversation.chat_loop.state == LoopState::Running {
                    return Err(AccessError::invalid("fake session is busy"));
                }
                if let Some(model) = change.model_id {
                    entry.configuration.model_id = model;
                }
                if let Some(mode) = change.mode {
                    entry.configuration.mode = mode;
                }
                CommandResult::ManagedConfigured(entry.configuration.clone())
            }
            AgentCommand::ManagedSubmit {
                execution, input, ..
            } => {
                self.submissions.fetch_add(1, Ordering::SeqCst);
                self.inputs.lock().unwrap().push(input.clone());
                let entry = entries.get_mut(&execution.agent_id).unwrap();
                let UserInput::Text { text, .. } = input else {
                    panic!("text only");
                };
                entry.mode = text;
                let receipt = Receipt {
                    chat_id: entry.snapshot.conversation.id,
                    loop_id: entry.snapshot.conversation.chat_loop.id,
                    run_id: RunId::new(),
                };
                entry.run = Some(receipt.clone());

                entry.snapshot.conversation.chat_loop = LoopSnapshot {
                    id: receipt.loop_id,
                    receiving: Default::default(),
                    subscriptions: Vec::new(),
                    state: LoopState::Running,
                    run_id: Some(receipt.run_id),
                    run_started_at: None,
                    state_since: None,
                    event_sources: Vec::new(),
                    runs_started: None,
                    compactions: None,
                };
                entry.emit(EventKind::Started);
                match entry.mode.as_str() {
                    "large" => entry.finish(&"x".repeat(2 * 1024 * 1024 + 1)),
                    "flood" => {
                        for _ in 0..128 {
                            entry.emit(EventKind::Text("x".repeat(16 * 1024)));
                        }
                        entry.commit(&"x".repeat(2 * 1024 * 1024));
                        entry.emit(EventKind::Completed {
                            usage: Usage::default(),
                        });
                    }
                    "wait" => entry.emit(EventKind::Text("partial".into())),
                    "tool-kinds" => {
                        for (index, name, input) in [
                            (
                                0,
                                "Read",
                                serde_json::json!({"file_path":"/fixture/read.rs"}),
                            ),
                            (
                                1,
                                "Write",
                                serde_json::json!({"file_path":"/fixture/write.rs"}),
                            ),
                            (2, "Grep", serde_json::json!({"path":"/fixture/src"})),
                            (3, "Bash", serde_json::json!({"cwd":"/fixture"})),
                            (
                                4,
                                "WebFetch",
                                serde_json::json!({"url":"https://example.invalid"}),
                            ),
                            (5, "Read", serde_json::json!({"file_path":"relative.rs"})),
                        ] {
                            let call_id = format!("kind-{index}");
                            entry.emit(EventKind::ToolStarted {
                                call_id: call_id.clone(),
                                name: name.into(),
                                input,
                            });
                            entry.emit(EventKind::ToolCompleted {
                                sources: Vec::new(),
                                call_id,
                                result: ToolResult {
                                    content: "done".into(),
                                    is_error: false,
                                    output_ref: None,
                                },
                            });
                        }
                        entry.finish("done");
                    }
                    "permission" | "mcp-permission" => {
                        let host = entry.mode == "mcp-permission";
                        let request = PermissionRequest {
                            parent_call: None,
                            mcp_server: None,
                            id: tickloop_harness::ids::generate(),
                            loop_id: receipt.loop_id,
                            execution: if host {
                                PermissionExecution::HostOperation {
                                    run_id: receipt.run_id,
                                    operation_id: tickloop_harness::ids::generate(),
                                }
                            } else {
                                PermissionExecution::ToolCall {
                                    run_id: receipt.run_id,
                                    call_id: "tool-1".into(),
                                }
                            },
                            name: if host { "MCPServerStart" } else { "Edit" }.into(),
                            input: if host {
                                serde_json::json!({"server":"private-fixture","program":"/fixture/private-command","env_names":["PRIVATE_ENV"]})
                            } else {
                                serde_json::json!({"file_path":"example.rs"})
                            },
                            file_change: None,
                            approval_reason: None,
                            approval_source: PermissionApprovalSource::ExplicitAsk,
                        };
                        entry.snapshot.permissions.push(request.clone());
                        entry.snapshot.request_refs.push(RequestRef {
                            execution,
                            request_id: request.id,
                            kind: PendingRequestKind::Permission,
                            version: 7,
                        });
                        entry.emit(EventKind::PermissionRequested { request });
                    }
                    "missing-terminal" => {
                        entry.snapshot.conversation.chat_loop.state = LoopState::Idle;
                        for feed in &entry.feeds {
                            let _ = feed.try_send(ObservationUpdate {
                                cursor: entry.cursor(),
                                change: ObservationChange::Changed,
                            });
                        }
                    }
                    "unknown" => {
                        entry.snapshot.operations.push(OperationRecord {
                            parent_call_id: None,
                            parent_call_index: None,
                            task_binding: None,
                            id: tickloop_harness::ids::generate(),
                            loop_id: receipt.loop_id,
                            run_id: receipt.run_id,
                            call_id: "unknown-tool".into(),
                            name: "Write".into(),
                            input: serde_json::json!({}),
                            file_change: None,
                            state: OperationState::Unknown,
                            result: None,
                        });
                        entry.finish("maybe");
                    }
                    "waiting" => {
                        entry.finish("not settled");
                        entry.snapshot.conversation.chat_loop.state = LoopState::WaitingQuestion;
                    }
                    "policy-denied" => {
                        entry.emit(EventKind::AssistantMessageCommitted {
                            message_id: tickloop_harness::ids::generate(),
                            tool_calls: vec![tickloop_harness::protocol::ProposedToolCall {
                                call_id: "denied-tool".into(),
                                name: "Write".into(),
                                input: serde_json::json!({"file_path":"example.rs"}),
                            }],
                        });
                        entry.emit(EventKind::ToolCompleted {
                            sources: Vec::new(),
                            call_id: "denied-tool".into(),
                            result: ToolResult {
                                content: "Denied by policy".into(),
                                is_error: true,
                                output_ref: None,
                            },
                        });
                        entry.finish("denied");
                    }
                    "failed" => entry.emit(EventKind::Failed("scripted failure".into())),
                    // Streamed per chunk, as Gemini does: one text block per chunk.
                    "chunked" => {
                        let chunks = ["Hello", " ", "world"];
                        for chunk in chunks {
                            entry.emit(EventKind::Text(chunk.into()));
                        }
                        entry.commit_with(&chunks, Vec::new());
                        entry.emit(EventKind::Completed {
                            usage: Usage::default(),
                        });
                    }
                    "fail-after-commit" => {
                        entry.emit(EventKind::Text("working".into()));
                        entry.commit_with(
                            &["working"],
                            vec![ProposedToolCall {
                                call_id: "never-run".into(),
                                name: "Write".into(),
                                input: serde_json::json!({"file_path":"example.rs"}),
                            }],
                        );
                        entry.emit(EventKind::Text("partial".into()));
                        entry.emit(EventKind::Failed("API key missing".into()));
                    }
                    // Supplementary tool content well past the turn budget, inside the feed.
                    "tool-flood" => {
                        for index in 0..40 {
                            let call_id = format!("flood-{index}");
                            entry.emit(EventKind::ToolStarted {
                                call_id: call_id.clone(),
                                name: "Bash".into(),
                                input: serde_json::json!({"command":"x".repeat(31 * 1024)}),
                            });
                            entry.emit(EventKind::ToolCompleted {
                                sources: Vec::new(),
                                call_id,
                                result: ToolResult {
                                    content: "y".repeat(31 * 1024),
                                    is_error: false,
                                    output_ref: None,
                                },
                            });
                        }
                        entry.finish("survived");
                    }
                    "auth" => entry.emit(EventKind::Failed("API key missing".into())),
                    "multi" => {
                        entry.emit(EventKind::Text("working".into()));
                        entry.commit("working");
                        entry.emit(EventKind::ToolStarted {
                            call_id: "read-1".into(),
                            name: "Read".into(),
                            input: serde_json::json!({"file_path":"example.rs"}),
                        });
                        entry.emit(EventKind::ToolCompleted {
                            sources: Vec::new(),
                            call_id: "read-1".into(),
                            result: ToolResult {
                                content: "read".into(),
                                is_error: false,
                                output_ref: None,
                            },
                        });
                        entry.finish("done");
                    }
                    "reset" => {
                        let feeds = std::mem::take(&mut entry.feeds);
                        entry.finish("recovered");
                        entry.feeds = feeds;
                        let update = ObservationUpdate {
                            cursor: entry.cursor(),
                            change: ObservationChange::Reset {
                                reason: ResetReason::Lagged,
                                snapshot: Box::new(entry.snapshot.clone()),
                            },
                        };
                        for feed in &entry.feeds {
                            let _ = feed.try_send(update.clone());
                        }
                    }
                    "duplicate" => {
                        entry.finish("once");
                        let update = ObservationUpdate {
                            cursor: entry.cursor(),
                            change: ObservationChange::Reset {
                                reason: ResetReason::Lagged,
                                snapshot: Box::new(entry.snapshot.clone()),
                            },
                        };
                        for feed in &entry.feeds {
                            let _ = feed.try_send(update.clone());
                        }
                    }
                    "status" => {
                        entry.emit(EventKind::Thinking("weighing options".into()));
                        entry.snapshot.conversation.generated_title = Some("Fixture title".into());
                        entry.snapshot.context_window_tokens = Some(100_000);
                        entry.finish("done");
                    }
                    "thought-flood" => {
                        for _ in 0..80 {
                            entry.emit(EventKind::Thinking("x".repeat(16 * 1024)));
                        }
                        entry.finish("answered");
                    }
                    _ => entry.finish("hello"),
                }
                self.started.notify_waiters();
                CommandResult::Submitted(receipt)
            }
            AgentCommand::ManagedDecidePermission {
                request,
                allow,
                scope,
                ..
            } => {
                assert!(matches!(scope, PermissionScope::Once));
                self.decisions.lock().unwrap().push((request, allow));
                let entry = entries.get_mut(&request.execution.agent_id).unwrap();
                assert_eq!(request.version, 7);
                let pending = entry.snapshot.permissions[0].clone();
                let call_id = pending.execution.call_key();
                entry.snapshot.permissions.clear();
                entry.snapshot.request_refs.clear();
                if allow {
                    entry.emit(EventKind::ToolStarted {
                        call_id: call_id.clone(),
                        name: pending.name,
                        input: pending.input,
                    });
                }
                entry.emit(EventKind::ToolCompleted {
                    sources: Vec::new(),
                    call_id,
                    result: ToolResult {
                        content: if allow { "edited" } else { "denied" }.into(),
                        is_error: !allow,
                        output_ref: None,
                    },
                });
                entry.finish(if allow { "allowed" } else { "rejected" });
                CommandResult::Done
            }
            AgentCommand::ManagedSettle {
                execution, cancel, ..
            } => {
                let entry = entries.get_mut(&execution.agent_id).unwrap();
                if cancel
                    && entry.run.is_some()
                    && !entry.snapshot.events.last().is_some_and(|event| {
                        matches!(
                            event.kind,
                            EventKind::Completed { .. }
                                | EventKind::Interrupted
                                | EventKind::Failed(_)
                        )
                    })
                {
                    entry.snapshot.permissions.clear();
                    entry.snapshot.request_refs.clear();
                    if entry.mode == "wait" {
                        entry.commit("partial");
                    }
                    entry.emit(EventKind::Interrupted);
                }
                CommandResult::ManagedSettled {
                    snapshot: Box::new(entry.snapshot.clone()),
                }
            }
            AgentCommand::ManagedRelease { execution, .. } => {
                self.releases.fetch_add(1, Ordering::SeqCst);
                entries.get_mut(&execution.agent_id).unwrap().released = true;
                CommandResult::Done
            }
            AgentCommand::ManagedHeartbeat { .. } if self.lease_lost.load(Ordering::SeqCst) => {
                return Err(AccessError::new(
                    AccessErrorCode::AccessDenied,
                    "fake lease rejected",
                ));
            }
            AgentCommand::ManagedHeartbeat { .. } => CommandResult::Done,
            _ => return Err(AccessError::invalid("fake command")),
        };
        Ok(CommandReceipt {
            daemon_epoch: self.epoch,
            command_id: envelope.command_id,
            target,
            state: ReceiptState::Completed,
            result: Some(result),
            error: None,
            persistence: PersistenceState::NotApplicable,
        })
    }
    async fn show_view(&self, _: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
        Err(AccessError::invalid("no view"))
    }
}
