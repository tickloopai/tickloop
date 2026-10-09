use std::collections::BTreeMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use async_trait::async_trait;
use serde_json::{Value, json};
use tick_protocol::InstanceRef;
use tickhost_connect::{
    access::TickAccess,
    herdr::{HerdrContext, HerdrPort, Pane, process_birth},
    host_error::HostError,
    session::{SessionLaunch, TickSession},
};
use tickloop_agent_daemon::EmbeddedAgentAccess;
use tickloop_agent_model::judgment::{
    Answer, Evaluation, EvaluationRequest, JudgmentClient, JudgmentError, Question,
};
use tickloop_agent_runtime::{
    Agent,
    host_tools::{HostCallResult, HostToolPermission, HostToolProvider},
    native_host::NativeHostToolProvider,
};
use tokio_util::sync::CancellationToken;

use tickloop_agent_protocol::{
    AgentAccess, AgentCommand, ApprovalPath, CommandEnvelope, CommandResult, ConfigurationChange,
    Content, Decision, EventKind, ExecutionMode, HerdrInteraction, HostCallContext,
    HostWorkContext, LaunchSpec, LoopState, PermissionRule, Usage, UserInput,
};
use tickloop_agent_test_support::{ScriptedModel, call, config, text};

struct ControlledHerdr {
    replaced: AtomicBool,
    changed_directory: AtomicBool,
    opened: AtomicBool,
    sent: Mutex<Vec<(String, Value)>>,
    protocol_instance: Mutex<Option<InstanceRef>>,
}

struct ViewlessHerdr(ControlledHerdr);

#[async_trait]
impl HerdrPort for ViewlessHerdr {
    async fn request(
        &self,
        method: &str,
        params: Value,
        cancel: CancellationToken,
    ) -> Result<Value, HostError> {
        if method == "pane.current" {
            return Err(HostError::before("TargetGone", "the Agent View was closed"));
        }
        self.0.request(method, params, cancel).await
    }
}

