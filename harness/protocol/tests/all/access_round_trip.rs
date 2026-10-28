//! Wire round trips and invariants of shared Harness snapshot values.

use tick_protocol::InstanceRef;
use tickloop_harness_protocol::MonitorMember;
use uuid::Uuid;

#[test]
fn subagent_colors_prefer_unused_members_and_older_registrations_decode_without_one() {
    use tickloop_harness_protocol::{AgentColor, SubagentRegistration};
    assert_eq!(AgentColor::next_for(&[]), AgentColor::Red);
    assert_eq!(
        AgentColor::next_for(&[Some(AgentColor::Red), None, Some(AgentColor::Green)]),
        AgentColor::Blue
    );
    let mut taken = AgentColor::ALL.map(Some).to_vec();
    assert_eq!(AgentColor::next_for(&taken), AgentColor::Red);
    taken.push(Some(AgentColor::Red));
    assert_eq!(AgentColor::next_for(&taken), AgentColor::Blue);

    let older: SubagentRegistration = serde_json::from_value(serde_json::json!({
        "child_chat_id": Uuid::nil(), "handle": "review", "description": "Review", "created_at": 1,
    }))
    .unwrap();
    assert_eq!(older.color, None);
    let colored = SubagentRegistration {
        color: Some(AgentColor::Purple),
        ..older
    };
    let wire = serde_json::to_value(&colored).unwrap();
    assert_eq!(wire["color"], "purple");
    let decoded: SubagentRegistration = serde_json::from_value(wire).unwrap();
    assert_eq!(decoded.color, Some(AgentColor::Purple));
}

#[test]
fn handled_marks_only_move_forward_and_stay_bounded() {
    use tickloop_harness_protocol::MonitorMembership;
    let mut membership = MonitorMembership::default();
    let instance: InstanceRef = "hosted:counter".parse().unwrap();
    assert!(membership.mark_handled(instance.clone(), 2_000));
    assert!(!membership.mark_handled(instance.clone(), 1_000));
    assert_eq!(membership.handled_at(&instance), Some(2_000));
    for index in 0..600_u64 {
        membership.mark_handled(
            InstanceRef::new("hosted", &format!("run-{index}")).unwrap(),
            10_000 + index,
        );
    }
    assert_eq!(membership.handled.len(), 512);
    assert_eq!(
        membership.handled_at(&instance),
        None,
        "the oldest mark goes first"
    );
    assert_eq!(
        membership.for_new_chat().handled,
        membership.handled,
        "a Branch knows what its source already handled"
    );
    let marks_only = MonitorMembership {
        handled: [(instance, 3_000)].into(),
        ..Default::default()
    };
    assert!(
        !marks_only.is_empty(),
        "a Chat whose only Monitor facts are handled marks still sends them"
    );
}

#[test]
fn an_agent_turn_taking_updates_in_handles_them_and_says_so() {
    use tickloop_harness_protocol::MonitorMembership;
    let mut membership = MonitorMembership::default();
    let opened: InstanceRef = "hosted:opened".parse().unwrap();
    let delivered: InstanceRef = "hosted:delivered".parse().unwrap();
    assert!(membership.mark_handled(opened.clone(), 2_000));
    assert!(membership.mark_taken_in(delivered.clone(), 2_000));
    assert_eq!(membership.handled_at(&delivered), Some(2_000));
    assert_eq!(membership.taken_in_at(&delivered), Some(2_000));
    assert_eq!(
        membership.taken_in_at(&opened),
        None,
        "opening a row handles it without an Agent turn"
    );
    assert!(!membership.mark_taken_in(opened.clone(), 1_000));
    assert_eq!(
        membership.taken_in_at(&opened),
        None,
        "an older take-in neither moves the mark nor claims the later handling"
    );
    for index in 0..600_u64 {
        membership.mark_handled(
            InstanceRef::new("hosted", &format!("run-{index}")).unwrap(),
            10_000 + index,
        );
    }
    assert!(
        membership.taken_in.is_empty(),
        "a take-in leaves with its handled mark"
    );
    let older: MonitorMembership =
        serde_json::from_value(serde_json::json!({ "handled": { "hosted:opened": 2000 } }))
            .unwrap();
    assert!(older.taken_in.is_empty(), "16.3 membership still decodes");
}

