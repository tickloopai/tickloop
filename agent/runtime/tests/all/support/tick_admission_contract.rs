use serde_json::Value;
use tickloop_agent_protocol::{
    Decision, EventKind, ExecutionMode, PermissionRule, TickSubscriptionChange,
};
use tickloop_agent_runtime::Agent;
use tickloop_agent_test_support::{ScriptedModel, call, next_event, text};
use uuid::Uuid;

pub struct AdmissionCase {
    pub name: &'static str,
    pub input: Value,
    pub policy_name: &'static str,
    pub policy_input: Value,
    pub plan_denied: bool,
}

pub async fn verify_admission_policy(
    agent: &Agent,
    model: &ScriptedModel,
    chat: Uuid,
    cases: &[AdmissionCase],
) {
    for mode in [ExecutionMode::FullAccess, ExecutionMode::Plan] {
        agent
            .set_execution_mode(tickloop_ids::Id::from(chat), mode)
            .await
            .unwrap();
        for (decision, revoke) in [
            (Decision::Deny, false),
            (Decision::Ask, false),
            (Decision::Allow, false),
            (Decision::Ask, true),
        ] {
            for case in cases {
                let plan_blocks = mode == ExecutionMode::Plan && case.plan_denied;
                let revoke = revoke && !plan_blocks;
                let rule = |decision| PermissionRule {
                    id: Uuid::new_v4(),
                    decision,
                    tool: case.policy_name.into(),
                    specifier: None,
                    input: Some(case.policy_input.clone()),
                    loop_id: None,
                    persistent: false,
                };
                agent
                    .set_permission_rules(tickloop_ids::Id::from(chat), vec![rule(decision)])
                    .await
                    .unwrap();
                let before = agent
                    .tick_subscriptions(tickloop_ids::Id::from(chat))
                    .await
                    .unwrap();
                model.enqueue(call("admission", case.name, case.input.clone()));
                if !revoke {
                    model.enqueue(text("done"));
                }
                let mut events = agent.subscribe();
                agent
                    .submit(
                        tickloop_ids::Id::from(chat),
                        format!("{mode:?}: {}", case.name),
                    )
                    .await
                    .unwrap();
                let mut asked = 0;
                let mut results = Vec::new();
                let mut started = 0;
                let mut failed = false;
                loop {
                    match next_event(&mut events).await.kind {
                        EventKind::PermissionRequested { request } => {
                            asked += 1;
                            assert_eq!(request.name, case.policy_name, "{}", case.name);
                            assert_eq!(request.input, case.policy_input, "{}", case.name);
                            if revoke {
                                agent
                                    .set_permission_rules(
                                        tickloop_ids::Id::from(chat),
                                        vec![rule(Decision::Deny)],
                                    )
                                    .await
                                    .unwrap();
                            }
                            agent
                                .answer_permission(tickloop_ids::Id::from(chat), request.id, true)
                                .await
                                .unwrap();
                        }
                        EventKind::ToolStarted { .. } => started += 1,
                        EventKind::Failed(reason) => {
                            assert!(revoke, "{}: {reason}", case.name);
                            assert!(
                                reason.contains("permission changed before admission"),
                                "{reason}"
                            );
                            failed = true;
                            break;
                        }
                        EventKind::ToolCompleted { result, .. } => results.push(result),
                        EventKind::Completed { .. } => break,
                        _ => {}
                    }
                }
                assert_eq!(
                    asked,
                    usize::from(decision == Decision::Ask && !plan_blocks),
                    "{} {mode:?}",
                    case.name
                );
                assert_eq!(failed, revoke);
                assert_eq!(
                    started,
                    usize::from(decision != Decision::Deny && !revoke && !plan_blocks)
                );
                assert_eq!(
                    results.len(),
                    usize::from(!revoke),
                    "{} {mode:?}",
                    case.name
                );
                let denied = decision == Decision::Deny || revoke || plan_blocks;
                for result in &results {
                    assert_eq!(
                        result.is_error, denied,
                        "{} {mode:?}: {}",
                        case.name, result.content
                    );
                }
                let after = agent
                    .tick_subscriptions(tickloop_ids::Id::from(chat))
                    .await
                    .unwrap();
                assert_eq!(
                    after.len(),
                    before.len()
                        + usize::from(
                            case.name == "TickManage"
                                && case.input["action"] == "subscribe"
                                && !denied
                        ),
                    "{} {mode:?}",
                    case.name
                );
                // The Chat's one Loop subscribes to an instance once; the next case starts from
                // the same subscriptions.
                for added in after
                    .iter()
                    .filter(|row| before.iter().all(|kept| kept.id != row.id))
                {
                    agent
                        .update_tick_subscription(
                            tickloop_ids::Id::from(chat),
                            added.id,
                            TickSubscriptionChange::Cancel,
                        )
                        .await
                        .unwrap();
                }
            }
        }
    }
}
