//! What each Trigger source declares to the wake-up base: how its deliveries wait, what their
//! content is, how many events one merges, and the shape a queued delivery is saved in.

use tick_protocol::InstanceRef;
use tickloop_agent_protocol::{
    Authority, Coalesce, PauseReason, Receiving, TickDeliveryItem, TriggerKind, TriggerSource,
};
use uuid::Uuid;

fn tick_update(subscription: Uuid, sequence: u64, updates: u64) -> TriggerSource {
    TriggerSource::TickUpdate {
        items: vec![TickDeliveryItem {
            subscription,
            watch: Uuid::from_u128(1),
            instance: InstanceRef::new("calendar", "fixture").unwrap(),
            sequence,
            updates,
        }],
    }
}

fn manual() -> TriggerSource {
    TriggerSource::TickManual {
        instance: InstanceRef::new("calendar", "fixture").unwrap(),
        sequence: 4,
        read_at_ms: 10,
        authority: None,
    }
}

fn feedback(delivery: u128) -> TriggerSource {
    TriggerSource::ArtifactFeedback {
        origin: "https://docs.example".into(),
        account_id: "owner".into(),
        artifact_id: "doc".into(),
        feedback_id: format!("feedback-{delivery}"),
        delivery_id: Uuid::from_u128(delivery),
    }
}

fn goal(goal_id: u128, after_run: u128) -> TriggerSource {
    TriggerSource::GoalContinuation {
        goal_id: Uuid::from_u128(goal_id),
        after_run: Uuid::from_u128(after_run),
        objective_revision: 1,
    }
}

fn scheduled(task_id: &str, scheduled_for_ms: u64) -> TriggerSource {
    TriggerSource::ScheduledTask {
        task_id: task_id.into(),
        scheduled_for_ms,
    }
}

#[test]
fn each_source_declares_its_coalescing_and_content() {
    let declared = [
        (
            tick_update(Uuid::from_u128(2), 1, 3),
            TriggerKind::TickUpdate,
            Coalesce::LatestOnly,
            Authority::Data,
            3,
        ),
        (
            manual(),
            TriggerKind::TickManual,
            Coalesce::EachItem,
            Authority::Data,
            1,
        ),
        (
            feedback(5),
            TriggerKind::ArtifactFeedback,
            Coalesce::EachItem,
            Authority::Data,
            1,
        ),
        (
            goal(6, 7),
            TriggerKind::GoalContinuation,
            Coalesce::OnePerKey(Uuid::from_u128(6).to_string()),
            Authority::Instruction,
            1,
        ),
        (
            scheduled("0a1b2c3d", 1_000),
            TriggerKind::ScheduledTask,
            Coalesce::OnePerKey("0a1b2c3d".into()),
            Authority::Instruction,
            1,
        ),
    ];
    for (source, kind, coalesce, authority, merged) in declared {
        assert_eq!(source.kind(), kind);
        assert_eq!(source.coalesce(), coalesce, "{kind:?}");
        assert_eq!(source.authority(), authority, "{kind:?}");
        assert_eq!(source.merged(), merged, "{kind:?}");
    }
}

#[test]
fn deliveries_wait_together_only_as_their_source_coalesces() {
    let first_tick = tick_update(Uuid::from_u128(2), 1, 1);
    let other_tick = tick_update(Uuid::from_u128(3), 9, 1);
    assert!(
        first_tick.shares_wait_with(&other_tick),
        "only the latest waits"
    );
    assert!(!first_tick.shares_wait_with(&manual()));
    assert!(
        !feedback(5).shares_wait_with(&feedback(5)),
        "each request waits"
    );
    assert!(!manual().shares_wait_with(&manual()));
    assert!(goal(6, 7).shares_wait_with(&goal(6, 8)), "one per Goal");
    assert!(!goal(6, 7).shares_wait_with(&goal(9, 7)));
    assert!(
        scheduled("a", 1).shares_wait_with(&scheduled("a", 2)),
        "one per task"
    );
    assert!(!scheduled("a", 1).shares_wait_with(&scheduled("b", 1)));
    assert!(!scheduled("a", 1).shares_wait_with(&goal(6, 7)));
}

#[test]
fn a_queued_delivery_names_its_source_by_kind() {
    assert_eq!(
        serde_json::to_value(feedback(5)).unwrap(),
        serde_json::json!({
            "kind": "artifact_feedback",
            "origin": "https://docs.example",
            "account_id": "owner",
            "artifact_id": "doc",
            "feedback_id": "feedback-5",
            "delivery_id": Uuid::from_u128(5),
        })
    );
    assert_eq!(
        serde_json::to_value(scheduled("0a1b2c3d", 1_000)).unwrap(),
        serde_json::json!({"kind": "scheduled_task", "task_id": "0a1b2c3d", "scheduled_for_ms": 1_000})
    );
    assert!(
        serde_json::from_value::<TriggerSource>(serde_json::json!({
            "source": "tick_subscription", "payload": {}
        }))
        .is_err(),
        "a delivery saved by name before its source had a type does not load"
    );
}

#[test]
fn the_receiving_switch_says_why_it_paused() {
    assert!(Receiving::default().is_receiving());
    for reason in [PauseReason::Stopped, PauseReason::HistoryLoaded] {
        let paused = Receiving::Paused { reason };
        assert!(!paused.is_receiving());
        let wire = serde_json::to_value(paused).unwrap();
        assert_eq!(serde_json::from_value::<Receiving>(wire).unwrap(), paused);
    }
    assert_eq!(
        serde_json::to_value(Receiving::Paused {
            reason: PauseReason::HistoryLoaded
        })
        .unwrap(),
        serde_json::json!({"state": "paused", "reason": "history_loaded"})
    );
}
