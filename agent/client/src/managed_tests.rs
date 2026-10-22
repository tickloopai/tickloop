use std::sync::atomic::AtomicUsize;

use async_trait::async_trait;
use tickloop_agent_protocol::{
    CommandReceipt, Cursor, DisplayReceipt, DisplayUpdate, PersistenceState, ReceiptState,
};

use super::*;

struct FakeAccess {
    calls: AtomicUsize,
    uncertain: bool,
    failure: bool,
    last_id: std::sync::Mutex<Option<Uuid>>,
}
impl FakeAccess {
    fn new(uncertain: bool, failure: bool) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            uncertain,
            failure,
            last_id: std::sync::Mutex::new(None),
        })
    }
}
fn receipt(epoch: Uuid, command_id: Uuid, failure: bool) -> CommandReceipt {
    CommandReceipt {
        daemon_epoch: epoch,
        command_id,
        target: None,
        state: if failure {
            ReceiptState::Failed
        } else {
            ReceiptState::Completed
        },
        result: (!failure).then_some(CommandResult::Done),
        error: failure.then(|| AccessError::storage("cleanup failed")),
        persistence: PersistenceState::FlushObserved,
    }
}
#[async_trait]
impl AgentAccess for FakeAccess {
    async fn observe(&self, query: ObservationRequest) -> Result<Observation, AccessError> {
        let ObservationRequest::Receipt { epoch, command_id } = query else {
            panic!("unexpected query")
        };
        assert_eq!(*self.last_id.lock().unwrap(), Some(command_id));
        Ok(Observation::new(
            ObservationValue::Receipt(Some(Box::new(receipt(epoch, command_id, self.failure)))),
            Cursor {
                daemon_epoch: epoch,
                execution: None,
                sequence: 0,
            },
            None,
            None,
            None,
        ))
    }
    async fn execute(&self, command: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        *self.last_id.lock().unwrap() = Some(command.command_id);
        if self.uncertain {
            Err(AccessError::new(
                AccessErrorCode::OutcomeUnknown,
                "dispatched but reply lost",
            ))
        } else {
            Ok(receipt(
                command.daemon_epoch,
                command.command_id,
                self.failure,
            ))
        }
    }
    async fn show_view(&self, _: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
        panic!("no View")
    }
}
fn session(access: Arc<dyn AgentAccess>) -> ManagedSession {
    ManagedSession {
        inner: Arc::new(SessionInner {
            access,
            execution: ExecutionRef {
                daemon_epoch: Uuid::new_v4(),
                agent_id: Uuid::new_v4(),
                generation: 1,
            },
            token: ManagedToken::new(),
            released: AtomicBool::new(false),
            stop: CancellationToken::new(),
            lost: CancellationToken::new(),
            lease_error: std::sync::Mutex::new(None),
            release_result: tokio::sync::Mutex::new(None),
        }),
    }
}

