use std::{sync::Arc, time::Duration};

use serde_json::json;
use tickloop_harness_protocol::{
    AgentHostOptions, ApprovalOutcome, Decision, EventKind, ExecutionMode, PermissionRule,
    PromptPolicy,
};
use tickloop_harness_runtime::{
    Agent, HarnessOptions, HarnessSeams,
    environment::{
        ExecutionIsolation, NetworkIsolation, WriteIsolation, memory::MemoryEnvironment,
    },
};
use tickloop_harness_test_support::{
    ScriptedModel, auto_classification, call, config, harness::ChosenModels, next_event, text,
};

#[tokio::test]
async fn prepared_environment_approval_controls_routing_without_platform_assumptions() {
    for isolation in [
        ExecutionIsolation::default(),
        ExecutionIsolation {
            writes: WriteIsolation::FolderAndDirectories,
            network: NetworkIsolation::Disconnected,
        },
    ] {
        for required in [false, true] {
            for block in [false, true] {
                for (mode, explicit) in [
                    (ExecutionMode::Auto, None),
                    (ExecutionMode::AcceptEdits, None),
                    (ExecutionMode::Auto, Some(Decision::Ask)),
                    (ExecutionMode::Auto, Some(Decision::Deny)),
                ] {
                    let folder = tempfile::tempdir().unwrap();
                    let environment = MemoryEnvironment::new();
                    environment.set_isolation(isolation);
                    environment.set_shell_approval_required(required);
                    environment.define_shell("pwd", Duration::ZERO, b"portable-result".to_vec(), 0);
                    let model = Arc::new(ScriptedModel::new(vec![
                        call(
                            "prepared",
                            "Bash",
                            json!({"command":"pwd","dangerouslyDisableSandbox":true}),
                        ),
                        text("done"),
                    ]));
                    let reviewer = Arc::new(ScriptedModel::new(vec![
                        text(&auto_classification(block).to_string()),
                        text(&auto_classification(block).to_string()),
                    ]));
                    let agent = Agent::with_seams(
                        HarnessSeams {
                            search: None,
                            transcripts: None,
                            environment: Some(Arc::new(environment)),
                            models: ChosenModels::new(model),
                        },
                        HarnessOptions {
                            host: AgentHostOptions {
                                // Even an unrestricted prompt host cannot waive required approval.
                                prompt_policy: PromptPolicy::Unrestricted,
                                ..AgentHostOptions::embedded_defaults()
                            },
                            ..HarnessOptions::local(config())
                        },
                    )
                    .unwrap();
                    agent.set_auto_reviewer(reviewer.clone()).await;
                    let chat = agent.create_chat(folder.path().into()).await.unwrap();
                    agent.set_execution_mode(chat, mode).await.unwrap();
                    if let Some(decision) = explicit {
                        agent
                            .set_permission_rules(
                                chat,
                                vec![PermissionRule {
                                    id: tickloop_ids::generate(),
                                    decision,
                                    tool: "Bash".into(),
                                    specifier: None,
                                    input: None,
                                    loop_id: None,
                                    persistent: false,
                                }],
                            )
                            .await
                            .unwrap();
                    }
                    let mut events = agent.subscribe();
                    let receipt = agent
                        .submit(
                            chat,
                            if block {
                                "Do not perform this operation."
                            } else {
                                "Run the local command."
                            }
                            .into(),
                        )
                        .await
                        .unwrap();
                    let mut assessment = None;
                    let mut asked = false;
                    let mut result = None;
                    loop {
                        let event = next_event(&mut events).await;
                        if event.run_id != receipt.run_id {
                            continue;
                        }
                        match event.kind {
                            EventKind::ApprovalAssessed { assessment: value } => {
                                assessment = Some(value)
                            }
                            EventKind::PermissionRequested { request } => {
                                asked = true;
                                agent
                                    .answer_permission(chat, request.id, !block)
                                    .await
                                    .unwrap();
                            }
                            EventKind::ToolCompleted { result: value, .. } => result = Some(value),
                            EventKind::Completed { .. } | EventKind::Interrupted => break,
                            EventKind::Failed(error) => panic!("{error}"),
                            _ => {}
                        }
                    }
                    agent.shutdown().await;
                    let reviewed = required && mode == ExecutionMode::Auto && explicit.is_none();
                    let expected_ask = explicit == Some(Decision::Ask)
                        || required && mode == ExecutionMode::AcceptEdits && explicit.is_none();
                    assert_eq!(
                        assessment.is_some(),
                        reviewed,
                        "required={required}, mode={mode:?}, explicit={explicit:?}"
                    );
                    assert_eq!(asked, expected_ask);
                    if let Some(assessment) = assessment {
                        assert_eq!(
                            assessment.outcome,
                            if block {
                                ApprovalOutcome::Block
                            } else {
                                ApprovalOutcome::Allow
                            }
                        );
                    }
                    let rejected =
                        explicit == Some(Decision::Deny) || (reviewed || expected_ask) && block;
                    let result = result.expect("Bash returns a result");
                    assert_eq!(result.is_error, rejected, "{result:?}");
                    if !rejected {
                        assert!(result.content.contains("portable-result"), "{result:?}");
                    }
                    assert_eq!(reviewer.requests.lock().unwrap().is_empty(), !reviewed);
                }
            }
        }
    }
}
