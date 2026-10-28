//! Wire round trips of AgentAccess values that the golden fixtures do not pin.

use tick_protocol::InstanceRef;
use tickloop_code_access::{AgentCommand, CommandResult, ExecutionRef, LiveChatSnapshot};
use tickloop_harness::protocol::{
    ChatSnapshot, FamilyCompletion, FamilyMessageRejection, FamilyMessageRejectionCode, LoopState,
    MonitorMember, SubagentNotificationDelivery, Usage,
};
use uuid::Uuid;

#[test]
fn monitor_members_and_required_push_completion_fields_share_one_snapshot_wire() {
    let execution = ExecutionRef {
        daemon_epoch: Uuid::new_v4(),
        agent_id: Uuid::new_v4(),
        generation: 1,
    };
    let instance: InstanceRef = "hosted:counter".parse().unwrap();
    for notification in [
        SubagentNotificationDelivery::Foreground,
        SubagentNotificationDelivery::Pending,
        SubagentNotificationDelivery::Queued,
        SubagentNotificationDelivery::Rejected(FamilyMessageRejection {
            code: FamilyMessageRejectionCode::StaleTarget,
            message: "The receiving parent is unavailable".into(),
        }),
    ] {
        let mut snapshot: ChatSnapshot = serde_json::from_value(serde_json::json!({
            "id":execution.agent_id, "folder":"/test", "messages":[],
            "chat_loop": {"id": Uuid::new_v4(), "state": "Idle"},
        }))
        .unwrap();
        snapshot.monitor.members.push(MonitorMember {
            min_rows: None,
            is_terminal: false,
            instance: instance.clone(),
            title: Some("Counter".into()),
        });
        snapshot.family_completions.push(FamilyCompletion {
            notification,
            usage: Usage {
                input_tokens: Some(11),
                output_tokens: Some(7),
                ..Default::default()
            },
            total_tokens: Some(18),
            child_chat_id: tickloop_ids::Id::from(Uuid::new_v4()),
            child_loop_id: tickloop_ids::Id::from(Uuid::new_v4()),
            child_run_id: tickloop_ids::Id::from(Uuid::new_v4()),
            handle: "audit".into(),
            state: LoopState::Idle,
            result: "Actual child report".into(),
            error: None,
            started_at: Some(1),
            finished_at: 2,
            triggering_message_ids: vec![],
            run_status: None,
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
    let instance: InstanceRef = "hosted:counter".parse().unwrap();
    for command in [
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
