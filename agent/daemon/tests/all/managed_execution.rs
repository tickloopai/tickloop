//! Managed execution control is separate from ordinary Console mutation and lifetime.

use std::{
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use tickloop_agent_client::LocalAgentClient;
use tickloop_agent_daemon::{DaemonServer, EmbeddedAgentAccess, WorkerFactory, serve_listener};
use tickloop_agent_protocol::{
    AccessError, AccessErrorCode, AgentAccess, AgentCommand, AgentDetailQuery, CommandEnvelope,
    CommandReceipt, CommandResult, DisplayReceipt, DisplayUpdate, ExecutionMode, ExecutionProfile,
    ExecutionRef, LaunchSpec, LiveChatSnapshot, ManagedToken, Observation, ObservationRequest,
    ObservationValue, PendingRequestKind, PermissionScope, PromptPolicy, RequestRef, UserInput,
};
use tickloop_agent_runtime::{
    Agent, HarnessOptions, HarnessSeams, ToolPolicy, WaitHosting, environment::LocalEnvironment,
};
use tickloop_agent_test_support::{ScriptedModel, call, config, harness::ChosenModels, text};
use tokio::net::UnixListener;
use uuid::Uuid;

const WAIT: Duration = Duration::from_secs(10);

struct Factory {
    model: Arc<ScriptedModel>,
    runtimes: Mutex<Vec<Agent>>,
    launches: Mutex<Vec<LaunchSpec>>,
    cleanups: Mutex<Vec<(Uuid, Uuid)>>,
    block_directory: Arc<AtomicBool>,
    directory_blocked: Arc<tokio::sync::Notify>,
    fail_settle: bool,
}

impl Factory {
    fn new(replies: Vec<Vec<tickloop_agent_protocol::Content>>) -> Arc<Self> {
        Arc::new(Self {
            model: Arc::new(ScriptedModel::new(replies)),
            runtimes: Mutex::new(vec![]),
            launches: Mutex::new(vec![]),
            cleanups: Mutex::new(vec![]),
            block_directory: Arc::new(AtomicBool::new(false)),
            directory_blocked: Arc::new(tokio::sync::Notify::new()),
            fail_settle: false,
        })
    }

    async fn shutdown(&self) {
        let runtimes = std::mem::take(&mut *self.runtimes.lock().unwrap());
        for runtime in runtimes {
            runtime.shutdown().await;
        }
    }
}

#[async_trait]
impl WorkerFactory for Factory {
    async fn start(
        &self,
        launch: &LaunchSpec,
        epoch: Uuid,
    ) -> Result<Arc<dyn AgentAccess>, AccessError> {
        self.launches.lock().unwrap().push(launch.clone());
        let runtime = Agent::with_seams(
            HarnessSeams {
                transcripts: None,
                environment: Some(Arc::new(LocalEnvironment::bare())),
                models: ChosenModels::new(self.model.clone()),
            },
            HarnessOptions {
                model: config(),
                settings: None,
                search: Default::default(),
                host: launch.host_options,
                tools: match launch.profile {
                    ExecutionProfile::Local => ToolPolicy::local(),
                    ExecutionProfile::OneShot => ToolPolicy::one_shot(),
                    ExecutionProfile::Acp => ToolPolicy::acp(),
                },
                host_rules: vec![],
                task_tools: Some(true),
                min_schedule_interval: tickloop_agent_runtime::LOCAL_MIN_SCHEDULE_INTERVAL,
                waits: WaitHosting::Process,
                citation_style: Default::default(),
                system_prompt_addition: None,
                request_gates: vec![],
            },
        )
        .unwrap();
        self.runtimes.lock().unwrap().push(runtime.clone());
        Ok(Arc::new(Worker {
            inner: EmbeddedAgentAccess::for_worker(runtime, epoch),
            block_directory: self.block_directory.clone(),
            directory_blocked: self.directory_blocked.clone(),
            fail_settle: self.fail_settle,
        }))
    }

    async fn exited(&self, _: &Arc<dyn AgentAccess>) -> bool {
        false
    }

    async fn end_managed(
        &self,
        _: &LaunchSpec,
        chat: Uuid,
        operation: Uuid,
    ) -> Result<(), AccessError> {
        self.cleanups.lock().unwrap().push((chat, operation));
        Ok(())
    }
}

struct Worker {
    inner: EmbeddedAgentAccess,
    block_directory: Arc<AtomicBool>,
    directory_blocked: Arc<tokio::sync::Notify>,
    fail_settle: bool,
}

#[async_trait]
impl AgentAccess for Worker {
    async fn observe(&self, query: ObservationRequest) -> Result<Observation, AccessError> {
        if self.block_directory.load(Ordering::SeqCst)
            && matches!(query, ObservationRequest::Overview { .. })
        {
            self.directory_blocked.notify_one();
            return std::future::pending().await;
        }
        self.inner.observe(query).await
    }
    async fn execute(&self, command: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
        if self.fail_settle && matches!(command.command, AgentCommand::ManagedSettle { .. }) {
            return Err(AccessError::unavailable(
                "injected Worker settlement failure",
            ));
        }
        self.inner.execute(command).await
    }
    async fn show_view(&self, update: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
        self.inner.show_view(update).await
    }
}

fn launch(path: &Path, profile: ExecutionProfile) -> LaunchSpec {
    LaunchSpec {
        profile,
        ..LaunchSpec::embedded(path.into())
    }
}

fn input(value: &str) -> UserInput {
    UserInput::Text {
        text: value.into(),
        contexts: vec![],
        paths: vec![],
    }
}

async fn execute(
    access: &dyn AgentAccess,
    epoch: Uuid,
    command: AgentCommand,
) -> Result<CommandResult, AccessError> {
    tokio::time::timeout(
        WAIT,
        access.execute(CommandEnvelope {
            daemon_epoch: epoch,
            command_id: Uuid::new_v4(),
            command,
        }),
    )
    .await
    .expect("command must finish promptly")?
    .into_result()
}

async fn create(
    access: &dyn AgentAccess,
    epoch: Uuid,
    launch: LaunchSpec,
    token: &ManagedToken,
) -> ExecutionRef {
    let result = execute(
        access,
        epoch,
        AgentCommand::CreateManaged {
            launch,
            initial: None,
            session_id: Uuid::new_v4(),
            token: token.clone(),
        },
    )
    .await
    .unwrap();
    let CommandResult::Created {
        execution,
        submitted,
        shell,
        snapshot,
    } = result
    else {
        panic!("managed Create must return Created")
    };
    assert!(submitted.is_none());
    assert!(shell.is_none());
    assert!(snapshot.conversation.messages.is_empty());
    execution
}

async fn snapshot(access: &dyn AgentAccess, execution: ExecutionRef) -> LiveChatSnapshot {
    let observation = access
        .observe(ObservationRequest::Agent {
            execution,
            detail: AgentDetailQuery::Snapshot,
            after: None,
        })
        .await
        .unwrap();
    let ObservationValue::Agent(snapshot) = observation.initial else {
        panic!("expected Agent snapshot")
    };
    *snapshot
}

async fn submit(
    daemon: &DaemonServer,
    execution: ExecutionRef,
    token: &ManagedToken,
    value: &str,
) -> Uuid {
    let result = execute(
        daemon,
        daemon.daemon_epoch(),
        AgentCommand::ManagedSubmit {
            execution,
            token: token.clone(),
            input: input(value),
        },
    )
    .await
    .unwrap();
    let CommandResult::Submitted(receipt) = result else {
        panic!("expected submission")
    };
    receipt.run_id
}

async fn settle(
    daemon: &DaemonServer,
    execution: ExecutionRef,
    token: &ManagedToken,
    cancel: bool,
) -> LiveChatSnapshot {
    let result = execute(
        daemon,
        daemon.daemon_epoch(),
        AgentCommand::ManagedSettle {
            execution,
            token: token.clone(),
            cancel,
        },
    )
    .await
    .unwrap();
    let CommandResult::ManagedSettled { snapshot } = result else {
        panic!("expected settled snapshot")
    };
    *snapshot
}

#[tokio::test]
async fn managed_profiles_are_normalized_and_offer_no_unrouted_tools() {
    for (profile, mode) in [
        (ExecutionProfile::OneShot, None),
        (ExecutionProfile::Acp, None),
        (ExecutionProfile::OneShot, Some(ExecutionMode::FullAccess)),
        (ExecutionProfile::Acp, Some(ExecutionMode::AcceptEdits)),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let factory = Factory::new(vec![text("done")]);
        let daemon = DaemonServer::new(Uuid::new_v4(), factory.clone());
        let token = ManagedToken::new();
        let mut spec = launch(directory.path(), profile);
        spec.host_options.execution_mode_override = mode;
        let execution = create(&daemon, daemon.daemon_epoch(), spec, &token).await;
        let run = submit(&daemon, execution, &token, "finish").await;
        let final_state = settle(&daemon, execution, &token, false).await;
        assert_eq!(
            final_state
                .clone()
                .into_snapshot()
                .run_response_text(run)
                .as_deref(),
            Some("done")
        );
        {
            let launches = factory.launches.lock().unwrap();
            assert_eq!(
                launches[0].host_options.execution_mode_override,
                mode.or(Some(ExecutionMode::Auto))
            );
            assert_eq!(
                launches[0].host_options.prompt_policy,
                if profile == ExecutionProfile::OneShot {
                    PromptPolicy::DenyOnPrompt
                } else {
                    PromptPolicy::Interactive
                }
            );
            let requests = factory.model.requests.lock().unwrap();
            let tools = &requests[0].tools;
            for name in [
                "AskUserQuestion",
                "EnterPlanMode",
                "ExitPlanMode",
                "StartGoal",
                "Subagent",
                "ListSubagents",
                "SendMessage",
                "StopSubagent",
                "TaskCreate",
                "TaskList",
                "TaskGet",
                "TaskUpdate",
            ] {
                assert!(!tools.iter().any(|tool| tool.name == name), "{name}");
            }
            assert!(tools.iter().any(|tool| tool.name == "Bash"));
        }
        factory.shutdown().await;
    }
}

#[tokio::test]
async fn invalid_creation_does_not_start_a_worker_or_submit_input() {
    let directory = tempfile::tempdir().unwrap();
    let factory = Factory::new(vec![]);
    let daemon = DaemonServer::new(Uuid::new_v4(), factory.clone());
    let mut exact = launch(directory.path(), ExecutionProfile::OneShot);
    exact.exact_chat = Some(Uuid::new_v4());
    let mut plan = launch(directory.path(), ExecutionProfile::Acp);
    plan.host_options.execution_mode_override = Some(ExecutionMode::Plan);
    for (spec, initial) in [
        (launch(directory.path(), ExecutionProfile::Local), None),
        (exact, None),
        (plan, None),
        (
            launch(directory.path(), ExecutionProfile::Acp),
            Some(input("must not run")),
        ),
    ] {
        assert!(
            execute(
                &daemon,
                daemon.daemon_epoch(),
                AgentCommand::CreateManaged {
                    launch: spec,
                    initial,
                    session_id: Uuid::new_v4(),
                    token: ManagedToken::new(),
                }
            )
            .await
            .is_err()
        );
    }
    assert!(factory.launches.lock().unwrap().is_empty());
    assert!(factory.model.requests.lock().unwrap().is_empty());
    assert!(
        execute(
            &daemon,
            daemon.daemon_epoch(),
            AgentCommand::Create {
                launch: launch(directory.path(), ExecutionProfile::OneShot),
                initial: None,
                origin: None,
            }
        )
        .await
        .is_err()
    );
    factory.shutdown().await;
}

#[tokio::test]
async fn embedded_creation_cannot_mislabel_the_actual_harness_profile() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(vec![])));
    let access = EmbeddedAgentAccess::new(runtime.clone());
    for profile in [ExecutionProfile::OneShot, ExecutionProfile::Acp] {
        assert!(
            execute(
                &access,
                access.daemon_epoch(),
                AgentCommand::CreateManaged {
                    launch: launch(directory.path(), profile),
                    initial: None,
                    session_id: Uuid::new_v4(),
                    token: ManagedToken::new(),
                }
            )
            .await
            .unwrap_err()
            .message
            .contains("profile")
        );
    }
    runtime.shutdown().await;

    let factory = Factory::new(vec![]);
    let epoch = Uuid::new_v4();
    let mut spec = launch(directory.path(), ExecutionProfile::OneShot);
    spec.host_options.execution_mode_override = Some(ExecutionMode::Auto);
    spec.host_options.prompt_policy = PromptPolicy::DenyOnPrompt;
    let managed = factory.start(&spec, epoch).await.unwrap();
    assert!(
        execute(
            &*managed,
            epoch,
            AgentCommand::Create {
                launch: launch(directory.path(), ExecutionProfile::Local),
                initial: None,
                origin: None,
            }
        )
        .await
        .is_err()
    );
    assert!(
        execute(
            &*managed,
            epoch,
            AgentCommand::Create {
                launch: spec,
                initial: None,
                origin: None,
            }
        )
        .await
        .is_err()
    );
    factory.shutdown().await;
}

