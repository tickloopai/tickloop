use crate::support;
use serde_json::json;
use std::sync::Arc;
use tick_protocol::InstanceRef;
use tickloop_agent_runtime::{Agent, tick_adapter::TickAdapters};
use tickloop_agent_test_support::{ScriptedModel, config};

#[tokio::test]
async fn shell_subscription_preserves_its_policy_including_final_admission() {
    use support::tick_admission_contract::{AdmissionCase, verify_admission_policy};
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let task = agent
        .submit_user_shell_for_goal(chat, None, "printf shell-body".into(), true)
        .await
        .unwrap();
    let instance = InstanceRef::shell(task.command_id);
    agent
        .install_tick_stream(Arc::new(TickAdapters::builtin(
            agent.clone(),
            None,
            None,
            None,
        )))
        .await;
    verify_admission_policy(
        &agent,
        &model,
        chat,
        &[AdmissionCase {
            plan_denied: false,
            name: "SubscribeTick",
            input: json!({"tick_instance_id":instance}),
            policy_name: "SubscribeTick",
            policy_input: json!({"tick_instance_id":instance}),
        }],
    )
    .await;
    agent.shutdown().await;
}