#[tokio::test]
async fn unknown_dispatch_reads_original_receipt_without_replay() {
    let fake = FakeAccess::new(true, false);
    let access: Arc<dyn AgentAccess> = fake.clone();
    let epoch = Uuid::new_v4();
    assert!(matches!(
        dispatch(&access, epoch, AgentCommand::FlushTranscripts)
            .await
            .unwrap(),
        CommandResult::Done
    ));
    assert_eq!(fake.calls.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn failed_release_is_not_reclassified_as_success_on_second_call() {
    let fake = FakeAccess::new(false, true);
    let session = session(fake.clone());
    assert_eq!(
        session.release().await.unwrap_err().code,
        AccessErrorCode::Storage
    );
    assert_eq!(
        session.release().await.unwrap_err().code,
        AccessErrorCode::Storage
    );
    assert_eq!(fake.calls.load(Ordering::Relaxed), 1);
    assert_eq!(
        session.observe().await.unwrap_err().code,
        AccessErrorCode::AccessDenied
    );
}

#[tokio::test]
async fn managed_create_rejects_local_profile_without_dispatch() {
    let fake = FakeAccess::new(false, false);
    let client = ManagedClient::new(fake.clone(), Uuid::new_v4(), Uuid::new_v4());
    let error = match client
        .create(LaunchSpec::embedded(PathBuf::from("/tmp")))
        .await
    {
        Ok(_) => panic!("accepted local profile"),
        Err(error) => error,
    };
    assert_eq!(error.code, AccessErrorCode::Invalid);
    assert_eq!(fake.calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn managed_load_rejects_non_acp_and_nonpersistent_without_dispatch() {
    let fake = FakeAccess::new(false, false);
    let client = ManagedClient::new(fake.clone(), Uuid::new_v4(), Uuid::new_v4());
    let mut launch = LaunchSpec::embedded(PathBuf::from("/tmp"));
    assert!(client.load(Uuid::new_v4(), launch.clone()).await.is_err());
    launch.profile = ExecutionProfile::OneShot;
    assert!(client.load(Uuid::new_v4(), launch.clone()).await.is_err());
    launch.profile = ExecutionProfile::Acp;
    launch.session_persistence = false;
    assert!(client.load(Uuid::new_v4(), launch).await.is_err());
    assert_eq!(fake.calls.load(Ordering::Relaxed), 0);
}

struct HangingAccess(AtomicUsize);
#[async_trait]
impl AgentAccess for HangingAccess {
    async fn observe(&self, _: ObservationRequest) -> Result<Observation, AccessError> {
        panic!("no query")
    }
    async fn execute(&self, _: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
        self.0.fetch_add(1, Ordering::Relaxed);
        std::future::pending().await
    }
    async fn show_view(&self, _: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
        panic!("no View")
    }
}

#[tokio::test]
async fn cancelled_release_stops_renewal_and_never_replays() {
    let access = Arc::new(HangingAccess(AtomicUsize::new(0)));
    let session = session(access.clone());
    assert!(
        tokio::time::timeout(Duration::from_millis(10), session.release())
            .await
            .is_err()
    );
    assert!(session.inner.stop.is_cancelled());
    assert_eq!(
        session.release().await.unwrap_err().code,
        AccessErrorCode::OutcomeUnknown
    );
    assert_eq!(access.0.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn explicit_release_is_once_and_blocks_further_commands() {
    let fake = FakeAccess::new(false, false);
    let session = session(fake.clone());
    let second = session.clone();
    let (a, b) = tokio::join!(session.release(), second.release());
    a.unwrap();
    b.unwrap();
    assert_eq!(fake.calls.load(Ordering::Relaxed), 1);
    assert_eq!(
        session.settle(false).await.unwrap_err().code,
        AccessErrorCode::AccessDenied
    );
}

/// Renewals fail with `failure` until `recovers_after` attempts (a daemon handoff), or forever.
struct Renewals {
    calls: AtomicUsize,
    failure: Option<AccessErrorCode>,
    recovers_after: usize,
}
#[async_trait]
impl AgentAccess for Renewals {
    async fn observe(&self, _: ObservationRequest) -> Result<Observation, AccessError> {
        Err(AccessError::unavailable("Agent daemon is not running"))
    }
    async fn execute(&self, command: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
        assert!(matches!(
            command.command,
            AgentCommand::ManagedHeartbeat { .. }
        ));
        if self.calls.fetch_add(1, Ordering::Relaxed) >= self.recovers_after {
            return Ok(receipt(command.daemon_epoch, command.command_id, false));
        }
        match self.failure {
            Some(code) => Err(AccessError::new(code, "renewal failed")),
            None => std::future::pending().await,
        }
    }
    async fn show_view(&self, _: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
        panic!("no View")
    }
}
fn renewing(failure: Option<AccessErrorCode>, recovers_after: usize) -> ManagedSession {
    let session = session(Arc::new(Renewals {
        calls: AtomicUsize::new(0),
        failure,
        recovers_after,
    }));
    tokio::spawn(heartbeat(
        Arc::downgrade(&session.inner),
        session.inner.stop.clone(),
    ));
    session
}

#[tokio::test(start_paused = true)]
async fn unreachable_or_unanswering_daemon_loses_the_lease_only_after_it_expires() {
    for failure in [
        Some(AccessErrorCode::OutcomeUnknown),
        Some(AccessErrorCode::Unavailable),
        None,
    ] {
        let session = renewing(failure, usize::MAX);
        assert!(
            tokio::time::timeout(LEASE - Duration::from_secs(1), session.lease_lost())
                .await
                .is_err(),
            "{failure:?}"
        );
        let error = tokio::time::timeout(HEARTBEAT_INTERVAL * 2, session.lease_lost())
            .await
            .expect("an unrenewed lease must end the managed execution");
        assert_eq!(error.code, AccessErrorCode::StaleExecution);
        assert_eq!(
            error.message,
            "Agent daemon stopped; managed execution lost"
        );
        assert_eq!(
            session.observe().await.unwrap_err().code,
            AccessErrorCode::StaleExecution
        );
    }
}

#[tokio::test(start_paused = true)]
async fn renewal_recovered_within_the_lease_keeps_the_execution() {
    let session = renewing(Some(AccessErrorCode::OutcomeUnknown), 3);
    assert!(
        tokio::time::timeout(LEASE * 4, session.lease_lost())
            .await
            .is_err()
    );
    assert!(session.lease_error().is_none());
}

#[tokio::test(start_paused = true)]
async fn rejected_renewal_loses_the_lease_at_once() {
    let session = renewing(Some(AccessErrorCode::AccessDenied), usize::MAX);
    let error = tokio::time::timeout(
        HEARTBEAT_INTERVAL + Duration::from_secs(1),
        session.lease_lost(),
    )
    .await
    .unwrap();
    assert_eq!(error.code, AccessErrorCode::AccessDenied);
}