#[tokio::test]
async fn managed_proof_is_bound_to_the_execution_and_generation() {
    let directory = tempfile::tempdir().unwrap();
    let factory = Factory::new(vec![]);
    let daemon = DaemonServer::new(Uuid::new_v4(), factory.clone());
    let token = ManagedToken::new();
    let execution = create(
        &daemon,
        daemon.daemon_epoch(),
        launch(directory.path(), ExecutionProfile::Acp),
        &token,
    )
    .await;
    for (target, proof) in [
        (execution, ManagedToken::new()),
        (
            ExecutionRef {
                generation: execution.generation + 1,
                ..execution
            },
            token.clone(),
        ),
        (
            ExecutionRef {
                agent_id: Uuid::new_v4(),
                ..execution
            },
            token.clone(),
        ),
    ] {
        let error = execute(
            &daemon,
            daemon.daemon_epoch(),
            AgentCommand::ManagedHeartbeat {
                execution: target,
                token: proof,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, AccessErrorCode::AccessDenied);
    }
    execute(
        &daemon,
        daemon.daemon_epoch(),
        AgentCommand::ManagedHeartbeat {
            execution,
            token: token.clone(),
        },
    )
    .await
    .unwrap();
    execute(
        &daemon,
        daemon.daemon_epoch(),
        AgentCommand::ManagedRelease { execution, token },
    )
    .await
    .unwrap();
    factory.shutdown().await;
}

#[tokio::test]
async fn ordinary_mutation_is_denied_for_managed_but_local_chats_are_unchanged() {
    let directory = tempfile::tempdir().unwrap();
    let factory = Factory::new(vec![text("local answer")]);
    let daemon = DaemonServer::new(Uuid::new_v4(), factory.clone());
    let token = ManagedToken::new();
    let managed = create(
        &daemon,
        daemon.daemon_epoch(),
        launch(directory.path(), ExecutionProfile::Acp),
        &token,
    )
    .await;
    let error = execute(
        &daemon,
        daemon.daemon_epoch(),
        AgentCommand::Submit {
            execution: managed,
            target_loop: None,
            input: input("unauthorized"),
            origin: None,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, AccessErrorCode::AccessDenied);
    assert!(
        snapshot(&daemon, managed)
            .await
            .conversation
            .messages
            .is_empty()
    );
    let CommandResult::Created {
        execution: local, ..
    } = execute(
        &daemon,
        daemon.daemon_epoch(),
        AgentCommand::Create {
            launch: launch(directory.path(), ExecutionProfile::Local),
            initial: None,
            origin: None,
        },
    )
    .await
    .unwrap()
    else {
        panic!("expected local Chat")
    };
    assert!(matches!(
        execute(
            &daemon,
            daemon.daemon_epoch(),
            AgentCommand::Submit {
                execution: local,
                target_loop: None,
                input: input("ordinary input"),
                origin: None,
            }
        )
        .await
        .unwrap(),
        CommandResult::Submitted(_)
    ));
    execute(
        &daemon,
        daemon.daemon_epoch(),
        AgentCommand::ManagedRelease {
            execution: managed,
            token,
        },
    )
    .await
    .unwrap();
    factory.shutdown().await;
}

#[tokio::test]
async fn acp_chat_reuses_identity_after_settle_and_cancel_then_release_revokes_control() {
    let directory = tempfile::tempdir().unwrap();
    let factory = Factory::new(vec![text("first answer"), text("second answer")]);
    let daemon = DaemonServer::new(Uuid::new_v4(), factory.clone());
    let token = ManagedToken::new();
    let execution = create(
        &daemon,
        daemon.daemon_epoch(),
        launch(directory.path(), ExecutionProfile::Acp),
        &token,
    )
    .await;
    let chat = snapshot(&daemon, execution).await.conversation.id;
    let first = submit(&daemon, execution, &token, "first").await;
    let first_snapshot = settle(&daemon, execution, &token, false).await;
    assert_eq!(
        first_snapshot
            .into_snapshot()
            .run_response_text(first)
            .as_deref(),
        Some("first answer")
    );
    let cancelled = settle(&daemon, execution, &token, true).await;
    assert_eq!(cancelled.conversation.id, chat);
    let second = submit(&daemon, execution, &token, "second").await;
    let second_snapshot = settle(&daemon, execution, &token, false).await;
    assert_eq!(second_snapshot.conversation.id, chat);
    assert_ne!(first, second);
    assert_eq!(
        second_snapshot
            .into_snapshot()
            .run_response_text(second)
            .as_deref(),
        Some("second answer")
    );
    {
        let cleanups = factory.cleanups.lock().unwrap();
        assert_eq!(cleanups.len(), 3);
        assert!(cleanups.iter().all(|(cleaned, _)| *cleaned == chat));
        assert_eq!(
            cleanups[0].1, cleanups[1].1,
            "retry cleanup must preserve operation identity"
        );
        assert_ne!(
            cleanups[0].1, cleanups[2].1,
            "a new prompt needs new cleanup ownership"
        );
    }
    execute(
        &daemon,
        daemon.daemon_epoch(),
        AgentCommand::ManagedRelease {
            execution,
            token: token.clone(),
        },
    )
    .await
    .unwrap();
    for command in [
        AgentCommand::ManagedHeartbeat {
            execution,
            token: token.clone(),
        },
        AgentCommand::ManagedSubmit {
            execution,
            token,
            input: input("after release"),
        },
    ] {
        assert_eq!(
            execute(&daemon, daemon.daemon_epoch(), command)
                .await
                .unwrap_err()
                .code,
            AccessErrorCode::AccessDenied
        );
    }
    assert_eq!(factory.model.requests.lock().unwrap().len(), 2);
    factory.shutdown().await;
}

#[tokio::test]
async fn one_shot_rejects_a_second_input_and_tokens_do_not_leak_in_results() {
    let directory = tempfile::tempdir().unwrap();
    let factory = Factory::new(vec![text("one answer")]);
    let daemon = DaemonServer::new(Uuid::new_v4(), factory.clone());
    let token = ManagedToken::new();
    let secret = serde_json::to_value(&token)
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned();
    assert!(!format!("{token:?}").contains(&secret));
    let execution = create(
        &daemon,
        daemon.daemon_epoch(),
        launch(directory.path(), ExecutionProfile::OneShot),
        &token,
    )
    .await;
    submit(&daemon, execution, &token, "once").await;
    let settled = settle(&daemon, execution, &token, false).await;
    assert!(!serde_json::to_string(&settled).unwrap().contains(&secret));
    assert!(!format!("{settled:?}").contains(&secret));
    let command = AgentCommand::ManagedSubmit {
        execution,
        token: token.clone(),
        input: input("twice"),
    };
    assert!(!format!("{command:?}").contains(&secret));
    assert!(
        execute(&daemon, daemon.daemon_epoch(), command)
            .await
            .is_err()
    );
    assert_eq!(factory.model.requests.lock().unwrap().len(), 1);
    execute(
        &daemon,
        daemon.daemon_epoch(),
        AgentCommand::ManagedRelease { execution, token },
    )
    .await
    .unwrap();
    factory.shutdown().await;
}

async fn pending_permission(daemon: &DaemonServer, execution: ExecutionRef) -> RequestRef {
    tokio::time::timeout(WAIT, async {
        loop {
            if let Some(request) = snapshot(daemon, execution)
                .await
                .request_refs
                .into_iter()
                .find(|request| request.kind == PendingRequestKind::Permission)
            {
                return request;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the external Read must wait for permission")
}

#[tokio::test]
async fn acp_permission_reply_is_once_only_and_cancellation_clears_a_pending_run() {
    let directory = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    let source = external.path().join("source.txt");
    std::fs::write(&source, "external test data").unwrap();
    let factory = Factory::new(vec![
        call(
            "read-once",
            "Read",
            serde_json::json!({"file_path": source}),
        ),
        text("first approved"),
        call(
            "read-again",
            "Read",
            serde_json::json!({"file_path": source}),
        ),
        text("after cancellation"),
    ]);
    let daemon = DaemonServer::new(Uuid::new_v4(), factory.clone());
    let token = ManagedToken::new();
    let execution = create(
        &daemon,
        daemon.daemon_epoch(),
        launch(directory.path(), ExecutionProfile::Acp),
        &token,
    )
    .await;
    let first = submit(&daemon, execution, &token, "read external data").await;
    let request = pending_permission(&daemon, execution).await;
    assert_eq!(
        execute(
            &daemon,
            daemon.daemon_epoch(),
            AgentCommand::DecidePermission {
                request,
                allow: true,
                scope: PermissionScope::Once,
                comment: None,
                rule: None,
            }
        )
        .await
        .unwrap_err()
        .code,
        AccessErrorCode::AccessDenied
    );
    execute(
        &daemon,
        daemon.daemon_epoch(),
        AgentCommand::ManagedDecidePermission {
            request,
            token: token.clone(),
            allow: true,
            scope: PermissionScope::Once,
            comment: None,
            rule: None,
        },
    )
    .await
    .unwrap();
    let completed = settle(&daemon, execution, &token, false).await;
    assert_eq!(
        completed
            .into_snapshot()
            .run_response_text(first)
            .as_deref(),
        Some("first approved")
    );

    submit(&daemon, execution, &token, "read again").await;
    let second_request = pending_permission(&daemon, execution).await;
    assert_ne!(
        request.request_id, second_request.request_id,
        "Once permission must not survive the prompt"
    );
    let cancelled = settle(&daemon, execution, &token, true).await;
    assert!(cancelled.request_refs.is_empty());
    assert!(cancelled.permissions.is_empty());
    assert!(
        cancelled
            .conversation
            .loops
            .iter()
            .any(|item| item.state == tickloop_agent_protocol::LoopState::Interrupted)
    );
    assert_eq!(factory.model.requests.lock().unwrap().len(), 3);
    let next = submit(&daemon, execution, &token, "continue without reading").await;
    let resumed = settle(&daemon, execution, &token, false).await;
    assert_eq!(
        resumed.into_snapshot().run_response_text(next).as_deref(),
        Some("after cancellation")
    );
    execute(
        &daemon,
        daemon.daemon_epoch(),
        AgentCommand::ManagedRelease { execution, token },
    )
    .await
    .unwrap();
    factory.shutdown().await;
}

#[tokio::test]
async fn managed_permission_decisions_are_acp_once_only() {
    let directory = tempfile::tempdir().unwrap();
    let factory = Factory::new(vec![]);
    let daemon = DaemonServer::new(Uuid::new_v4(), factory.clone());
    for profile in [ExecutionProfile::OneShot, ExecutionProfile::Acp] {
        let token = ManagedToken::new();
        let execution = create(
            &daemon,
            daemon.daemon_epoch(),
            launch(directory.path(), profile),
            &token,
        )
        .await;
        let request = RequestRef {
            execution,
            request_id: Uuid::new_v4(),
            kind: PendingRequestKind::Permission,
            version: 1,
        };
        for (scope, rule) in [
            (PermissionScope::Session, None),
            (PermissionScope::Persistent, None),
            (PermissionScope::Once, Some("Bash(*)".into())),
        ] {
            let error = execute(
                &daemon,
                daemon.daemon_epoch(),
                AgentCommand::ManagedDecidePermission {
                    request,
                    token: token.clone(),
                    allow: true,
                    scope,
                    comment: None,
                    rule,
                },
            )
            .await
            .unwrap_err();
            assert_eq!(error.code, AccessErrorCode::AccessDenied);
        }
        if profile == ExecutionProfile::OneShot {
            assert_eq!(
                execute(
                    &daemon,
                    daemon.daemon_epoch(),
                    AgentCommand::ManagedDecidePermission {
                        request,
                        token: token.clone(),
                        allow: true,
                        scope: PermissionScope::Once,
                        comment: None,
                        rule: None,
                    }
                )
                .await
                .unwrap_err()
                .code,
                AccessErrorCode::AccessDenied
            );
        }
        execute(
            &daemon,
            daemon.daemon_epoch(),
            AgentCommand::ManagedRelease { execution, token },
        )
        .await
        .unwrap();
    }
    factory.shutdown().await;
}

#[tokio::test]
async fn hosted_cleanup_runs_even_when_worker_settlement_fails() {
    let directory = tempfile::tempdir().unwrap();
    let mut factory = Factory::new(vec![]);
    Arc::get_mut(&mut factory).unwrap().fail_settle = true;
    let daemon = DaemonServer::new(Uuid::new_v4(), factory.clone());
    let token = ManagedToken::new();
    let execution = create(
        &daemon,
        daemon.daemon_epoch(),
        launch(directory.path(), ExecutionProfile::Acp),
        &token,
    )
    .await;
    let chat = snapshot(&daemon, execution).await.conversation.id;
    let error = execute(
        &daemon,
        daemon.daemon_epoch(),
        AgentCommand::ManagedSettle {
            execution,
            token: token.clone(),
            cancel: true,
        },
    )
    .await
    .unwrap_err();
    assert!(error.message.contains("injected Worker settlement failure"));
    assert_eq!(factory.cleanups.lock().unwrap().len(), 1);
    assert_eq!(factory.cleanups.lock().unwrap()[0].0, chat);
    execute(
        &daemon,
        daemon.daemon_epoch(),
        AgentCommand::ManagedRelease { execution, token },
    )
    .await
    .unwrap();
    factory.shutdown().await;
}

#[tokio::test]
async fn ipc_creation_must_claim_the_actual_client_session() {
    let directory = tempfile::tempdir_in(tickloop_agent_test_support::SHORT_TEMP_ROOT).unwrap();
    let factory = Factory::new(vec![]);
    let epoch = Uuid::new_v4();
    let daemon = Arc::new(DaemonServer::new(epoch, factory.clone()));
    let socket = directory.path().join("managed.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(serve_listener(
        listener,
        daemon,
        epoch,
        tickloop_agent_test_support::TRANSPORT_LIMITS,
    ));
    let client = LocalAgentClient::connect(&socket).await.unwrap();
    let token = ManagedToken::new();
    let result = execute(
        &client,
        epoch,
        AgentCommand::CreateManaged {
            launch: launch(directory.path(), ExecutionProfile::Acp),
            initial: None,
            session_id: Uuid::new_v4(),
            token: token.clone(),
        },
    )
    .await;
    assert!(result.is_err());
    assert!(factory.launches.lock().unwrap().is_empty());
    let CommandResult::Created { execution, .. } = execute(
        &client,
        epoch,
        AgentCommand::CreateManaged {
            launch: launch(directory.path(), ExecutionProfile::Acp),
            initial: None,
            session_id: client.session_id(),
            token: token.clone(),
        },
    )
    .await
    .unwrap() else {
        panic!("matching session must create")
    };
    execute(
        &client,
        epoch,
        AgentCommand::ManagedRelease { execution, token },
    )
    .await
    .unwrap();
    drop(client);
    server.abort();
    factory.shutdown().await;
}

#[tokio::test]
async fn unresponsive_worker_directory_does_not_prevent_managed_lease_cleanup() {
    let directory = tempfile::tempdir().unwrap();
    let factory = Factory::new(vec![]);
    let daemon = DaemonServer::new(Uuid::new_v4(), factory.clone());
    let token = ManagedToken::new();
    let execution = create(
        &daemon,
        daemon.daemon_epoch(),
        launch(directory.path(), ExecutionProfile::Acp),
        &token,
    )
    .await;
    let chat = snapshot(&daemon, execution).await.conversation.id;
    factory.block_directory.store(true, Ordering::SeqCst);
    tokio::time::timeout(WAIT, factory.directory_blocked.notified())
        .await
        .unwrap();
    // No heartbeat: exercise the production lease rather than a test-only expiry hook.
    tokio::time::timeout(Duration::from_secs(50), async {
        loop {
            if factory
                .cleanups
                .lock()
                .unwrap()
                .iter()
                .any(|(owner, _)| *owner == chat)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("directory observation must not stall lease revocation");
    assert_eq!(
        execute(
            &daemon,
            daemon.daemon_epoch(),
            AgentCommand::ManagedHeartbeat { execution, token }
        )
        .await
        .unwrap_err()
        .code,
        AccessErrorCode::AccessDenied
    );
    factory.block_directory.store(false, Ordering::SeqCst);
    factory.shutdown().await;
}
