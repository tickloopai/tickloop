use async_trait::async_trait;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use tickloop_agent_console::agent_console::{AgentConsole, Intent};
use tickloop_agent_daemon::EmbeddedAgentAccess;
use tickloop_agent_protocol::{
    AccessError, AgentAccess, AgentSummary, CommandEnvelope, CommandReceipt, DisplayReceipt,
    DisplayUpdate, ExecutionRef, Observation, ObservationRequest, ObservationValue,
};
use uuid::Uuid;

struct Catalog {
    daemon: EmbeddedAgentAccess,
    rows: Mutex<Vec<AgentSummary>>,
}

fn catalog(epoch: Uuid, rows: Vec<AgentSummary>) -> Arc<Catalog> {
    Arc::new(Catalog {
        daemon: EmbeddedAgentAccess::with_epoch(
            tickloop_agent_runtime::Agent::new_embedded(
                tickloop_agent_test_support::config(),
                Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![])),
            ),
            epoch,
        ),
        rows: Mutex::new(rows),
    })
}

#[async_trait]
impl AgentAccess for Catalog {
    async fn observe(&self, query: ObservationRequest) -> Result<Observation, AccessError> {
        let mut observation = self.daemon.observe(query).await?;
        observation.initial = ObservationValue::Overview(self.rows.lock().unwrap().clone());
        Ok(observation)
    }
    async fn execute(&self, _: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
        Err(AccessError::unavailable("read-only catalog"))
    }
    async fn show_view(&self, _: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
        Err(AccessError::unavailable("no host"))
    }
}

fn row(epoch: Uuid, id: u128, parent: Option<ExecutionRef>, state: &str) -> AgentSummary {
    serde_json::from_value(serde_json::json!({"execution":{"daemon_epoch":epoch,"agent_id":Uuid::from_u128(id),"generation":1},"chat_id":Uuid::from_u128(id+100),"folder":"/test","state":state,"title":format!("Root title {id}"),"has_work":true,"waiting_permissions":0,"waiting_questions":0,"parent":parent,"record":null,"workspace":null,"capabilities":[],"views":[],"updated_at":100000,"subagent":parent.map(|_|serde_json::json!({"child_chat_id":Uuid::from_u128(id+100),"handle":format!("child-{id}"),"description":"task","created_at":100000}))})).unwrap()
}

#[tokio::test]
async fn viewed_finished_children_survive_until_thirty_seconds_after_leaving_and_memory_expires() {
    let epoch = Uuid::new_v4();
    let main = row(epoch, 1, None, "Idle");
    let child = row(epoch, 2, Some(main.execution), "Idle");
    let other = row(epoch, 3, None, "Idle");
    let clock = Arc::new(AtomicU64::new(100000));
    let injected = clock.clone();
    let catalog = catalog(epoch, vec![main.clone(), child.clone(), other.clone()]);
    let mut console = AgentConsole::new(catalog.clone(), epoch, "/test".into(), None)
        .with_clock(Arc::new(move || injected.load(Ordering::SeqCst)));
    console.refresh().await.unwrap();
    console.visible_ready(Some(main.execution));
    let view = console
        .interpret(Intent::Open(child.execution))
        .unwrap()
        .unwrap();
    assert!(console.complete(&view));
    clock.store(1_000_000, Ordering::SeqCst);
    console.refresh().await.unwrap();
    assert_eq!(
        console.family(main.execution).len(),
        2,
        "viewed completion has no expiry"
    );
    let away = console
        .interpret(Intent::Open(other.execution))
        .unwrap()
        .unwrap();
    assert!(console.complete(&away));
    clock.store(1_029_999, Ordering::SeqCst);
    console.refresh().await.unwrap();
    assert_eq!(
        console.family(main.execution).len(),
        2,
        "fresh grace starts on leaving"
    );
    clock.store(1_030_000, Ordering::SeqCst);
    console.refresh().await.unwrap();
    assert_eq!(
        console.family(main.execution).len(),
        1,
        "only terminal children retire"
    );
    console.interpret(Intent::Overview).unwrap();
    let returning = console
        .interpret(Intent::Open(main.execution))
        .unwrap()
        .unwrap();
    assert_eq!(
        returning.target, main.execution,
        "retired child is not restored"
    );
    assert_eq!(
        catalog.rows.lock().unwrap().len(),
        3,
        "retirement hides only presentation rows"
    );
    catalog
        .rows
        .lock()
        .unwrap()
        .iter_mut()
        .find(|row| row.execution == child.execution)
        .unwrap()
        .state = tickloop_agent_protocol::LoopState::Running;
    console.refresh().await.unwrap();
    assert_eq!(
        console.family(main.execution).len(),
        2,
        "continued work reappears"
    );
    catalog.daemon.shutdown().await;
}

