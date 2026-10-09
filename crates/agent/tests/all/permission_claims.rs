use crate::support;

use std::{
    ffi::CString,
    fs::{File, OpenOptions},
    io::Write,
    os::unix::{
        ffi::OsStrExt,
        fs::{FileTypeExt, OpenOptionsExt, PermissionsExt},
    },
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use tickloop_agent::{
    agent_access::*,
    runtime::{Agent, EventKind, LoopState},
};
use tickloop_agent_config::UserSettings;
use tickloop_agent_protocol::{Decision, PermissionScope};
use tokio::{net::UnixListener, task::JoinHandle};
use uuid::Uuid;

const WAIT: Duration = Duration::from_secs(5);
const ACCEPTED_WRITE_WAIT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug)]
enum Adapter {
    Embedded,
    Ipc,
}

struct Fixture {
    directory: tempfile::TempDir,
    _external: tempfile::TempDir,
    root: PathBuf,
    runtime: Agent,
    model: Arc<support::ScriptedModel>,
    first: Arc<dyn AgentAccess>,
    second: Arc<dyn AgentAccess>,
    server: Option<JoinHandle<()>>,
    execution: ExecutionRef,
    loop_id: Uuid,
    request: RequestRef,
}

impl Fixture {
    async fn new(adapter: Adapter) -> Self {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let external = tempfile::tempdir_in("/tmp").unwrap();
        let source = external.path().join("source.txt");
        std::fs::write(&source, "scoped original information").unwrap();
        let root = directory.path().join("state");
        let model = Arc::new(support::ScriptedModel::new(vec![
            support::call(
                "first-read",
                "Read",
                serde_json::json!({"file_path": source}),
            ),
            support::text("First read completed."),
            support::call(
                "second-read",
                "Read",
                serde_json::json!({"file_path": source}),
            ),
            support::text("Second read completed."),
        ]));
        let runtime = Agent::open_embedded(root.clone(), support::config(), model.clone()).unwrap();
        let embedded = Arc::new(EmbeddedAgentAccess::new(runtime.clone()));
        let epoch = embedded.daemon_epoch();
        let (first, second, server): (Arc<dyn AgentAccess>, Arc<dyn AgentAccess>, _) = match adapter
        {
            Adapter::Embedded => (embedded.clone(), embedded, None),
            Adapter::Ipc => {
                let socket = directory.path().join("claims.sock");
                let listener = UnixListener::bind(&socket).unwrap();
                let server = tokio::spawn(async move {
                    serve_listener(listener, embedded, epoch, support::TRANSPORT_LIMITS)
                        .await
                        .unwrap();
                });
                let first = tokio::time::timeout(WAIT, LocalAgentClient::connect(&socket))
                    .await
                    .unwrap()
                    .unwrap();
                let second = tokio::time::timeout(WAIT, LocalAgentClient::connect(&socket))
                    .await
                    .unwrap()
                    .unwrap();
                assert_ne!(first.session_id(), second.session_id());
                (Arc::new(first), Arc::new(second), Some(server))
            }
        };
        let created = execute(
            &*first,
            envelope(
                epoch,
                AgentCommand::Create {
                    launch: LaunchSpec::embedded(directory.path().to_path_buf()),
                    initial: Some(UserInput::Text {
                        text: "Read the external source".into(),
                        contexts: vec![],
                        paths: vec![],
                    }),
                    origin: None,
                },
            ),
        )
        .await
        .into_result()
        .unwrap();
        let CommandResult::Created {
            execution,
            submitted: Some(submitted),
            ..
        } = created
        else {
            panic!("expected submitted Create");
        };
        let request = tokio::time::timeout(WAIT, async {
            loop {
                if let Some(request) = snapshot(&*first, execution)
                    .await
                    .request_refs
                    .into_iter()
                    .find(|request| request.kind == PendingRequestKind::Permission)
                {
                    break request;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        Self {
            directory,
            _external: external,
            root,
            runtime,
            model,
            first,
            second,
            server,
            execution,
            loop_id: submitted.loop_id,
            request,
        }
    }

    async fn completed(&self, message: &str) {
        tokio::time::timeout(WAIT, async {
            loop {
                let current = snapshot(&*self.second, self.execution).await;
                if current.conversation.messages.iter().flat_map(|message| &message.content).any(|content| matches!(content, tickloop_agent_protocol::Content::Text { text } if text == message)) {
                    assert!(current.permissions.is_empty());
                    break;
                }
                tokio::task::yield_now().await;
            }
        }).await.unwrap();
    }

    async fn repeat_read(&self) -> Option<RequestRef> {
        execute(
            &*self.first,
            envelope(
                self.execution.daemon_epoch,
                AgentCommand::Submit {
                    execution: self.execution,
                    target_loop: Some(self.loop_id),
                    input: UserInput::Text {
                        text: "Read the exact same source in the original Loop".into(),
                        contexts: vec![],
                        paths: vec![],
                    },
                    origin: None,
                },
            ),
        )
        .await
        .into_result()
        .unwrap();
        tokio::time::timeout(WAIT, async {
            loop {
                let current = snapshot(&*self.second, self.execution).await;
                if let Some(request) = current.request_refs.into_iter().find(|request| request.kind == PendingRequestKind::Permission) { break Some(request); }
                if current.conversation.messages.iter().flat_map(|message| &message.content).any(|content| matches!(content, tickloop_agent_protocol::Content::Text { text } if text == "Second read completed.")) { break None; }
                tokio::task::yield_now().await;
            }
        }).await.unwrap()
    }

    async fn shutdown(&self) {
        tokio::time::timeout(WAIT, self.runtime.shutdown())
            .await
            .unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(server) = &self.server {
            server.abort();
        }
    }
}

fn envelope(epoch: Uuid, command: AgentCommand) -> CommandEnvelope {
    CommandEnvelope {
        daemon_epoch: epoch,
        command_id: Uuid::new_v4(),
        command,
    }
}

fn answer(request: RequestRef, scope: PermissionScope, allow: bool) -> CommandEnvelope {
    envelope(
        request.execution.daemon_epoch,
        AgentCommand::DecidePermission {
            request,
            allow,
            scope,
            comment: None,
            rule: None,
        },
    )
}

async fn execute(access: &dyn AgentAccess, command: CommandEnvelope) -> CommandReceipt {
    tokio::time::timeout(WAIT, access.execute(command))
        .await
        .unwrap()
        .unwrap()
}

async fn snapshot(access: &dyn AgentAccess, execution: ExecutionRef) -> LiveChatSnapshot {
    let observation = tokio::time::timeout(
        WAIT,
        access.observe(ObservationRequest::Agent {
            execution,
            detail: AgentDetailQuery::Snapshot,
            after: None,
        }),
    )
    .await
    .unwrap()
    .unwrap();
    let ObservationValue::Agent(snapshot) = observation.initial else {
        panic!("expected live snapshot");
    };
    *snapshot
}

async fn receipt(access: &dyn AgentAccess, epoch: Uuid, command_id: Uuid) -> CommandReceipt {
    let observation = tokio::time::timeout(
        WAIT,
        access.observe(ObservationRequest::Receipt { epoch, command_id }),
    )
    .await
    .unwrap()
    .unwrap();
    let ObservationValue::Receipt(Some(receipt)) = observation.initial else {
        panic!("accepted command must stay queryable");
    };
    *receipt
}

struct SettingsReadGate {
    path: PathBuf,
    seed: Vec<u8>,
    writer: Option<File>,
}

impl SettingsReadGate {
    fn new(path: PathBuf, seed: Vec<u8>) -> Self {
        if path.exists() {
            std::fs::remove_file(&path).unwrap();
        }
        let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(
            unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) },
            0,
            "could not create owned settings FIFO"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        Self {
            path,
            seed,
            writer: None,
        }
    }

    async fn reader_arrived(&mut self) {
        let writer = tokio::time::timeout(WAIT, async {
            loop {
                match OpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(&self.path)
                {
                    Ok(writer) => break writer,
                    Err(error) if error.raw_os_error() == Some(libc::ENXIO) => {
                        tokio::task::yield_now().await
                    }
                    Err(error) => panic!("owned settings FIFO failed: {error}"),
                }
            }
        })
        .await
        .expect("an authorized settings writer must reach the real storage read");
        self.writer = Some(writer);
        self.writer.as_mut().unwrap().write_all(&self.seed).unwrap();
    }

    fn restore_fifo(&mut self) -> std::io::Result<()> {
        if std::fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| metadata.file_type().is_fifo())
        {
            // Keep a writer open through unlink so an in-flight blocking reader always gets EOF.
            if self.writer.is_none() {
                let mut writer = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(&self.path)?;
                writer.write_all(&self.seed)?;
                self.writer = Some(writer);
            }
            std::fs::remove_file(&self.path)?;
            let mut restored = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&self.path)?;
            restored.write_all(&self.seed)?;
        }
        Ok(())
    }

    fn release(mut self) {
        let result = self.restore_fifo();
        self.writer.take();
        result.unwrap();
    }
}

impl Drop for SettingsReadGate {
    fn drop(&mut self) {
        if let Err(error) = self.restore_fifo() {
            eprintln!("owned settings FIFO restoration failed: {error}");
        }
        self.writer.take();
    }
}

async fn race_once_against(scope: PermissionScope) {
    for adapter in [Adapter::Embedded, Adapter::Ipc] {
        for once_is_first in [true, false] {
            let fixture = Fixture::new(adapter).await;
            let mut gate = (scope == PermissionScope::Persistent)
                .then(|| SettingsReadGate::new(fixture.root.join("settings.json"), b"{}".to_vec()));
            let barrier = Arc::new(tokio::sync::Barrier::new(3));
            let once = answer(fixture.request, PermissionScope::Once, true);
            let scoped = answer(fixture.request, scope, true);
            let once_id = once.command_id;
            let scoped_id = scoped.command_id;
            let (once_view, scoped_view) = if once_is_first {
                (fixture.first.clone(), fixture.second.clone())
            } else {
                (fixture.second.clone(), fixture.first.clone())
            };
            let once_ready = barrier.clone();
            let once_task = tokio::spawn(async move {
                tokio::time::timeout(WAIT, once_ready.wait()).await.unwrap();
                execute(&*once_view, once).await
            });
            let scoped_ready = barrier.clone();
            let scoped_task = tokio::spawn(async move {
                tokio::time::timeout(WAIT, scoped_ready.wait())
                    .await
                    .unwrap();
                tokio::time::timeout(ACCEPTED_WRITE_WAIT, scoped_view.execute(scoped))
                    .await
                    .unwrap()
                    .unwrap()
            });
            tokio::time::timeout(WAIT, barrier.wait()).await.unwrap();
            let once_result = tokio::time::timeout(WAIT, once_task)
                .await
                .unwrap()
                .unwrap();
            let once_won = matches!(once_result.state, ReceiptState::Completed);
            if let Some(mut gate) = gate.take() {
                if !once_won {
                    gate.reader_arrived().await;
                    assert_eq!(
                        snapshot(&*fixture.second, fixture.execution)
                            .await
                            .permissions
                            .len(),
                        1
                    );
                    assert!(matches!(
                        receipt(&*fixture.second, fixture.execution.daemon_epoch, scoped_id)
                            .await
                            .state,
                        ReceiptState::Accepted | ReceiptState::Running
                    ));
                }
                gate.release();
            }
            let scoped_result = tokio::time::timeout(WAIT, scoped_task)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                usize::from(matches!(once_result.state, ReceiptState::Completed))
                    + usize::from(matches!(scoped_result.state, ReceiptState::Completed)),
                1,
                "two Views must share one effective permission response"
            );
            let (winner, loser) = if once_won {
                (&once_result, &scoped_result)
            } else {
                (&scoped_result, &once_result)
            };
            assert!(winner.error.is_none());
            assert!(matches!(loser.state, ReceiptState::Failed));
            assert!(matches!(
                loser.error.as_ref().unwrap().code,
                AccessErrorCode::Responding | AccessErrorCode::AlreadyResolved
            ));
            for original in [&once_result, &scoped_result] {
                let known = receipt(
                    &*fixture.second,
                    fixture.execution.daemon_epoch,
                    original.command_id,
                )
                .await;
                assert_eq!(known.target, Some(fixture.execution));
                assert_eq!(
                    matches!(known.state, ReceiptState::Completed),
                    matches!(original.state, ReceiptState::Completed)
                );
            }
            fixture.completed("First read completed.").await;
            assert_eq!(fixture.model.requests.lock().unwrap().len(), 2);
            let persistent_won = !once_won && scope == PermissionScope::Persistent;
            let saved = UserSettings::in_root(&fixture.root)
                .permission_rules(&fixture.directory.path().canonicalize().unwrap());
            assert_eq!(
                saved.len(),
                usize::from(persistent_won),
                "an unclaimed Persistent answer must not write a project rule"
            );
            assert_eq!(
                snapshot(&*fixture.second, fixture.execution)
                    .await
                    .policy
                    .rules
                    .len(),
                usize::from(persistent_won)
            );

            let next = fixture.repeat_read().await;
            assert_eq!(
                next.is_some(),
                once_won,
                "a losing Session or Persistent answer must not authorize the next identical operation"
            );
            if let Some(next) = next {
                assert_ne!(next.request_id, fixture.request.request_id);
                let stale = tokio::time::timeout(
                    WAIT,
                    fixture.second.execute(answer(fixture.request, scope, true)),
                )
                .await
                .unwrap()
                .unwrap();
                assert_eq!(stale.error.unwrap().code, AccessErrorCode::AlreadyResolved);
                assert_eq!(
                    snapshot(&*fixture.second, fixture.execution)
                        .await
                        .permissions[0]
                        .id,
                    next.request_id
                );
                tokio::time::timeout(
                    WAIT,
                    fixture
                        .second
                        .execute(answer(next, PermissionScope::Once, true)),
                )
                .await
                .unwrap()
                .unwrap()
                .into_result()
                .unwrap();
            }
            fixture.completed("Second read completed.").await;
            assert_eq!(fixture.model.requests.lock().unwrap().len(), 4);
            assert_eq!(
                receipt(&*fixture.second, fixture.execution.daemon_epoch, once_id)
                    .await
                    .command_id,
                once_id
            );
            eprintln!("permission competition: {adapter:?}, {scope:?}, once_won={once_won}");
            fixture.shutdown().await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn once_and_session_answers_share_one_claim_and_only_the_winner_grants_authority() {
    race_once_against(PermissionScope::Session).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn once_and_persistent_answers_share_one_claim_and_only_the_winner_writes_a_rule() {
    race_once_against(PermissionScope::Persistent).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_completed_once_answer_leaves_losing_session_and_persistent_answers_without_authority() {
    for adapter in [Adapter::Embedded, Adapter::Ipc] {
        for scope in [PermissionScope::Session, PermissionScope::Persistent] {
            let fixture = Fixture::new(adapter).await;
            let once = execute(
                &*fixture.first,
                answer(fixture.request, PermissionScope::Once, true),
            )
            .await;
            assert!(matches!(once.state, ReceiptState::Completed));
            fixture.completed("First read completed.").await;
            let gate = (scope == PermissionScope::Persistent)
                .then(|| SettingsReadGate::new(fixture.root.join("settings.json"), b"{}".to_vec()));
            let rejected = execute(&*fixture.second, answer(fixture.request, scope, true)).await;
            assert!(matches!(rejected.state, ReceiptState::Failed));
            assert_eq!(
                rejected.error.as_ref().unwrap().code,
                AccessErrorCode::AlreadyResolved
            );
            assert_eq!(rejected.target, Some(fixture.execution));
            let known = receipt(
                &*fixture.first,
                fixture.execution.daemon_epoch,
                rejected.command_id,
            )
            .await;
            assert!(matches!(known.state, ReceiptState::Failed));
            assert_eq!(known.error.unwrap().code, AccessErrorCode::AlreadyResolved);
            if let Some(gate) = gate {
                let writer = OpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(&gate.path);
                assert!(
                    matches!(writer, Err(error) if error.raw_os_error() == Some(libc::ENXIO)),
                    "an unclaimed Persistent answer must not reach the settings write's read"
                );
                gate.release();
            }
            assert!(
                snapshot(&*fixture.second, fixture.execution)
                    .await
                    .policy
                    .rules
                    .is_empty()
            );
            assert!(
                UserSettings::in_root(&fixture.root)
                    .permission_rules(&fixture.directory.path().canonicalize().unwrap())
                    .is_empty()
            );
            let next = fixture.repeat_read().await.expect(
                "an unclaimed Session or Persistent answer must not authorize the next read",
            );
            assert_ne!(next.request_id, fixture.request.request_id);
            execute(&*fixture.second, answer(next, PermissionScope::Once, true))
                .await
                .into_result()
                .unwrap();
            fixture.completed("Second read completed.").await;
            assert_eq!(fixture.model.requests.lock().unwrap().len(), 4);
            assert!(matches!(
                receipt(
                    &*fixture.second,
                    fixture.execution.daemon_epoch,
                    once.command_id
                )
                .await
                .state,
                ReceiptState::Completed
            ));
            eprintln!("Once completed before scoped answer: {adapter:?}, {scope:?}");
            fixture.shutdown().await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn permission_claim_before_a_blocked_persistent_write_rejects_once_and_session_side_effects()
{
    for adapter in [Adapter::Embedded, Adapter::Ipc] {
        let fixture = Fixture::new(adapter).await;
        let mut gate = SettingsReadGate::new(fixture.root.join("settings.json"), b"{}".to_vec());
        let persistent = answer(fixture.request, PermissionScope::Persistent, true);
        let command_id = persistent.command_id;
        let first = fixture.first.clone();
        let writing = tokio::spawn(async move {
            tokio::time::timeout(ACCEPTED_WRITE_WAIT, first.execute(persistent))
                .await
                .unwrap()
                .unwrap()
        });
        gate.reader_arrived().await;
        let accepted = receipt(&*fixture.second, fixture.execution.daemon_epoch, command_id).await;
        assert!(matches!(
            accepted.state,
            ReceiptState::Accepted | ReceiptState::Running
        ));
        assert_eq!(accepted.target, Some(fixture.execution));
        assert!(
            snapshot(&*fixture.second, fixture.execution)
                .await
                .policy
                .rules
                .is_empty()
        );
        for scope in [PermissionScope::Once, PermissionScope::Session] {
            let losing = answer(fixture.request, scope, false);
            let id = losing.command_id;
            let rejected = tokio::time::timeout(WAIT, fixture.second.execute(losing))
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(rejected.state, ReceiptState::Failed));
            assert_eq!(rejected.error.unwrap().code, AccessErrorCode::Responding);
            assert_eq!(
                receipt(&*fixture.second, fixture.execution.daemon_epoch, id)
                    .await
                    .error
                    .unwrap()
                    .code,
                AccessErrorCode::Responding
            );
            assert_eq!(
                snapshot(&*fixture.second, fixture.execution)
                    .await
                    .permissions
                    .len(),
                1
            );
        }
        gate.release();
        let finished = tokio::time::timeout(WAIT, writing).await.unwrap().unwrap();
        assert!(matches!(finished.state, ReceiptState::Completed));
        fixture.completed("First read completed.").await;
        let saved = UserSettings::in_root(&fixture.root)
            .permission_rules(&fixture.directory.path().canonicalize().unwrap());
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].decision, Decision::Allow);
        assert!(saved[0].persistent);
        assert!(
            fixture.repeat_read().await.is_none(),
            "an unclaimed Session denial must not survive the winning persistent grant"
        );
        fixture.completed("Second read completed.").await;
        assert_eq!(fixture.model.requests.lock().unwrap().len(), 4);
        fixture.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_authorized_persistent_write_survives_run_interrupt_with_a_queryable_failed_answer() {
    for adapter in [Adapter::Embedded, Adapter::Ipc] {
        let fixture = Fixture::new(adapter).await;
        let mut gate = SettingsReadGate::new(fixture.root.join("settings.json"), b"{}".to_vec());
        let persistent = answer(fixture.request, PermissionScope::Persistent, true);
        let command_id = persistent.command_id;
        let first = fixture.first.clone();
        let writing = tokio::spawn(async move {
            tokio::time::timeout(ACCEPTED_WRITE_WAIT, first.execute(persistent))
                .await
                .unwrap()
                .unwrap()
        });
        gate.reader_arrived().await;
        let accepted = receipt(&*fixture.second, fixture.execution.daemon_epoch, command_id).await;
        assert!(matches!(
            accepted.state,
            ReceiptState::Accepted | ReceiptState::Running
        ));
        assert_eq!(
            snapshot(&*fixture.second, fixture.execution)
                .await
                .permissions
                .len(),
            1
        );

        tokio::time::timeout(
            WAIT,
            fixture.second.execute(envelope(
                fixture.execution.daemon_epoch,
                AgentCommand::ManageLoop {
                    execution: fixture.execution,
                    action: LoopAction::Interrupt,
                },
            )),
        )
        .await
        .unwrap()
        .unwrap()
        .into_result()
        .unwrap();
        let interrupted = tokio::time::timeout(WAIT, async {
            loop {
                let current = snapshot(&*fixture.second, fixture.execution).await;
                if current.permissions.is_empty()
                    && current.conversation.loops.iter().any(|goal| {
                        goal.id == fixture.loop_id && goal.state == LoopState::Interrupted
                    })
                {
                    break current;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            interrupted.policy.rules.is_empty(),
            "reader arrival does not mean that the rule was saved"
        );
        assert!(!interrupted.events.iter().any(|event| matches!(&event.kind, EventKind::ToolCompleted { call_id, .. } if call_id == "first-read")));
        assert_eq!(fixture.model.requests.lock().unwrap().len(), 1);
        assert!(matches!(
            receipt(&*fixture.second, fixture.execution.daemon_epoch, command_id)
                .await
                .state,
            ReceiptState::Accepted | ReceiptState::Running
        ));

        gate.release();
        let finished = tokio::time::timeout(WAIT, writing).await.unwrap().unwrap();
        assert!(matches!(finished.state, ReceiptState::Failed));
        assert_eq!(finished.target, Some(fixture.execution));
        let failure = finished.error.unwrap();
        assert_eq!(failure.code, AccessErrorCode::Invalid);
        assert!(failure.message.contains("permission request expired"));
        let queried = receipt(&*fixture.second, fixture.execution.daemon_epoch, command_id).await;
        assert!(matches!(queried.state, ReceiptState::Failed));
        assert_eq!(queried.error.unwrap().code, AccessErrorCode::Invalid);
        let current = snapshot(&*fixture.second, fixture.execution).await;
        assert!(current.permissions.is_empty());
        assert_eq!(current.policy.rules.len(), 1);
        assert_eq!(current.policy.rules[0].decision, Decision::Allow);
        assert!(current.policy.rules[0].persistent);
        assert!(!current.events.iter().any(|event| matches!(&event.kind, EventKind::ToolCompleted { call_id, .. } if call_id == "first-read")));
        assert_eq!(
            fixture.model.requests.lock().unwrap().len(),
            1,
            "saving an authorized rule after Interrupt must not execute the cancelled tool"
        );
        let saved = UserSettings::in_root(&fixture.root)
            .permission_rules(&fixture.directory.path().canonicalize().unwrap());
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].decision, Decision::Allow);
        let stale = tokio::time::timeout(
            WAIT,
            fixture
                .second
                .execute(answer(fixture.request, PermissionScope::Once, true)),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(stale.error.unwrap().code, AccessErrorCode::AlreadyResolved);
        fixture.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_persistent_write_is_queryable_and_requires_a_fresh_request_version() {
    for adapter in [Adapter::Embedded, Adapter::Ipc] {
        let fixture = Fixture::new(adapter).await;
        let mut gate =
            SettingsReadGate::new(fixture.root.join("settings.json"), b"invalid JSON".to_vec());
        let persistent = answer(fixture.request, PermissionScope::Persistent, true);
        let command_id = persistent.command_id;
        let first = fixture.first.clone();
        let writing = tokio::spawn(async move {
            tokio::time::timeout(ACCEPTED_WRITE_WAIT, first.execute(persistent))
                .await
                .unwrap()
                .unwrap()
        });
        gate.reader_arrived().await;
        assert!(matches!(
            receipt(&*fixture.second, fixture.execution.daemon_epoch, command_id)
                .await
                .state,
            ReceiptState::Accepted | ReceiptState::Running
        ));
        gate.release();
        let failure = tokio::time::timeout(WAIT, writing).await.unwrap().unwrap();
        assert!(matches!(failure.state, ReceiptState::Failed));
        assert_eq!(failure.error.unwrap().code, AccessErrorCode::Storage);
        let known = receipt(&*fixture.second, fixture.execution.daemon_epoch, command_id).await;
        assert!(matches!(known.state, ReceiptState::Failed));
        assert_eq!(known.error.unwrap().code, AccessErrorCode::Storage);
        let current = snapshot(&*fixture.second, fixture.execution).await;
        assert!(current.policy.rules.is_empty());
        assert_eq!(current.permissions.len(), 1);
        let retry = current
            .request_refs
            .into_iter()
            .find(|request| request.kind == PendingRequestKind::Permission)
            .unwrap();
        assert_eq!(retry.request_id, fixture.request.request_id);
        assert_eq!(retry.version, fixture.request.version + 1);
        let stale = tokio::time::timeout(
            WAIT,
            fixture
                .second
                .execute(answer(fixture.request, PermissionScope::Session, true)),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(stale.error.unwrap().code, AccessErrorCode::VersionConflict);
        std::fs::write(fixture.root.join("settings.json"), b"{}").unwrap();
        tokio::time::timeout(
            WAIT,
            fixture
                .second
                .execute(answer(retry, PermissionScope::Once, true)),
        )
        .await
        .unwrap()
        .unwrap()
        .into_result()
        .unwrap();
        fixture.completed("First read completed.").await;
        let next = fixture.repeat_read().await.expect(
            "neither the failed write nor the unclaimed Session answer may grant authority",
        );
        assert_ne!(next.request_id, fixture.request.request_id);
        tokio::time::timeout(
            WAIT,
            fixture
                .second
                .execute(answer(next, PermissionScope::Once, true)),
        )
        .await
        .unwrap()
        .unwrap()
        .into_result()
        .unwrap();
        fixture.completed("Second read completed.").await;
        assert!(
            UserSettings::in_root(&fixture.root)
                .permission_rules(&fixture.directory.path().canonicalize().unwrap())
                .is_empty()
        );
        assert_eq!(fixture.model.requests.lock().unwrap().len(), 4);
        fixture.shutdown().await;
    }
}