#[tokio::test]
async fn explicit_host_operations_continue_after_the_agent_view_closes() {
    let directory = tempfile::tempdir().unwrap();
    let host = Arc::new(ViewlessHerdr(ControlledHerdr {
        replaced: AtomicBool::new(false),
        changed_directory: AtomicBool::new(false),
        opened: AtomicBool::new(false),
        sent: Mutex::new(Vec::new()),
        protocol_instance: Mutex::new(None),
    }));
    let model = Arc::new(ScriptedModel::new(vec![
        call("discover", "ReadWorkspace", json!({})),
        call(
            "read",
            "ReadTick",
            json!({"workspace_id":"workspace-a","tick_instance_id":target_instance()}),
        ),
        call(
            "open",
            "ModifyTick",
            json!({"action":"open","command":["/bin/echo","fixture"],"placement":{"type":"new_tab"}}),
        ),
        call(
            "delegate-host",
            "Subagent",
            json!({"task":"Read the inherited host Workspace without expanding it."}),
        ),
        call("child-discover", "ReadWorkspace", json!({})),
        call(
            "child-input",
            "ModifyTick",
            json!({"action":"send_input","tick_instance_id":target_instance(),"text":"arbitrary input"}),
        ),
        call(
            "child-cross",
            "ReadWorkspace",
            json!({"workspace_id":"workspace-b"}),
        ),
        text("The child read only the inherited host scope."),
        text("The background operations completed."),
    ]));
    let agent = Agent::new_embedded(config(), model);
    agent
        .install_host_tools(Arc::new(
            NativeHostToolProvider::with_trusted_adapter_identity(
                Arc::new(TickAccess::with_host(
                    HerdrContext::without_view("/tmp/controlled-herdr.sock".into()),
                    host.clone(),
                )),
                uuid::Uuid::new_v4(),
            ),
        ))
        .unwrap();
    let access = EmbeddedAgentAccess::new(agent.clone());
    let mut launch = LaunchSpec::embedded(directory.path().into());
    launch.host_work_context = Some(HostWorkContext {
        socket: "/tmp/controlled-herdr.sock".into(),
        workspace: Some("workspace-a".into()),
    });
    let created = access
        .execute(CommandEnvelope {
            daemon_epoch: access.daemon_epoch(),
            command_id: uuid::Uuid::new_v4(),
            command: AgentCommand::Create {
                launch,
                initial: None,
                origin: None,
            },
        })
        .await
        .unwrap();
    let Some(CommandResult::Created {
        execution,
        snapshot: created_snapshot,
        ..
    }) = created.result
    else {
        panic!("Create failed: {created:?}");
    };
    let chat = created_snapshot.conversation.id;
    let configured = access
        .execute(CommandEnvelope {
            daemon_epoch: access.daemon_epoch(),
            command_id: uuid::Uuid::new_v4(),
            command: AgentCommand::Configure {
                provider: None,
                execution,
                change: ConfigurationChange::ExecutionMode {
                    mode: ExecutionMode::FullAccess,
                },
            },
        })
        .await
        .unwrap();
    assert!(configured.error.is_none(), "{configured:?}");
    let mut events = agent.subscribe();
    let submitted = access
        .execute(CommandEnvelope {
            daemon_epoch: access.daemon_epoch(),
            command_id: uuid::Uuid::new_v4(),
            command: AgentCommand::Submit {
                execution,
                target_loop: None,
                input: UserInput::Text {
                    text: "Discover, read and open the explicit targets.".into(),
                    contexts: vec![],
                    paths: vec![],
                },
                origin: None,
            },
        })
        .await
        .unwrap();
    assert!(submitted.error.is_none(), "{submitted:?}");
    let Some(CommandResult::Submitted(parent)) = submitted.result else {
        panic!("Submit failed: {submitted:?}");
    };
    let mut results = BTreeMap::new();
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        match event.kind {
            EventKind::ToolCompleted { call_id, result } => {
                results.insert(call_id, result);
            }
            EventKind::Completed { .. } if event.run_id == parent.run_id => break,
            EventKind::Failed(error) => panic!("host task failed: {error}"),
            _ => {}
        }
    }
    let membership = agent.snapshot(chat).await.unwrap().monitor;
    let subscriptions = agent.tick_subscriptions(chat).await.unwrap();
    agent.shutdown().await;
    for name in [
        "discover",
        "read",
        "open",
        "child-discover",
        "delegate-host",
    ] {
        let result = &results[name];
        assert!(!result.is_error, "{name}: {}", result.content);
    }
    let opened: Value = serde_json::from_str(&results["open"].content).unwrap();
    let discovered: Value = serde_json::from_str(&results["discover"].content).unwrap();
    assert!(discovered["caller"].is_null());
    let child_discovered: Value = serde_json::from_str(&results["child-discover"].content).unwrap();
    assert!(child_discovered["caller"].is_null());
    assert_eq!(child_discovered["workspace_id"], "workspace-a");
    assert!(results["child-input"].is_error);
    assert!(results["child-input"].content.contains("Unsupported"));
    assert!(results["child-cross"].is_error);
    assert_eq!(opened["status"], "opened");
    assert_eq!(opened["pane_id"], "opened");
    assert_eq!(opened["tab_id"], "tab");
    assert_eq!(opened["workspace_id"], "workspace-a");
    assert!(opened["tick_instance_id"].is_string());
    assert_eq!(
        membership
            .members
            .iter()
            .map(|member| (member.instance.as_str(), member.source))
            .collect::<Vec<_>>(),
        vec![(
            opened["tick_instance_id"].as_str().unwrap(),
            tickloop_agent_protocol::MonitorMemberSource::Opened
        )],
        "a verified open joins the opening Chat's Monitor"
    );
    assert!(subscriptions.is_empty(), "opening never subscribes");
    assert_eq!(host.0.sent.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn host_context_uses_the_captured_anchor_and_keeps_background_discovery_available() {
    let directory = tempfile::tempdir().unwrap();
    let socket = std::path::PathBuf::from("/tmp/context-herdr.sock");
    let host = Arc::new(ViewlessHerdr(ControlledHerdr {
        replaced: AtomicBool::new(false),
        changed_directory: AtomicBool::new(false),
        opened: AtomicBool::new(false),
        sent: Mutex::new(Vec::new()),
        protocol_instance: Mutex::new(None),
    }));
    let provider = NativeHostToolProvider::new(Arc::new(TickAccess::with_host(
        HerdrContext::without_view(socket.clone()),
        host.clone(),
    )));
    let chat = uuid::Uuid::new_v4();
    let mut context = HostCallContext {
        workdir: directory.path().into(),
        workspace_id: None,
        interaction: None,
    };
    let discovery = provider
        .prepare_call_in_context(
            chat,
            &context,
            "ReadWorkspace",
            &json!({}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let HostCallResult::Completed(discovery) = provider
        .execute_prepared(chat, discovery, CancellationToken::new())
        .await
    else {
        panic!("background discovery must complete")
    };
    let inventory: Value = serde_json::from_str(&discovery.content).unwrap();
    assert!(inventory["caller"].is_null());
    assert_eq!(
        inventory["workspaces"],
        json!(["workspace-a", "workspace-b"])
    );
    context.workspace_id = Some("workspace-a".into());
    let input = json!({"action":"open","command":["/bin/echo","fixture"],"placement":{"type":"current_tab","direction":"right"}});
    let error = provider
        .prepare_call_in_context(
            chat,
            &context,
            "ModifyTick",
            &input,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(error.contains("MissingAnchor"));
    context.interaction = Some(HerdrInteraction {
        socket,
        pane: serde_json::from_value(json!(host.0.pane("target", "workspace-a"))).unwrap(),
    });
    let prepared = provider
        .prepare_call_in_context(
            chat,
            &context,
            "ModifyTick",
            &input,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(prepared.input()["resolved_placement"]["pane_id"], "target");
    assert_eq!(
        prepared.input()["cwd"],
        json!(directory.path().canonicalize().unwrap())
    );
    host.0.replaced.store(true, Ordering::SeqCst);
    let result = provider
        .execute_prepared(chat, prepared, CancellationToken::new())
        .await;
    assert!(
        matches!(result, HostCallResult::NotDispatched(message) if message.contains("TargetChanged"))
    );
    assert!(host.0.sent.lock().unwrap().is_empty());
}

#[test]
fn native_tool_schemas_declare_object_roots_for_model_apis() {
    let provider = NativeHostToolProvider::new(Arc::new(TickAccess::with_host(
        HerdrContext {
            socket: "/tmp/controlled-herdr.sock".into(),
            caller_pane: "agent".into(),
        },
        Arc::new(ControlledHerdr {
            replaced: AtomicBool::new(false),
            changed_directory: AtomicBool::new(false),
            opened: AtomicBool::new(false),
            sent: Mutex::new(Vec::new()),
            protocol_instance: Mutex::new(None),
        }),
    )));

    for definition in provider.definitions() {
        assert_eq!(
            definition.input_schema["type"], "object",
            "{} must declare object parameters at the schema root",
            definition.name
        );
    }
}

#[derive(Clone, Copy)]
enum LaunchFailure {
    BeforeCreation,
    ResponseLost,
    CancelledAfterCreation,
    MissingPaneIdentity,
}

struct FaultingLaunchHerdr {
    inner: ControlledHerdr,
    failure: LaunchFailure,
}

#[async_trait]
impl HerdrPort for FaultingLaunchHerdr {
    async fn request(
        &self,
        method: &str,
        params: Value,
        cancel: CancellationToken,
    ) -> Result<Value, HostError> {
        if method != "layout.apply" {
            return self.inner.request(method, params, cancel).await;
        }
        if matches!(self.failure, LaunchFailure::BeforeCreation) {
            return Err(HostError::before("Rejected", "creation rejected"));
        }
        let result = self.inner.request(method, params, cancel.clone()).await?;
        match self.failure {
            LaunchFailure::ResponseLost => Err(HostError {
                code: "OutcomeUnknown".into(),
                message: "creation response lost".into(),
                dispatched: true,
            }),
            LaunchFailure::CancelledAfterCreation => {
                cancel.cancel();
                Ok(result)
            }
            LaunchFailure::MissingPaneIdentity => Ok(json!({"layout":{}})),
            LaunchFailure::BeforeCreation => unreachable!(),
        }
    }
}

#[tokio::test]
async fn opening_failure_reports_known_creation_without_repeating_or_rolling_back() {
    let directory = tempfile::tempdir().unwrap();
    for failure in [
        LaunchFailure::BeforeCreation,
        LaunchFailure::ResponseLost,
        LaunchFailure::CancelledAfterCreation,
        LaunchFailure::MissingPaneIdentity,
    ] {
        let host = Arc::new(FaultingLaunchHerdr {
            inner: ControlledHerdr {
                replaced: AtomicBool::new(false),
                changed_directory: AtomicBool::new(false),
                opened: AtomicBool::new(false),
                sent: Mutex::new(Vec::new()),
                protocol_instance: Mutex::new(None),
            },
            failure,
        });
        let provider = NativeHostToolProvider::new(Arc::new(TickAccess::with_host(
            HerdrContext {
                socket: "/tmp/controlled-herdr.sock".into(),
                caller_pane: "agent".into(),
            },
            host.clone(),
        )));
        let chat = uuid::Uuid::new_v4();
        let cancel = CancellationToken::new();
        let call = provider
            .prepare_call(
                chat,
                "ModifyTick",
                &json!({"action":"open","command":["/bin/echo","fixture"],"cwd":directory.path(),"placement":{"type":"new_tab"}}),
                cancel.clone(),
            )
            .await
            .unwrap();
        assert_eq!(call.permission(), HostToolPermission::Reviewable);
        let result = provider.execute_prepared(chat, call, cancel).await;
        match failure {
            LaunchFailure::BeforeCreation => {
                assert!(matches!(result, HostCallResult::NotDispatched(_)));
                assert!(!host.inner.opened.load(Ordering::SeqCst));
                assert!(host.inner.sent.lock().unwrap().is_empty());
                continue;
            }
            LaunchFailure::ResponseLost | LaunchFailure::MissingPaneIdentity => {
                assert!(matches!(result, HostCallResult::OutcomeUnknown(_)));
            }
            LaunchFailure::CancelledAfterCreation => {
                let HostCallResult::Completed(result) = result else {
                    panic!("known created Pane must be reported");
                };
                let content: Value = serde_json::from_str(&result.content).unwrap();
                assert_eq!(content["status"], "initializing");
                assert_eq!(content["pane_id"], "opened");
                assert_eq!(content["workspace_id"], "workspace-a");
                assert_eq!(content["stage"], "discover_instance");
                assert!(
                    content["diagnostic"]
                        .as_str()
                        .unwrap()
                        .contains("Do not create")
                );
            }
        }
        assert!(host.inner.opened.load(Ordering::SeqCst));
        let sent = host.inner.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, "layout.apply");
    }
}

impl ControlledHerdr {
    fn pane(&self, pane_id: &str, workspace_id: &str) -> Pane {
        Pane {
            pane_id: pane_id.into(),
            terminal_id: if self.replaced.load(Ordering::SeqCst) && pane_id == "target" {
                "replacement".into()
            } else {
                pane_id.into()
            },
            workspace_id: workspace_id.into(),
            tab_id: self
                .sent
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find(|(name, params)| name == "pane.move" && params["pane_id"] == pane_id)
                .map(|(_, params)| {
                    if params["destination"]["type"] == "new_tab" {
                        "temporary-tab".into()
                    } else {
                        params["destination"]["tab_id"].as_str().unwrap().to_owned()
                    }
                })
                .unwrap_or_else(|| "tab".into()),
            cwd: Some(if self.changed_directory.load(Ordering::SeqCst) {
                "/tmp/other".into()
            } else {
                "/tmp/project".into()
            }),
            foreground_cwd: Some(if self.changed_directory.load(Ordering::SeqCst) {
                "/tmp/other".into()
            } else {
                "/tmp/project".into()
            }),
            terminal_title_stripped: Some(pane_id.into()),
            tokens: if pane_id == "protocol" {
                self.protocol_instance
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(|instance| [("tickloop_instance".into(), instance.to_string())].into())
                    .unwrap_or_default()
            } else {
                Default::default()
            },
        }
    }
}

#[async_trait]
impl HerdrPort for ControlledHerdr {
    async fn request(
        &self,
        method: &str,
        params: Value,
        _: CancellationToken,
    ) -> Result<Value, HostError> {
        match method {
            "pane.current" => Ok(json!({"pane":self.pane("agent", "workspace-a")})),
            "pane.list" => {
                let mut panes = vec![
                    self.pane("target", "workspace-a"),
                    self.pane("other", "workspace-b"),
                ];
                if self.protocol_instance.lock().unwrap().is_some() {
                    panes.push(self.pane("protocol", "workspace-a"));
                }
                if self.opened.load(Ordering::SeqCst) {
                    panes.push(self.pane("opened", "workspace-a"));
                }
                let workspace = params["workspace_id"].as_str();
                Ok(
                    json!({"panes":panes.into_iter().filter(|pane| workspace.is_none_or(|workspace| pane.workspace_id == workspace)).collect::<Vec<_>>()}),
                )
            }
            "pane.process_info" => Ok(
                json!({"foreground_process_group_id":1,"foreground_processes":[{"pid":std::process::id()}]}),
            ),
            "pane.read" => Ok(
                json!({"source":"recent_unwrapped","format":"text","text":"latest line","truncated":false}),
            ),
            "pane.send_input" | "pane.close" => {
                self.sent.lock().unwrap().push((method.into(), params));
                Ok(json!({"accepted":true}))
            }
            "pane.neighbor" => {
                let sent = self.sent.lock().unwrap();
                let source = sent
                    .iter()
                    .rev()
                    .find(|(method, input)| {
                        method == "pane.move"
                            && input["destination"]["target_pane_id"] == params["pane_id"]
                            && input["destination"]["split"] == params["direction"]
                    })
                    .map(|(_, input)| input["pane_id"].clone());
                Ok(
                    json!({"neighbor":{"pane_id":params["pane_id"],"direction":params["direction"],"neighbor_pane_id":source}}),
                )
            }
            "pane.move" => {
                let pane_id = params["pane_id"].as_str().unwrap().to_owned();
                self.sent.lock().unwrap().push((method.into(), params));
                let pane = self.pane(&pane_id, "workspace-a");
                Ok(json!({"move_result":{"changed":true,"pane":pane}}))
            }
            "layout.apply" => {
                self.sent.lock().unwrap().push((method.into(), params));
                self.opened.store(true, Ordering::SeqCst);
                Ok(json!({"layout":{"root":{"pane_id":"opened"}}}))
            }
            "pane.get" => {
                let pane_id = params["pane_id"].as_str().unwrap();
                let workspace = if pane_id == "other" {
                    "workspace-b"
                } else {
                    "workspace-a"
                };
                Ok(json!({"pane":self.pane(pane_id, workspace)}))
            }
            _ => Err(HostError::before("UnknownMethod", method)),
        }
    }
}

fn target_instance() -> InstanceRef {
    let birth = process_birth(std::process::id()).unwrap();
    InstanceRef::native(format!("terminal:target:1:{birth}")).unwrap()
}

struct ControlledJudgment {
    block: bool,
}

#[async_trait]
impl JudgmentClient for ControlledJudgment {
    fn model(&self) -> &str {
        "native-test-jev"
    }

    async fn evaluate(
        &self,
        request: &EvaluationRequest,
        _: CancellationToken,
    ) -> Result<Evaluation, JudgmentError> {
        let answers = request
            .questions
            .iter()
            .map(|(id, question)| {
                let Question::Choice { criteria, .. } = question else {
                    panic!("expected typed approval choices")
                };
                let selected = if self.block { "block_soft" } else { "allow" };
                (
                    id.clone(),
                    Answer::Choice {
                        choice: selected.into(),
                        confidence: 1.0,
                        probabilities: criteria
                            .keys()
                            .map(|option| (option.clone(), f64::from(option == selected)))
                            .collect::<BTreeMap<_, _>>(),
                    },
                )
            })
            .collect();
        Ok(Evaluation {
            model: self.model().into(),
            answers,
            usage: Usage::default(),
        })
    }
}

#[tokio::test]
async fn native_mode_rules_and_human_recovery_apply_to_new_messages_for_both_reviewers() {
    for use_jev in [false, true] {
        let host = Arc::new(ControlledHerdr {
            replaced: AtomicBool::new(false),
            changed_directory: AtomicBool::new(false),
            opened: AtomicBool::new(false),
            sent: Mutex::new(Vec::new()),
            protocol_instance: Mutex::new(None),
        });
        let mut outputs = Vec::new();
        for step in 0..12 {
            outputs.push(call(&format!("input-{step}"), "ModifyTick", json!({"action":"send_input","tick_instance_id":target_instance(),"text":format!("input-{step}")})));
            outputs.push(text("Step completed."));
        }
        let agent = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(outputs)));
        agent
            .install_host_tools(Arc::new(NativeHostToolProvider::new(Arc::new(
                TickAccess::with_host(
                    HerdrContext {
                        socket: "/tmp/controlled-herdr.sock".into(),
                        caller_pane: "agent".into(),
                    },
                    host.clone(),
                ),
            ))))
            .unwrap();
        if use_jev {
            agent
                .set_judgment_client(Arc::new(ControlledJudgment { block: true }))
                .await;
        } else {
            agent
                .set_auto_reviewer(Arc::new(ScriptedModel::new(vec![
                    text(
                        &tickloop_agent_test_support::auto_classification(true).to_string()
                    );
                    14
                ])))
                .await;
        }
        let directory = tempfile::tempdir().unwrap();
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let mut events = agent.subscribe();
        let cases: [(ExecutionMode, &[Decision], bool, usize); 12] = [
            (ExecutionMode::Auto, &[], false, 0),
            (ExecutionMode::Auto, &[], false, 0),
            (ExecutionMode::FullAccess, &[], false, 1),
            (ExecutionMode::Auto, &[], false, 1),
            (ExecutionMode::Auto, &[Decision::Allow], false, 2),
            (
                ExecutionMode::FullAccess,
                &[Decision::Allow, Decision::Ask],
                true,
                3,
            ),
            (
                ExecutionMode::FullAccess,
                &[Decision::Allow, Decision::Ask, Decision::Deny],
                false,
                3,
            ),
            (ExecutionMode::AcceptEdits, &[], true, 4),
            (ExecutionMode::Auto, &[], false, 4),
            (ExecutionMode::Auto, &[], false, 4),
            (ExecutionMode::Auto, &[], true, 5),
            (ExecutionMode::Auto, &[], false, 5),
        ];
        for (step, (mode, decisions, should_prompt, expected_dispatches)) in
            cases.into_iter().enumerate()
        {
            agent.set_execution_mode(chat, mode).await.unwrap();
            agent
                .set_permission_rules(
                    chat,
                    decisions
                        .iter()
                        .map(|decision| PermissionRule {
                            id: uuid::Uuid::new_v4(),
                            decision: *decision,
                            tool: "ModifyTick".into(),
                            specifier: None,
                            input: None,
                            loop_id: None,
                            persistent: false,
                        })
                        .collect(),
                )
                .await
                .unwrap();
            agent
                .submit(chat, None, format!("Send input for step {step}."))
                .await
                .unwrap();
            let mut prompted = false;
            loop {
                match tickloop_agent_test_support::next_event(&mut events)
                    .await
                    .kind
                {
                    EventKind::PermissionRequested { request } => {
                        assert!(
                            should_prompt,
                            "unexpected human request at step {step}: {:?}",
                            request.approval_reason
                        );
                        assert!(!prompted);
                        prompted = true;
                        agent
                            .answer_permission(chat, request.id, true)
                            .await
                            .unwrap();
                    }
                    EventKind::Completed { .. } => break,
                    EventKind::Failed(error) => panic!("native step {step} failed: {error}"),
                    _ => {}
                }
            }
            assert_eq!(prompted, should_prompt);
            assert_eq!(
                host.sent.lock().unwrap().len(),
                expected_dispatches,
                "step {step}"
            );
        }
        let history = agent.approval_history_view(chat).await.unwrap();
        let auto_path = if use_jev {
            ApprovalPath::AutoJev
        } else {
            ApprovalPath::AutoLlm
        };
        assert_eq!(
            history
                .records
                .iter()
                .filter(|record| record.path == auto_path)
                .count(),
            6
        );
        assert_eq!(history.records.len(), 12);
        agent.shutdown().await;
    }
}

async fn assert_native_mode_admits_without_human(mode: ExecutionMode, use_jev: bool) {
    let host = Arc::new(ControlledHerdr {
        replaced: AtomicBool::new(false),
        changed_directory: AtomicBool::new(false),
        opened: AtomicBool::new(false),
        sent: Mutex::new(Vec::new()),
        protocol_instance: Mutex::new(None),
    });
    let access = Arc::new(TickAccess::with_host(
        HerdrContext {
            socket: "/tmp/controlled-herdr.sock".into(),
            caller_pane: "agent".into(),
        },
        host.clone(),
    ));
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "close",
            "ModifyTick",
            json!({"action":"close","tick_instance_id":target_instance()}),
        ),
        text("Closed."),
    ]));
    let reviewer = Arc::new(ScriptedModel::new(vec![text(
        &tickloop_agent_test_support::auto_classification(false).to_string(),
    )]));
    let agent = Agent::new_embedded(config(), model);
    agent
        .install_host_tools(Arc::new(NativeHostToolProvider::new(access)))
        .unwrap();
    if use_jev {
        agent
            .set_judgment_client(Arc::new(ControlledJudgment { block: false }))
            .await;
    } else {
        agent.set_auto_reviewer(reviewer.clone()).await;
    }
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent.set_execution_mode(chat, mode).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Close the selected Tick Pane.".into())
        .await
        .unwrap();
    loop {
        match tickloop_agent_test_support::next_event(&mut events)
            .await
            .kind
        {
            EventKind::PermissionRequested { .. } => {
                panic!("{mode:?} requested a human for an ordinary Tick modification")
            }
            EventKind::ToolStarted { .. } => break,
            _ => {}
        }
    }
    wait_for_completion(&agent, chat).await;
    assert_eq!(host.sent.lock().unwrap().len(), 1);
    assert_eq!(
        reviewer.requests.lock().unwrap().len(),
        usize::from(mode == ExecutionMode::Auto && !use_jev)
    );
    let history = agent.approval_history_view(chat).await.unwrap();
    assert_eq!(history.records.len(), 1);
    assert_eq!(
        history.records[0].path,
        match (mode, use_jev) {
            (ExecutionMode::Auto, true) => ApprovalPath::AutoJev,
            (ExecutionMode::Auto, false) => ApprovalPath::AutoLlm,
            _ => ApprovalPath::Direct,
        }
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn full_access_native_modification_does_not_request_a_human() {
    assert_native_mode_admits_without_human(ExecutionMode::FullAccess, false).await;
}

#[tokio::test]
async fn auto_native_modification_uses_reviewer_instead_of_forced_human_confirmation() {
    for use_jev in [false, true] {
        assert_native_mode_admits_without_human(ExecutionMode::Auto, use_jev).await;
    }
}

async fn wait_for_completion(agent: &Agent, chat: uuid::Uuid) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if agent
                .snapshot(chat)
                .await
                .unwrap()
                .loops
                .last()
                .is_some_and(|loop_| loop_.state == LoopState::Completed)
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn agent_reads_native_workspace_and_terminal_content_without_a_subscription() {
    let host = Arc::new(ControlledHerdr {
        replaced: AtomicBool::new(false),
        changed_directory: AtomicBool::new(false),
        opened: AtomicBool::new(false),
        sent: Mutex::new(Vec::new()),
        protocol_instance: Mutex::new(None),
    });
    let access = Arc::new(TickAccess::with_host(
        HerdrContext {
            socket: "/tmp/controlled-herdr.sock".into(),
            caller_pane: "agent".into(),
        },
        host,
    ));
    let model = Arc::new(ScriptedModel::new(vec![
        call("workspace", "ReadWorkspace", json!({})),
        call(
            "tick",
            "ReadTick",
            json!({"tick_instance_id":target_instance()}),
        ),
        call(
            "other-workspace",
            "ReadWorkspace",
            json!({"workspace_id":"workspace-b"}),
        ),
        text("The target contains latest line."),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent
        .install_host_tools(Arc::new(NativeHostToolProvider::new(access)))
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .submit(chat, None, "Read the current program".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;

    let messages = serde_json::to_string(&model.requests.lock().unwrap()[2].messages).unwrap();
    assert!(messages.contains("workspace-a"));
    assert!(messages.contains("latest line"));
    assert!(!messages.contains("workspace-b"));
    let cross_workspace =
        serde_json::to_string(&model.requests.lock().unwrap()[3].messages).unwrap();
    assert!(cross_workspace.contains("workspace-b"));
    agent.shutdown().await;
}

#[tokio::test]
async fn forged_protocol_metadata_does_not_advertise_live_tick_tools() {
    let forged_instance = InstanceRef::native(uuid::Uuid::new_v4().to_string()).unwrap();
    let host = Arc::new(ControlledHerdr {
        replaced: AtomicBool::new(false),
        changed_directory: AtomicBool::new(false),
        opened: AtomicBool::new(false),
        sent: Mutex::new(Vec::new()),
        protocol_instance: Mutex::new(Some(forged_instance.clone())),
    });
    let access = Arc::new(TickAccess::with_host(
        HerdrContext {
            socket: "/tmp/controlled-herdr.sock".into(),
            caller_pane: "agent".into(),
        },
        host,
    ));
    let model = Arc::new(ScriptedModel::new(vec![
        call("workspace", "ReadWorkspace", json!({})),
        text("The advertised protocol instance is disconnected."),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent
        .install_host_tools(Arc::new(NativeHostToolProvider::new(access)))
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .submit(chat, None, "List the Tick instances".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;

    let workspace: Value = {
        let requests = model.requests.lock().unwrap();
        let result = requests[1]
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .find_map(|content| match content {
                Content::ToolResult {
                    tool_use_id,
                    content,
                    is_error: false,
                } if tool_use_id == "workspace" => Some(content),
                _ => None,
            })
            .expect("read_workspace must return the disconnected instance");
        serde_json::from_str(&result.text()).unwrap()
    };
    let tick = workspace["ticks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tick| tick["tick_instance_id"] == forged_instance.to_string())
        .expect("the forged instance must remain visible as disconnected");
    assert_eq!(tick["state"], "disconnected");
    assert_eq!(tick["capabilities"], json!([]));
    agent.shutdown().await;
}

#[tokio::test]
async fn replacing_an_approved_terminal_prevents_input_to_the_replacement() {
    let host = Arc::new(ControlledHerdr {
        replaced: AtomicBool::new(false),
        changed_directory: AtomicBool::new(false),
        opened: AtomicBool::new(false),
        sent: Mutex::new(Vec::new()),
        protocol_instance: Mutex::new(None),
    });
    let access = Arc::new(TickAccess::with_host(
        HerdrContext {
            socket: "/tmp/controlled-herdr.sock".into(),
            caller_pane: "agent".into(),
        },
        host.clone(),
    ));
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "input",
            "ModifyTick",
            json!({"tick_instance_id":target_instance(),"action":"send_input","text":"hello; $HOME\n"}),
        ),
        text("The input was refused."),
    ]));
    let agent = Agent::new_embedded(config(), model);
    agent
        .install_host_tools(Arc::new(NativeHostToolProvider::new(access)))
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Send the exact text".into())
        .await
        .unwrap();
    let request = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let EventKind::PermissionRequested { request } = events.recv().await.unwrap().kind {
                break request;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(request.input["workspace_id"], "workspace-a");
    assert_eq!(request.input["pane_id"], "target");
    assert_eq!(request.input["text"], "hello; $HOME\n");
    host.replaced.store(true, Ordering::SeqCst);
    agent
        .answer_permission(chat, request.id, true)
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    assert!(host.sent.lock().unwrap().is_empty());
    agent.shutdown().await;
}

#[tokio::test]
async fn changing_the_target_directory_invalidates_an_approved_input() {
    let host = Arc::new(ControlledHerdr {
        replaced: AtomicBool::new(false),
        changed_directory: AtomicBool::new(false),
        opened: AtomicBool::new(false),
        sent: Mutex::new(Vec::new()),
        protocol_instance: Mutex::new(None),
    });
    let access = Arc::new(TickAccess::with_host(
        HerdrContext {
            socket: "/tmp/controlled-herdr.sock".into(),
            caller_pane: "agent".into(),
        },
        host.clone(),
    ));
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "input",
            "ModifyTick",
            json!({"tick_instance_id":target_instance(),"action":"send_input","text":"run"}),
        ),
        text("The input was refused."),
    ]));
    let agent = Agent::new_embedded(config(), model);
    agent
        .install_host_tools(Arc::new(NativeHostToolProvider::new(access)))
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, None, "Send input".into()).await.unwrap();
    let request = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let EventKind::PermissionRequested { request } = events.recv().await.unwrap().kind {
                break request;
            }
        }
    })
    .await
    .unwrap();
    host.changed_directory.store(true, Ordering::SeqCst);
    agent
        .answer_permission(chat, request.id, true)
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    assert!(host.sent.lock().unwrap().is_empty());
    agent.shutdown().await;
}