#[test]
fn used_marks_only_move_forward_stay_bounded_and_stay_with_their_chat() {
    use tickloop_harness_protocol::MonitorMembership;
    let mut membership = MonitorMembership::default();
    let instance: InstanceRef = "hosted:quotes".parse().unwrap();
    assert!(membership.mark_used(instance.clone(), 2_000));
    assert!(!membership.mark_used(instance.clone(), 1_000));
    assert!(!membership.mark_used(instance.clone(), 2_000));
    assert_eq!(membership.used_at(&instance), Some(2_000));
    assert_eq!(
        membership.handled_at(&instance),
        None,
        "using an instance handles nothing by itself"
    );
    assert!(
        !membership.is_empty(),
        "a Chat whose only Monitor facts are used marks still sends them"
    );
    assert!(
        membership.for_new_chat().used.is_empty(),
        "a Branch's Agent has used nothing yet"
    );
    for index in 0..600_u64 {
        membership.mark_used(
            InstanceRef::new("hosted", &format!("run-{index}")).unwrap(),
            10_000 + index,
        );
    }
    assert_eq!(membership.used.len(), 512);
    assert_eq!(
        membership.used_at(&instance),
        None,
        "the oldest mark goes first"
    );
    let older: MonitorMembership =
        serde_json::from_value(serde_json::json!({ "taken_in": { "hosted:quotes": 2000 } }))
            .unwrap();
    assert!(older.used.is_empty(), "20.5 membership still decodes");
}

#[test]
fn a_membership_saved_with_member_sources_loads_every_member_as_opened() {
    use tickloop_harness_protocol::MonitorMembership;
    let saved: MonitorMembership = serde_json::from_value(serde_json::json!({
        "members": [
            { "instance": "hosted:opened", "source": "opened", "title": "Quotes", "min_rows": 9 },
            { "instance": "hosted:added", "source": "added" },
        ],
        "removed": ["hosted:hidden"],
    }))
    .unwrap();
    assert_eq!(
        saved.members,
        [
            MonitorMember {
                is_terminal: false,
                instance: "hosted:opened".parse().unwrap(),
                title: Some("Quotes".into()),
                min_rows: Some(9),
            },
            MonitorMember {
                is_terminal: false,
                instance: "hosted:added".parse().unwrap(),
                title: None,
                min_rows: None,
            },
        ]
    );
    assert!(saved.is_removed(&"hosted:hidden".parse().unwrap()));
    assert!(
        serde_json::to_string(&saved.members[1]).unwrap() == r#"{"is_terminal":false,"instance":"hosted:added"}"#,
        "an undeclared minimum is left out"
    );
    assert!(
        !serde_json::to_string(&saved).unwrap().contains("source"),
        "the source is no longer written"
    );
}

#[test]
fn a_tick_subscription_saved_with_a_cadence_in_seconds_is_not_converted() {
    let subscription = |timing: (&str, serde_json::Value)| {
        let mut wire = serde_json::json!({
            "id": Uuid::from_u128(1),
            "instance": "hosted:00000000-0000-0000-0000-000000000040",
            "authority": null,
        });
        wire[timing.0] = timing.1;
        serde_json::from_value::<tickloop_harness_protocol::TickSubscription>(wire)
    };
    assert!(subscription(("every_seconds", serde_json::json!(60))).is_err());
    assert!(subscription(("frequency", serde_json::json!(60))).is_err());
    assert_eq!(
        subscription(("frequency", serde_json::json!("low")))
            .unwrap()
            .frequency,
        tickloop_harness_protocol::TickSubscriptionFrequency::Low
    );
}
