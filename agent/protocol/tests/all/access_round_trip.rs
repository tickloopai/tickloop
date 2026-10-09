//! Wire round trips of AgentAccess values that the golden fixtures do not pin.

use tick_protocol::InstanceRef;
use tickloop_agent_protocol::{
    AgentCommand, ChatSnapshot, CommandResult, ExecutionRef, FamilyCompletion,
    FamilyMessageRejection, FamilyMessageRejectionCode, LiveChatSnapshot, LoopState, MonitorMember,
    MonitorMemberSource, SubagentNotificationDelivery, Usage,
};
use uuid::Uuid;

#[test]
fn monitor_members_and_required_push_completion_fields_share_one_snapshot_wire() {
    let execution = ExecutionRef {
        daemon_epoch: Uuid::new_v4(),
        agent_id: Uuid::new_v4(),
        generation: 1,
    };
    let instance: InstanceRef = "herdr:counter".parse().unwrap();
    for notification in [
        SubagentNotificationDelivery::Foreground,
        SubagentNotificationDelivery::Pending,
        SubagentNotificationDelivery::Queued,
        SubagentNotificationDelivery::Rejected(FamilyMessageRejection {
            code: FamilyMessageRejectionCode::StaleTarget,
            message: "The receiving parent is stopped".into(),
        }),
    ] {
        let mut snapshot: ChatSnapshot = serde_json::from_value(serde_json::json!({
            "id":execution.agent_id, "workdir":"/test", "messages":[], "loops":[],
        }))
        .unwrap();
        snapshot.monitor.members.push(MonitorMember {
            instance: instance.clone(),
            source: MonitorMemberSource::Added,
            title: Some("Counter".into()),
        });
        snapshot.family_completions.push(FamilyCompletion {
            notification,
            usage: Usage {
                input_tokens: Some(11),
                output_tokens: Some(7),
                ..Default::default()
            },
            child_chat_id: Uuid::new_v4(),
            child_loop_id: Uuid::new_v4(),
            child_run_id: Uuid::new_v4(),
            handle: "audit".into(),
            state: LoopState::Completed,
            result: "Actual child report".into(),
            error: None,
            started_at: Some(1),
            finished_at: 2,
            triggering_message_ids: vec![],
        });
        let response = CommandResult::Loaded {
            execution,
            snapshot: Box::new(LiveChatSnapshot::from_snapshot(execution, snapshot, vec![])),
        };
        let wire = serde_json::to_value(response).unwrap();
        let decoded: CommandResult = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), wire);
        for required in ["notification", "usage"] {
            let mut missing = wire.clone();
            missing["Loaded"]["snapshot"]["conversation"]["family_completions"][0]
                .as_object_mut()
                .unwrap()
                .remove(required);
            assert!(
                serde_json::from_value::<CommandResult>(missing).is_err(),
                "{required} is part of the single required wire shape"
            );
        }
    }
}

#[test]
fn monitor_membership_commands_cross_the_wire_unchanged() {
    let execution = ExecutionRef {
        daemon_epoch: Uuid::new_v4(),
        agent_id: Uuid::new_v4(),
        generation: 1,
    };
    let instance: InstanceRef = "herdr:counter".parse().unwrap();
    for command in [
        AgentCommand::AddMonitorMember {
            execution,
            instance: instance.clone(),
            title: Some("Counter".into()),
        },
        AgentCommand::RemoveMonitorMember {
            execution,
            instance: instance.clone(),
        },
        AgentCommand::MarkMonitorHandled {
            execution,
            instance: instance.clone(),
        },
    ] {
        let wire = serde_json::to_value(&command).unwrap();
        let decoded: AgentCommand = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(&decoded).unwrap(), wire);
        assert_eq!(decoded.execution(), Some(execution));
    }
}

#[test]
fn subagent_colors_prefer_unused_members_and_older_registrations_decode_without_one() {
    use tickloop_agent_protocol::{AgentColor, SubagentRegistration};
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
    use tickloop_agent_protocol::MonitorMembership;
    let mut membership = MonitorMembership::default();
    let instance: InstanceRef = "herdr:counter".parse().unwrap();
    assert!(membership.mark_handled(instance.clone(), 2_000));
    assert!(!membership.mark_handled(instance.clone(), 1_000));
    assert_eq!(membership.handled_at(&instance), Some(2_000));
    for index in 0..600_u64 {
        membership.mark_handled(
            InstanceRef::native(format!("run-{index}")).unwrap(),
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