#[tokio::test]
async fn approved_text_and_keys_reach_the_selected_terminal_verbatim() {
    let host = Arc::new(ControlledHerdr {
        replaced: AtomicBool::new(false),
        changed_directory: AtomicBool::new(false),
        opened: AtomicBool::new(false),
        sent: Mutex::new(Vec::new()),
        protocol_instance: Mutex::new(None),
    });
    let access = Arc::new(TickAccess::with_host(
        HerdrContext {
            socket: "/tmp/controlled-herdr.sock".into(),
            caller_pane: "agent".into(),
        },
        host.clone(),
    ));
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "text",
            "ModifyTick",
            json!({"tick_instance_id":target_instance(),"action":"send_input","text":"literal ; $HOME `whoami`\n"}),
        ),
        call(
            "keys",
            "ModifyTick",
            json!({"tick_instance_id":target_instance(),"action":"send_input","keys":["Ctrl-C","Enter"]}),
        ),
        text("Both inputs were delivered."),
    ]));
    let agent = Agent::new_embedded(config(), model);
    agent
        .install_host_tools(Arc::new(NativeHostToolProvider::new(access)))
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Send both inputs".into())
        .await
        .unwrap();
    for _ in 0..2 {
        let request = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let EventKind::PermissionRequested { request } =
                    events.recv().await.unwrap().kind
                {
                    break request;
                }
            }
        })
        .await
        .unwrap();
        agent
            .answer_permission(chat, request.id, true)
            .await
            .unwrap();
    }
    wait_for_completion(&agent, chat).await;

    {
        let sent = host.sent.lock().unwrap();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].0, "pane.send_input");
        assert_eq!(sent[0].1["pane_id"], "target");
        assert_eq!(sent[0].1["text"], "literal ; $HOME `whoami`\n");
        assert_eq!(sent[1].1["pane_id"], "target");
        assert_eq!(sent[1].1["keys"], json!(["Ctrl-C", "Enter"]));
    }
    agent.shutdown().await;
}