#[tokio::test]
async fn family_returns_to_last_member_and_missing_child_falls_back_to_root() {
    let epoch = Uuid::new_v4();
    let main = row(epoch, 1, None, "Running");
    let child = row(epoch, 2, Some(main.execution), "Idle");
    let other = row(epoch, 3, None, "Idle");
    let catalog = catalog(epoch, vec![main.clone(), child.clone(), other.clone()]);
    let mut console = AgentConsole::new(catalog.clone(), epoch, "/test".into(), None);
    console.refresh().await.unwrap();
    console.visible_ready(Some(child.execution));
    let away = console
        .interpret(Intent::Open(other.execution))
        .unwrap()
        .unwrap();
    console.complete(&away);
    let returning = console
        .interpret(Intent::Open(main.execution))
        .unwrap()
        .unwrap();
    assert_eq!(returning.target, child.execution);
    console.complete(&returning);
    assert_eq!(
        console
            .family(main.execution)
            .iter()
            .find(|row| row.parent.is_none())
            .unwrap()
            .title,
        "Root title 1"
    );
    let away = console
        .interpret(Intent::Open(other.execution))
        .unwrap()
        .unwrap();
    console.complete(&away);
    catalog
        .rows
        .lock()
        .unwrap()
        .retain(|row| row.execution != child.execution);
    console.refresh().await.unwrap();
    let fallback = console
        .interpret(Intent::Open(main.execution))
        .unwrap()
        .unwrap();
    assert_eq!(fallback.target, main.execution);
    catalog.daemon.shutdown().await;
}

#[tokio::test]
async fn a_new_draft_starts_the_finished_child_departure_grace() {
    let epoch = Uuid::new_v4();
    let main = row(epoch, 1, None, "Idle");
    let child = row(epoch, 2, Some(main.execution), "Idle");
    let clock = Arc::new(AtomicU64::new(100_000));
    let injected = clock.clone();
    let catalog = catalog(epoch, vec![main.clone(), child.clone()]);
    let mut console = AgentConsole::new(catalog.clone(), epoch, "/test".into(), None)
        .with_clock(Arc::new(move || injected.load(Ordering::SeqCst)));
    console.refresh().await.unwrap();
    console.visible_ready(Some(child.execution));
    clock.store(1_000_000, Ordering::SeqCst);
    console.refresh().await.unwrap();
    console.interpret(Intent::Overview).unwrap();
    console.interpret(Intent::NewDraft).unwrap();
    clock.store(1_029_999, Ordering::SeqCst);
    console.refresh().await.unwrap();
    assert_eq!(console.family(main.execution).len(), 2);
    clock.store(1_030_000, Ordering::SeqCst);
    console.refresh().await.unwrap();
    assert_eq!(console.family(main.execution).len(), 1);
    console.interpret(Intent::Overview).unwrap();
    let target = console
        .interpret(Intent::Open(main.execution))
        .unwrap()
        .unwrap();
    assert_eq!(target.target, main.execution);
    catalog.daemon.shutdown().await;
}

