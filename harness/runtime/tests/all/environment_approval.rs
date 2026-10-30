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

/// In Auto, an ordinary TickManage open still goes to the reviewer and runs once allowed; the
/// placement and move forms earlier builds offered for Herdr Panes are refused before any review
/// or dispatch, so nothing they named is admitted.
#[tokio::test]
async fn auto_reviews_ordinary_tick_opens_and_refuses_retired_view_forms_before_review() {
    use tickloop_harness_protocol::{HostEndpointIdentity, HostToolAuthority};
    use tickloop_harness_runtime::environment::{ExecutionTools, memory::counter_environment};

    let open = json!({"action":"open","command":["/counter"],"cwd":"/","protocol":true});
    let mut placed = open.clone();
    placed["placement"] = json!({"type":"current_tab","direction":"right"});
    let moved =
        json!({"action":"move","tick_instance_id":"hosted:0190d0c4-6b7a-7c3e-9f00-0123456789ab"});
    for (input, admitted) in [(open, true), (placed, false), (moved, false)] {
        let folder = tempfile::tempdir().unwrap();
        let environment = Arc::new(counter_environment());
        let model = Arc::new(ScriptedModel::new(vec![
            call("tick", "TickManage", input.clone()),
            text("done"),
        ]));
        let reviewer = Arc::new(ScriptedModel::new(vec![text(
            &auto_classification(false).to_string(),
        )]));
        let agent = Agent::with_seams(
            HarnessSeams {
                search: None,
                transcripts: None,
                environment: Some(environment.clone()),
                models: ChosenModels::new(model),
            },
            HarnessOptions::local(config()),
        )
        .unwrap();
        agent
            .install_host_tools(Arc::new(ExecutionTools::new(
                environment.clone(),
                HostToolAuthority::Native {
                    endpoint: HostEndpointIdentity::TrustedAdapter {
                        scope: tickloop_ids::generate(),
                    },
                },
            )))
            .unwrap();
        agent.set_auto_reviewer(reviewer.clone()).await;
        let chat = agent.create_chat(folder.path().into()).await.unwrap();
        agent
            .set_execution_mode(chat, ExecutionMode::Auto)
            .await
            .unwrap();
        let mut events = agent.subscribe();
        let receipt = agent
            .submit(chat, "Open the counter beside you.".into())
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
                EventKind::ApprovalAssessed { assessment: value } => assessment = Some(value),
                EventKind::PermissionRequested { .. } => asked = true,
                EventKind::ToolCompleted { result: value, .. } => result = Some(value),
                EventKind::Completed { .. } | EventKind::Interrupted => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        agent.shutdown().await;
        let result = result.expect("TickManage returns a result");
        assert!(!asked, "{input}");
        assert_eq!(result.is_error, !admitted, "{input}: {}", result.content);
        if admitted {
            assert_eq!(
                assessment.map(|assessment| assessment.outcome),
                Some(ApprovalOutcome::Allow),
                "{input}"
            );
            assert!(result.content.contains("opened"), "{}", result.content);
        } else {
            assert!(assessment.is_none(), "{input}");
            assert!(reviewer.requests.lock().unwrap().is_empty(), "{input}");
            assert!(
                result.content.contains("invalid TickManage input"),
                "{input}: {}",
                result.content
            );
        }
    }
}

/// Models fill every field of the flat TickManage schema and leave TickCall's `args` out; the
/// fields other actions take are dropped and missing `args` are no arguments, so both calls run.
#[tokio::test]
async fn tick_calls_run_with_other_actions_fields_filled_and_args_left_out() {
    use tickloop_harness_protocol::{Content, HostEndpointIdentity, HostToolAuthority};
    use tickloop_harness_runtime::environment::{ExecutionTools, memory::counter_environment};
    use tickloop_harness_test_support::routed::{RoutedModel, has_tool_result, tool_result};

    let folder = tempfile::tempdir().unwrap();
    let environment = Arc::new(counter_environment());
    let model = Arc::new(RoutedModel::new(|request| {
        let Some(opened) = tool_result(&request.messages, "open") else {
            return call(
                "open",
                "TickManage",
                json!({"action":"open","command":["/counter"],"cwd":"","protocol":true,
                    "tick_instance_id":"","text":"","keys":[],"frequency":"","subscription":""}),
            );
        };
        if !has_tool_result(request, "increment") {
            return call(
                "increment",
                "TickCall",
                json!({"tick_instance_id":opened["tick_instance_id"],"tool":"increment"}),
            );
        }
        text("done")
    }));
    let agent = Agent::with_seams(
        HarnessSeams {
            search: None,
            transcripts: None,
            environment: Some(environment.clone()),
            models: ChosenModels::new(model),
        },
        HarnessOptions::local(config()),
    )
    .unwrap();
    agent
        .install_host_tools(Arc::new(ExecutionTools::new(
            environment.clone(),
            HostToolAuthority::Native {
                endpoint: HostEndpointIdentity::TrustedAdapter {
                    scope: tickloop_ids::generate(),
                },
            },
        )))
        .unwrap();
    let chat = agent.create_chat(folder.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, "Count once.".into()).await.unwrap();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != receipt.run_id {
            continue;
        }
        match event.kind {
            EventKind::Completed { .. } | EventKind::Interrupted => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    let snapshot = agent.snapshot(chat).await.unwrap();
    agent.shutdown().await;
    let result = |id: &str| {
        snapshot
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .find_map(|content| match content {
                Content::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } if tool_use_id == id => Some((content.text(), *is_error)),
                _ => None,
            })
            .unwrap()
    };
    for id in ["open", "increment"] {
        let (content, is_error) = result(id);
        assert!(!is_error, "{id}: {content}");
    }
    assert!(
        result("increment").0.contains("\"value\":1"),
        "{}",
        result("increment").0
    );
}