#[tokio::test]
async fn agent_reads_and_changes_the_same_external_protocol_tick() {
    let directory = tempfile::Builder::new()
        .prefix("tlp")
        .tempdir_in("/private/tmp")
        .unwrap();
    let runtime_dir = directory.path().join("sessions");
    let host_socket = directory.path().join("herdr.sock");
    let script =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tick/examples/counter.py");
    let session = TickSession::spawn(SessionLaunch {
        command: vec!["python3".into(), script.to_string_lossy().into_owned()],
        cwd: directory.path().into(),
        backend: tickhost_connect::session::SessionBackend::Herdr {
            socket: host_socket.clone(),
            terminal_id: "protocol".into(),
        },
        runtime_dir: Some(runtime_dir.clone()),
    })
    .await
    .unwrap();
    let instance = session.instance().clone();
    let host = Arc::new(ControlledHerdr {
        replaced: AtomicBool::new(false),
        changed_directory: AtomicBool::new(false),
        opened: AtomicBool::new(false),
        sent: Mutex::new(Vec::new()),
        protocol_instance: Mutex::new(Some(instance.clone())),
    });
    let access = Arc::new(
        TickAccess::with_host(
            HerdrContext {
                socket: host_socket,
                caller_pane: "agent".into(),
            },
            host.clone(),
        )
        .with_runtime_dir(runtime_dir),
    );
    let model = Arc::new(ScriptedModel::new(vec![
        call("before", "ReadTick", json!({"tick_instance_id":instance})),
        call(
            "increment",
            "CallTickTool",
            json!({"tick_instance_id":instance,"tool":"increment","args":{"amount":5}}),
        ),
        call("after", "ReadTick", json!({"tick_instance_id":instance})),
        text("The counter is five."),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent
        .install_host_tools(Arc::new(NativeHostToolProvider::new(access)))
        .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Increment the counter by five".into())
        .await
        .unwrap();
    let request = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let EventKind::PermissionRequested { request } = events.recv().await.unwrap().kind {
                break request;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(request.input["tick_instance_id"], instance.to_string());
    assert_eq!(request.input["pane_id"], "protocol");
    assert_eq!(request.input["args"], json!({"amount":5}));
    agent
        .answer_permission(chat, request.id, true)
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;

    let messages = serde_json::to_string(&model.requests.lock().unwrap()[3].messages).unwrap();
    assert!(messages.contains("example.counter"));
    assert!(messages.contains("increment"));
    assert!(messages.contains("\\\"value\\\":5"));
    assert!(host.sent.lock().unwrap().is_empty());
    agent.shutdown().await;
    session.shutdown().await.unwrap();
}

#[tokio::test]
async fn agent_opens_a_program_in_a_new_herdr_tab_after_approval() {
    let host = Arc::new(ControlledHerdr {
        replaced: AtomicBool::new(false),
        changed_directory: AtomicBool::new(false),
        opened: AtomicBool::new(false),
        sent: Mutex::new(Vec::new()),
        protocol_instance: Mutex::new(None),
    });
    let access = Arc::new(TickAccess::with_host(
        HerdrContext {
            socket: "/tmp/controlled-herdr.sock".into(),
            caller_pane: "agent".into(),
        },
        host.clone(),
    ));
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "open",
            "ModifyTick",
            json!({"action":"open","command":["/bin/echo","hello; $HOME"],"cwd":"/private/tmp","placement":{"type":"new_tab"}}),
        ),
        text("Opened the program."),
    ]));
    let agent = Agent::new_embedded(config(), model);
    agent
        .install_host_tools(Arc::new(NativeHostToolProvider::new(access)))
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Open the program".into())
        .await
        .unwrap();
    let request = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let EventKind::PermissionRequested { request } = events.recv().await.unwrap().kind {
                break request;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(request.input["workspace_id"], "workspace-a");
    assert_eq!(request.input["command"][1], "hello; $HOME");
    assert!(
        request.input["command"][0]
            .as_str()
            .unwrap()
            .ends_with("/echo")
    );
    assert!(host.sent.lock().unwrap().is_empty());
    agent
        .answer_permission(chat, request.id, true)
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;

    {
        let sent = host.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, "layout.apply");
        assert_eq!(sent[0].1["focus"], false);
        assert_eq!(sent[0].1["root"]["command"][1], "hello; $HOME");
    }
    assert_eq!(
        agent.snapshot(chat).await.unwrap().operations[0].state,
        tickloop_agent_protocol::OperationState::Completed
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn agent_places_and_moves_ticks_only_after_approval() {
    let host = Arc::new(ControlledHerdr {
        replaced: AtomicBool::new(false),
        changed_directory: AtomicBool::new(false),
        opened: AtomicBool::new(false),
        sent: Mutex::new(Vec::new()),
        protocol_instance: Mutex::new(None),
    });
    let access = Arc::new(TickAccess::with_host(
        HerdrContext {
            socket: "/tmp/controlled-herdr.sock".into(),
            caller_pane: "agent".into(),
        },
        host.clone(),
    ));
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "open-here",
            "ModifyTick",
            json!({
                "action":"open", "command":["/bin/cat"], "cwd":"/private/tmp",
                "placement":{"type":"current_tab","direction":"right"}
            }),
        ),
        call(
            "move-here",
            "ModifyTick",
            json!({
                "action":"move", "tick_instance_id":target_instance(),
                "placement":{"type":"current_tab","direction":"down"}
            }),
        ),
        text("Both Ticks are beside this Agent."),
    ]));
    let agent = Agent::new_embedded(config(), model);
    agent
        .install_host_tools(Arc::new(NativeHostToolProvider::new(access)))
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(
            chat,
            None,
            "Open a program to your right and move the selected Tick below you.".into(),
        )
        .await
        .unwrap();
    for (action, prior_mutations) in [("open", 0), ("move", 2)] {
        let request = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let EventKind::PermissionRequested { request } =
                    events.recv().await.unwrap().kind
                {
                    break request;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(request.input["action"], action);
        assert_eq!(request.input["workspace_id"], "workspace-a");
        assert_eq!(host.sent.lock().unwrap().len(), prior_mutations);
        agent
            .answer_permission(chat, request.id, true)
            .await
            .unwrap();
    }
    wait_for_completion(&agent, chat).await;
    {
        let sent = host.sent.lock().unwrap();
        assert_eq!(sent.len(), 4);
        assert_eq!(sent[0].0, "layout.apply");
        assert_eq!(sent[2].0, "pane.move");
        assert_eq!(sent[2].1["pane_id"], "target");
        assert_eq!(sent[2].1["destination"]["type"], "new_tab");
        for (index, pane, direction) in [(1, "opened", "right"), (3, "target", "down")] {
            assert_eq!(sent[index].0, "pane.move");
            assert_eq!(sent[index].1["pane_id"], pane);
            assert_eq!(sent[index].1["focus"], false);
            assert_eq!(sent[index].1["destination"]["target_pane_id"], "agent");
            assert_eq!(sent[index].1["destination"]["tab_id"], "tab");
            assert_eq!(sent[index].1["destination"]["split"], direction);
        }
    }
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(
        snapshot
            .operations
            .iter()
            .all(|operation| operation.state == tickloop_agent_protocol::OperationState::Completed)
    );
    agent.shutdown().await;
}