#[tokio::test]
async fn a_continuation_completed_between_refreshes_gets_a_new_terminal_grace() {
    for second_completion in [110_000, 130_001] {
        let epoch = Uuid::new_v4();
        let main = row(epoch, 1, None, "Idle");
        let child = row(epoch, 2, Some(main.execution), "Idle");
        let clock = Arc::new(AtomicU64::new(100_000));
        let injected = clock.clone();
        let catalog = catalog(epoch, vec![main.clone(), child]);
        let mut console = AgentConsole::new(catalog.clone(), epoch, "/test".into(), None)
            .with_clock(Arc::new(move || injected.load(Ordering::SeqCst)));
        console.refresh().await.unwrap();
        console.visible_ready(Some(main.execution));
        clock.store(second_completion, Ordering::SeqCst);
        catalog.rows.lock().unwrap()[1].updated_at = Some(second_completion);
        console.refresh().await.unwrap();
        assert_eq!(
            console.family(main.execution).len(),
            2,
            "new completion must reappear"
        );
        clock.store(second_completion + 29_999, Ordering::SeqCst);
        console.refresh().await.unwrap();
        assert_eq!(
            console.family(main.execution).len(),
            2,
            "a full fresh grace"
        );
        clock.store(second_completion + 30_000, Ordering::SeqCst);
        console.refresh().await.unwrap();
        assert_eq!(console.family(main.execution).len(), 1);
        catalog.daemon.shutdown().await;
    }
}

#[tokio::test]
async fn run_identity_rearms_completion_without_a_reported_timestamp() {
    let epoch = Uuid::new_v4();
    let main = row(epoch, 1, None, "Idle");
    let mut child = row(epoch, 2, Some(main.execution), "Idle");
    child.updated_at = None;
    child.run_id = Some(Uuid::from_u128(100));
    let clock = Arc::new(AtomicU64::new(100_000));
    let injected = clock.clone();
    let catalog = catalog(epoch, vec![main.clone(), child]);
    let mut console = AgentConsole::new(catalog.clone(), epoch, "/test".into(), None)
        .with_clock(Arc::new(move || injected.load(Ordering::SeqCst)));
    console.refresh().await.unwrap();
    console.visible_ready(Some(main.execution));
    clock.store(140_000, Ordering::SeqCst);
    console.refresh().await.unwrap();
    assert_eq!(console.family(main.execution).len(), 1);
    catalog.rows.lock().unwrap()[1].run_id = Some(Uuid::from_u128(101));
    console.refresh().await.unwrap();
    assert_eq!(console.family(main.execution).len(), 2);
    clock.store(169_999, Ordering::SeqCst);
    console.refresh().await.unwrap();
    assert_eq!(console.family(main.execution).len(), 2);
    clock.store(170_000, Ordering::SeqCst);
    console.refresh().await.unwrap();
    assert_eq!(console.family(main.execution).len(), 1);
    catalog.daemon.shutdown().await;
}

#[tokio::test]
async fn departure_grace_survives_an_unobserved_completion() {
    let epoch = Uuid::new_v4();
    let main = row(epoch, 1, None, "Idle");
    let child = row(epoch, 2, Some(main.execution), "Running");
    let clock = Arc::new(AtomicU64::new(100_000));
    let injected = clock.clone();
    let catalog = catalog(epoch, vec![main.clone(), child.clone()]);
    let mut console = AgentConsole::new(catalog.clone(), epoch, "/test".into(), None)
        .with_clock(Arc::new(move || injected.load(Ordering::SeqCst)));
    console.refresh().await.unwrap();
    console.visible_ready(Some(child.execution));
    clock.store(200_000, Ordering::SeqCst);
    catalog.rows.lock().unwrap()[1].state = tickloop_agent_protocol::LoopState::Idle;
    catalog.rows.lock().unwrap()[1].updated_at = Some(199_000);
    console.interpret(Intent::NewDraft).unwrap();
    clock.store(229_999, Ordering::SeqCst);
    console.refresh().await.unwrap();
    assert_eq!(console.family(main.execution).len(), 2);
    clock.store(230_000, Ordering::SeqCst);
    console.refresh().await.unwrap();
    assert_eq!(console.family(main.execution).len(), 1);
    catalog.daemon.shutdown().await;
}