#[tokio::test]
#[ignore = "requires TICKLOOP_TEST_HERDR_SOCKET for an isolated Herdr server"]
async fn isolated_herdr_keeps_background_host_access_and_places_beside_the_input_view() {
    use tickhost_connect::herdr::SocketHerdr;
    use tickloop_agent_protocol::{DisplayUpdate, HostLocation, InteractionSource, ViewRef};
    let socket = std::path::PathBuf::from(std::env::var("TICKLOOP_TEST_HERDR_SOCKET").unwrap());
    assert!(
        socket
            .to_string_lossy()
            .contains("tickloop-herdr-implementation")
    );
    let directory = tempfile::tempdir().unwrap();
    let host = Arc::new(SocketHerdr::new(HerdrContext::without_view(socket.clone())));
    let created = host.request("workspace.create", json!({"cwd":directory.path(),"focus":false,"label":format!("agent-host-{}",uuid::Uuid::new_v4()),"env":{"ZDOTDIR":directory.path(),"HISTFILE":"/dev/null"}}), CancellationToken::new()).await.unwrap();
    let workspace = created["workspace"]["workspace_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let old_caller = created["root_pane"]["pane_id"].as_str().unwrap().to_owned();
    let target = host.request("layout.apply", json!({"workspace_id":workspace,"focus":false,"root":{"type":"pane","command":["/bin/cat"],"cwd":directory.path()}}), CancellationToken::new()).await.unwrap();
    let target_pane = target["layout"]["root"]["pane_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let tick_access = Arc::new(TickAccess::with_host(
        HerdrContext {
            socket: socket.clone(),
            caller_pane: old_caller.clone(),
        },
        host.clone(),
    ));
    let target_instance =
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let inventory = tick_access.discover(&workspace).await.unwrap();
                if let Some(item) = inventory["ticks"].as_array().unwrap().iter().find(|item| {
                    item["pane_id"] == target_pane && item["tick_instance_id"].is_string()
                }) {
                    break item["tick_instance_id"].clone();
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap();
    host.request(
        "pane.close",
        json!({"pane_id":old_caller}),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call("discover", "ReadWorkspace", json!({})),
        call(
            "read",
            "ReadTick",
            json!({"tick_instance_id":target_instance}),
        ),
        call(
            "open",
            "ModifyTick",
            json!({"action":"open","command":["/bin/cat"],"placement":{"type":"new_tab"}}),
        ),
        call(
            "no_anchor",
            "ModifyTick",
            json!({"action":"open","command":["/bin/cat"],"placement":{"type":"current_tab","direction":"right"}}),
        ),
        text("The background host task completed."),
        call(
            "beside",
            "ModifyTick",
            json!({"action":"open","command":["/bin/cat"],"placement":{"type":"current_tab","direction":"right"}}),
        ),
        text("The Tick is beside the input View."),
    ]));
    let runtime = Agent::new_embedded(config(), model);
    runtime
        .install_host_tools(Arc::new(NativeHostToolProvider::new(tick_access)))
        .unwrap();
    let access = EmbeddedAgentAccess::new(runtime.clone());
    let mut launch = LaunchSpec::embedded(directory.path().into());
    launch.host_work_context = Some(HostWorkContext {
        socket: socket.clone(),
        workspace: Some(workspace.clone()),
    });
    let receipt = access
        .execute(CommandEnvelope {
            daemon_epoch: access.daemon_epoch(),
            command_id: uuid::Uuid::new_v4(),
            command: AgentCommand::Create {
                launch,
                initial: None,
                origin: None,
            },
        })
        .await
        .unwrap();
    let Some(CommandResult::Created { execution, .. }) = receipt.result else {
        panic!("{receipt:?}");
    };
    let configured = access
        .execute(CommandEnvelope {
            daemon_epoch: access.daemon_epoch(),
            command_id: uuid::Uuid::new_v4(),
            command: AgentCommand::Configure {
                provider: None,
                execution,
                change: ConfigurationChange::ExecutionMode {
                    mode: ExecutionMode::FullAccess,
                },
            },
        })
        .await
        .unwrap();
    assert!(configured.error.is_none(), "{configured:?}");
    let mut events = runtime.subscribe();
    let submitted = access
        .execute(CommandEnvelope {
            daemon_epoch: access.daemon_epoch(),
            command_id: uuid::Uuid::new_v4(),
            command: AgentCommand::Submit {
                execution,
                target_loop: None,
                input: UserInput::Text {
                    text: "Discover, read and open without a View.".into(),
                    contexts: vec![],
                    paths: vec![],
                },
                origin: None,
            },
        })
        .await
        .unwrap();
    assert!(submitted.error.is_none(), "{submitted:?}");
    let mut results = BTreeMap::new();
    loop {
        match tickloop_agent_test_support::next_event(&mut events)
            .await
            .kind
        {
            EventKind::ToolCompleted { call_id, result } => {
                results.insert(call_id, result);
            }
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert!(results["no_anchor"].is_error);
    assert!(results["no_anchor"].content.contains("MissingAnchor"));
    for id in ["discover", "read", "open"] {
        assert!(!results[id].is_error, "{id}: {}", results[id].content);
    }
    let inventory: Value = serde_json::from_str(&results["discover"].content).unwrap();
    assert!(inventory["caller"].is_null());
    let opened: Value = serde_json::from_str(&results["open"].content).unwrap();
    assert_eq!(opened["status"], "opened");
    let opened_pane = host
        .request(
            "pane.get",
            json!({"pane_id":opened["pane_id"]}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(opened["terminal_id"], opened_pane["pane"]["terminal_id"]);
    assert_eq!(opened["tab_id"], opened_pane["pane"]["tab_id"]);
    assert_eq!(opened["workspace_id"], opened_pane["pane"]["workspace_id"]);
    assert_eq!(
        opened_pane["pane"]["cwd"],
        json!(directory.path().canonicalize().unwrap())
    );
    host.request(
        "pane.focus",
        json!({"pane_id":opened["pane_id"]}),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let source = host
        .request(
            "pane.get",
            json!({"pane_id":target_pane}),
            CancellationToken::new(),
        )
        .await
        .unwrap()["pane"]
        .clone();
    let view = ViewRef {
        session_id: uuid::Uuid::new_v4(),
        view_id: uuid::Uuid::new_v4(),
    };
    use std::os::unix::fs::MetadataExt;
    let socket_metadata = std::fs::symlink_metadata(&socket).unwrap();
    let location = HostLocation {
        host_instance: format!("herdr:{}:{}", socket_metadata.dev(), socket_metadata.ino()),
        socket: socket.clone(),
        workspace: Some(workspace.clone()),
        tab: source["tab_id"].as_str().map(str::to_owned),
        pane: Some(target_pane.clone()),
        terminal: source["terminal_id"].as_str().map(str::to_owned),
        client: None,
    };
    let displayed = access
        .show_view(DisplayUpdate::Displayed {
            view,
            expected_version: 0,
            execution,
            host: Some(location.clone()),
        })
        .await
        .unwrap();
    let submitted = access
        .execute(CommandEnvelope {
            daemon_epoch: access.daemon_epoch(),
            command_id: uuid::Uuid::new_v4(),
            command: AgentCommand::Submit {
                execution,
                target_loop: None,
                input: UserInput::Text {
                    text: "Open beside this input View.".into(),
                    contexts: vec![],
                    paths: vec![],
                },
                origin: Some(InteractionSource {
                    view,
                    display_version: displayed.version,
                    host: Some(location),
                }),
            },
        })
        .await
        .unwrap();
    assert!(submitted.error.is_none(), "{submitted:?}");
    access
        .show_view(DisplayUpdate::Left {
            view,
            version: displayed.version,
        })
        .await
        .unwrap();
    loop {
        match tickloop_agent_test_support::next_event(&mut events)
            .await
            .kind
        {
            EventKind::ToolCompleted { call_id, result } => {
                results.insert(call_id, result);
            }
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert!(!results["beside"].is_error, "{}", results["beside"].content);
    let beside: Value = serde_json::from_str(&results["beside"].content).unwrap();
    assert_eq!(beside["tab_id"], source["tab_id"]);
    assert_ne!(beside["tab_id"], opened["tab_id"]);
    let neighbor = host
        .request(
            "pane.neighbor",
            json!({"pane_id":target_pane,"direction":"right"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        neighbor["neighbor"]["neighbor_pane_id"], beside["pane_id"],
        "{neighbor}"
    );
    println!("isolated-background={opened} captured-view-right={beside} neighbor={neighbor}");
    runtime.shutdown().await;
    host.request(
        "workspace.close",
        json!({"workspace_id":workspace}),
        CancellationToken::new(),
    )
    .await
    .unwrap();
}
