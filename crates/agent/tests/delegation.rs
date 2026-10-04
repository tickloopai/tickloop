use serde_json::{Value, json};
use std::sync::Arc;
use tickloop_agent::permissions::ExecutionMode;
use tickloop_agent::runtime::{Agent, EventKind};

mod support;
use support::{ScriptedModel, call, config, text};

#[tokio::test]
async fn handles_use_short_description_fallback_and_reserve_parent_keywords() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "one",
            "DelegateAgent",
            json!({"task":"Audit the report in detail","description":"Audit revenue growth"}),
        ),
        text("one done"),
        call(
            "two",
            "DelegateAgent",
            json!({"task":"Audit another report","description":"Audit revenue growth"}),
        ),
        text("two done"),
        call(
            "three",
            "DelegateAgent",
            json!({"task":"Check metadata","name":"parent"}),
        ),
        text("three done"),
        text("parent done"),
    ]));
    let core = Agent::new_embedded(config(), model);
    let parent = core.create_chat(directory.path().into()).await.unwrap();
    let mut events = core.subscribe();
    let receipt = core
        .submit(parent, None, "Audit the reports".into())
        .await
        .unwrap();
    loop {
        let event = support::next_event(&mut events).await;
        if event.run_id == receipt.run_id && matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
    }
    let registry = core.subagents(parent).await.unwrap();
    assert_eq!(
        registry
            .iter()
            .map(|entry| entry.handle.as_str())
            .collect::<Vec<_>>(),
        ["audit-revenue-growth", "audit-revenue-growth-2", "parent-2"]
    );
    assert!(registry.iter().all(|entry| entry.handle.len() <= 32));
    core.shutdown().await;
}

#[tokio::test]
async fn invalid_handles_never_create_a_child_or_reserve_a_name() {
    for name in [
        json!(""),
        json!("line\nbreak"),
        json!("x".repeat(161)),
        json!(5),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let core = Agent::new_embedded(
            config(),
            Arc::new(ScriptedModel::new(vec![
                call(
                    "invalid",
                    "DelegateAgent",
                    json!({"task":"Check input","name":name}),
                ),
                text("invalid handled"),
            ])),
        );
        let parent = core.create_chat(directory.path().into()).await.unwrap();
        let mut events = core.subscribe();
        let receipt = core
            .submit(parent, None, "Check handles".into())
            .await
            .unwrap();
        let mut refused = false;
        loop {
            let event = support::next_event(&mut events).await;
            if event.run_id != receipt.run_id {
                continue;
            }
            match event.kind {
                EventKind::ToolCompleted { result, .. } => refused = result.is_error,
                EventKind::Completed { .. } => break,
                _ => {}
            }
        }
        assert!(refused);
        assert!(core.subagents(parent).await.unwrap().is_empty());
        assert_eq!(core.list_chats().await.len(), 1);
        core.shutdown().await;
    }
}

#[tokio::test]
async fn child_handles_are_reserved_in_the_parent_registry_across_completion_and_restart() {
    let directory = tempfile::tempdir().unwrap();
    let store = directory.path().join("state");
    let replies = vec![
        call(
            "first",
            "DelegateAgent",
            json!({"task":"Audit the first file","name":"Audit"}),
        ),
        text("first done"),
        call(
            "second",
            "DelegateAgent",
            json!({"task":"Audit the second file","name":"audit"}),
        ),
        text("second done"),
        text("parent done"),
    ];
    let core = Agent::open_embedded(
        store.clone(),
        config(),
        Arc::new(ScriptedModel::new(replies)),
    )
    .unwrap();
    let parent = core.create_chat(directory.path().into()).await.unwrap();
    let mut events = core.subscribe();
    let receipt = core
        .submit(parent, None, "Audit both files".into())
        .await
        .unwrap();
    loop {
        let event = support::next_event(&mut events).await;
        if event.run_id == receipt.run_id && matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
    }
    let registry = serde_json::to_value(core.subagents(parent).await.unwrap()).unwrap();
    assert_eq!(registry[0]["handle"], "audit");
    assert_eq!(registry[1]["handle"], "audit-2");
    assert!(registry[0]["created_at"].as_u64().is_some());
    core.shutdown().await;
    let restored = Agent::open_embedded(
        store,
        config(),
        Arc::new(ScriptedModel::new(vec![
            call(
                "third",
                "DelegateAgent",
                json!({"task":"Audit a third file","name":"audit"}),
            ),
            text("third done"),
            text("parent follow-up done"),
        ])),
    )
    .unwrap();
    restored.resume_chat(parent).await.unwrap();
    let mut events = restored.subscribe();
    let receipt = restored
        .submit(parent, Some(receipt.loop_id), "Audit the third file".into())
        .await
        .unwrap();
    loop {
        let event = support::next_event(&mut events).await;
        if event.run_id == receipt.run_id && matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
    }
    let registry = serde_json::to_value(restored.subagents(parent).await.unwrap()).unwrap();
    assert_eq!(
        registry[2]["handle"], "audit-3",
        "completed or hidden handles are never reused"
    );
    restored.shutdown().await;
}

struct ScopedHostSocket {
    path: std::path::PathBuf,
    requests: Arc<std::sync::Mutex<Vec<Value>>>,
    cancel: tokio_util::sync::CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl ScopedHostSocket {
    fn start(path: std::path::PathBuf) -> Self {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let cancel = tokio_util::sync::CancellationToken::new();
        let stopping = cancel.clone();
        let task = tokio::spawn(async move {
            loop {
                let stream = tokio::select! {
                    _ = stopping.cancelled() => break,
                    accepted = listener.accept() => accepted.unwrap().0,
                };
                let mut stream = BufReader::new(stream);
                let mut line = String::new();
                stream.read_line(&mut line).await.unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(request["method"], "pane.list");
                recorded.lock().unwrap().push(request.clone());
                let response = json!({"id":request["id"],"result":{"panes":[]}});
                let mut bytes = serde_json::to_vec(&response).unwrap();
                bytes.push(b'\n');
                stream.get_mut().write_all(&bytes).await.unwrap();
            }
        });
        Self {
            path,
            requests,
            cancel,
            task,
        }
    }

    fn install(&self, agent: &Agent) {
        agent
            .install_host_tools(Arc::new(
                tickloop_agent::native_host::NativeHostToolProvider::new(Arc::new(
                    tick_connect::access::TickAccess::new(
                        tick_connect::herdr::HerdrContext::without_view(self.path.clone()),
                    ),
                )),
            ))
            .unwrap();
    }

    async fn stop(self) {
        self.cancel.cancel();
        self.task.await.unwrap();
    }
}

async fn access_command(
    access: &tickloop_agent::agent_access::EmbeddedAgentAccess,
    command: tickloop_agent::agent_access::AgentCommand,
) -> tickloop_agent::agent_access::CommandResult {
    use tickloop_agent::agent_access::{AgentAccess, CommandEnvelope};
    let receipt = access
        .execute(CommandEnvelope {
            owner_epoch: access.owner_epoch(),
            command_id: uuid::Uuid::new_v4(),
            command,
        })
        .await
        .unwrap();
    assert!(receipt.error.is_none(), "{receipt:?}");
    receipt.result.unwrap()
}

#[tokio::test]
async fn loaded_delegated_host_authority_stays_with_its_original_socket_and_workspace() {
    use tickloop_agent::agent_access::{
        AgentCommand, CommandResult, ConfigurationChange, EmbeddedAgentAccess, HostLocation,
        HostWorkContext, InteractionSource, LaunchSpec, TranscriptRef, UserInput, ViewRef,
    };
    let directory = tempfile::Builder::new()
        .prefix("tl-host-")
        .tempdir_in("/tmp")
        .unwrap();
    let store = directory.path().join("state");
    let original = ScopedHostSocket::start(directory.path().join("h1.sock"));
    let other = ScopedHostSocket::start(directory.path().join("h2.sock"));
    let agent = Agent::open_embedded(
        store.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![
            call(
                "delegate",
                "DelegateAgent",
                json!({"task":"Read only the inherited Workspace."}),
            ),
            call("child-host", "ReadWorkspace", json!({})),
            text("The inherited host was read."),
            text("The child completed its task."),
        ])),
    )
    .unwrap();
    original.install(&agent);
    let access = EmbeddedAgentAccess::new(agent.clone());
    let mut launch = LaunchSpec::embedded(directory.path().into());
    launch.host_work_context = Some(HostWorkContext {
        socket: original.path.clone(),
        workspace: Some("W1".into()),
    });
    let CommandResult::Created {
        execution: parent, ..
    } = access_command(
        &access,
        AgentCommand::Create {
            launch,
            initial: None,
            origin: None,
        },
    )
    .await
    else {
        panic!("expected Create");
    };
    access_command(
        &access,
        AgentCommand::Configure {
            provider: None,
            execution: parent,
            change: ConfigurationChange::ExecutionMode {
                mode: ExecutionMode::FullAccess,
            },
        },
    )
    .await;
    let mut events = agent.subscribe();
    let CommandResult::Submitted(parent_run) = access_command(
        &access,
        AgentCommand::Submit {
            execution: parent,
            target_loop: None,
            input: UserInput::Text {
                text: "Delegate the scoped host read.".into(),
                contexts: vec![],
                paths: vec![],
            },
            origin: None,
        },
    )
    .await
    else {
        panic!("expected Submit");
    };
    let mut child = None;
    loop {
        let event = support::next_event(&mut events).await;
        if let EventKind::Failed(error) = &event.kind {
            panic!("initial delegation failed: {error}");
        }
        if event.chat_id != parent_run.chat_id {
            child = Some(event.chat_id);
            if let EventKind::ToolCompleted { result, .. } = &event.kind {
                assert!(!result.is_error, "{}", result.content);
            }
        }
        if event.run_id == parent_run.run_id && matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
    }
    let child = child.unwrap();
    access.shutdown().await;
    for (case, provider, workspace, should_read) in [
        ("same-host-other-workspace", &original, Some("W2"), true),
        ("same-host-headless-launch", &original, None, true),
        ("other-host-same-workspace", &other, Some("W1"), false),
        ("legacy-unknown-endpoint", &original, Some("W1"), false),
    ] {
        if case == "legacy-unknown-endpoint" {
            let transcript = support::transcript_file(&store, child);
            let rewritten = std::fs::read_to_string(&transcript)
                .unwrap()
                .lines()
                .map(|line| {
                    let mut entry: Value = serde_json::from_str(line).unwrap();
                    if entry["type"] == "delegation" {
                        entry["delegation"]["ceiling"]
                            .as_object_mut()
                            .unwrap()
                            .remove("host_endpoint");
                    }
                    serde_json::to_string(&entry).unwrap()
                })
                .collect::<Vec<_>>()
                .join("\n")
                + "\n";
            std::fs::write(transcript, rewritten).unwrap();
        }
        let restored = Agent::open_embedded(
            store.clone(),
            config(),
            Arc::new(ScriptedModel::new(vec![
                call("restored-host", "ReadWorkspace", json!({})),
                text("The scoped request finished."),
            ])),
        )
        .unwrap();
        provider.install(&restored);
        let access = EmbeddedAgentAccess::for_worker(restored.clone(), uuid::Uuid::new_v4());
        let origin = (case == "other-host-same-workspace").then(|| {
            use std::os::unix::fs::MetadataExt;
            let metadata = std::fs::symlink_metadata(&other.path).unwrap();
            InteractionSource {
                view: ViewRef {
                    session_id: uuid::Uuid::new_v4(),
                    view_id: uuid::Uuid::new_v4(),
                },
                display_version: 1,
                host: Some(HostLocation {
                    host_instance: format!("herdr:{}:{}", metadata.dev(), metadata.ino()),
                    socket: other.path.clone(),
                    workspace: Some("W1".into()),
                    tab: Some("new-view-tab".into()),
                    pane: Some("new-view-pane".into()),
                    terminal: Some("new-view-terminal".into()),
                    client: None,
                }),
            }
        });
        let mut launch = LaunchSpec::embedded(directory.path().into());
        if let Some(workspace) = workspace {
            launch.host_work_context = Some(HostWorkContext {
                socket: provider.path.clone(),
                workspace: Some(workspace.into()),
            });
        }
        let CommandResult::Loaded {
            execution,
            snapshot,
        } = access_command(
            &access,
            AgentCommand::LoadRecord {
                record: TranscriptRef {
                    store_root: store.clone(),
                    chat_id: child,
                },
                as_new: false,
                launch: Some(launch),
            },
        )
        .await
        else {
            panic!("expected LoadRecord");
        };
        let mut events = restored.subscribe();
        let CommandResult::Submitted(run) = access_command(
            &access,
            AgentCommand::Submit {
                execution,
                target_loop: Some(snapshot.conversation.loops[0].id),
                input: UserInput::Text {
                    text: "Read the original host scope.".into(),
                    contexts: vec![],
                    paths: vec![],
                },
                origin,
            },
        )
        .await
        else {
            panic!("expected Submit");
        };
        let mut result_seen = false;
        loop {
            let event = support::next_event(&mut events).await;
            if event.run_id != run.run_id {
                continue;
            }
            if let EventKind::Failed(error) = &event.kind {
                panic!("{case}: {error}");
            }
            if let EventKind::ToolCompleted { result, .. } = &event.kind {
                result_seen = true;
                assert_eq!(!result.is_error, should_read, "{case}: {}", result.content);
                if should_read {
                    let value: Value = serde_json::from_str(&result.content).unwrap();
                    assert_eq!(value["workspace_id"], "W1", "{case}");
                    assert_eq!(value["host_instance"], json!(original.path), "{case}");
                    assert!(value["caller"].is_null());
                } else {
                    assert!(
                        result.content.contains("Unsupported"),
                        "{case}: {}",
                        result.content
                    );
                }
            }
            if matches!(event.kind, EventKind::Completed { .. }) {
                break;
            }
        }
        assert!(result_seen, "{case}");
        assert!(
            other.requests.lock().unwrap().is_empty(),
            "the different host was contacted in {case}"
        );
        access.shutdown().await;
    }
    assert!(
        original
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| request["params"]["workspace_id"] == "W1")
    );
    original.stop().await;
    other.stop().await;
}

#[tokio::test]
async fn replacing_the_socket_instance_blocks_a_prepared_native_call_before_dispatch() {
    use tickloop_agent::host_tools::{HostCallContext, HostCallResult, HostToolProvider};
    let directory = tempfile::Builder::new()
        .prefix("tl-host-")
        .tempdir_in("/tmp")
        .unwrap();
    let original = ScopedHostSocket::start(directory.path().join("host.sock"));
    let provider = tickloop_agent::native_host::NativeHostToolProvider::new(Arc::new(
        tick_connect::access::TickAccess::new(tick_connect::herdr::HerdrContext::without_view(
            original.path.clone(),
        )),
    ));
    let call = provider
        .prepare_call_in_context(
            uuid::Uuid::new_v4(),
            &HostCallContext {
                workdir: directory.path().into(),
                workspace_id: Some("W1".into()),
                interaction: None,
            },
            "ReadWorkspace",
            &json!({}),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    std::fs::remove_file(&original.path).unwrap();
    let replacement = ScopedHostSocket::start(original.path.clone());
    let result = provider
        .execute_prepared(
            uuid::Uuid::new_v4(),
            call,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
    assert!(
        matches!(result, HostCallResult::NotDispatched(message) if message.contains("endpoint changed"))
    );
    assert!(original.requests.lock().unwrap().is_empty());
    assert!(replacement.requests.lock().unwrap().is_empty());
    original.stop().await;
    replacement.stop().await;
}

#[tokio::test]
async fn delegated_full_access_keeps_the_parent_session_denial_after_restore() {
    use tickloop_agent::permissions::PermissionScope;
    let directory = tempfile::tempdir().unwrap();
    let store = directory.path().join("state");
    let denied_input = json!({"file_path":".bashrc","content":"fixture=denied"});
    let model = Arc::new(ScriptedModel::new(vec![
        call("parent-denied", "Write", denied_input.clone()),
        call(
            "delegate",
            "DelegateAgent",
            json!({"task":"Try the same scoped write."}),
        ),
        call("child-denied", "Write", denied_input.clone()),
        text("The child respected the Session denial."),
        text("The delegated check completed."),
    ]));
    let agent = Agent::open_embedded(store.clone(), config(), model).unwrap();
    let parent = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(parent, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let denied = agent
        .submit(parent, None, "Try the scoped write.".into())
        .await
        .unwrap();
    loop {
        let event = support::next_event(&mut events).await;
        if let EventKind::PermissionRequested { request } = event.kind {
            agent
                .answer_permission_with_scope(parent, request.id, false, PermissionScope::Session)
                .await
                .unwrap();
            break;
        }
    }
    loop {
        if matches!(
            support::next_event(&mut events).await.kind,
            EventKind::Interrupted
        ) {
            break;
        }
    }
    agent
        .set_execution_mode(parent, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let delegation = agent
        .submit(
            parent,
            Some(denied.loop_id),
            "Delegate the denied write.".into(),
        )
        .await
        .unwrap();
    let mut child = None;
    let mut denial_returned = false;
    loop {
        let event = support::next_event(&mut events).await;
        if event.chat_id != parent {
            child = Some(event.chat_id);
            if let EventKind::ToolCompleted { call_id, result } = &event.kind
                && call_id == "child-denied"
            {
                assert!(
                    result.is_error,
                    "the parent's Session denial was lost: {}",
                    result.content
                );
                denial_returned = true;
            }
        }
        if event.run_id == delegation.run_id && matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
    }
    assert!(denial_returned);
    assert!(!directory.path().join(".bashrc").exists());
    let child = child.unwrap();
    let metadata = agent.snapshot(child).await.unwrap().delegation.unwrap();
    assert!(
        !serde_json::to_string(&metadata.ceiling.session_denials)
            .unwrap()
            .contains("fixture=denied")
    );
    agent.shutdown().await;
    let restored = Agent::open_embedded(
        store,
        config(),
        Arc::new(ScriptedModel::new(vec![
            call("restored-denied", "Write", denied_input),
            text("The restored child kept the denial."),
        ])),
    )
    .unwrap();
    let snapshot = restored.resume_chat(child).await.unwrap();
    assert_eq!(snapshot.policy.mode, ExecutionMode::FullAccess);
    assert_eq!(
        snapshot.delegation.unwrap().delegation_id,
        metadata.delegation_id
    );
    let mut events = restored.subscribe();
    restored
        .submit(
            child,
            Some(snapshot.loops[0].id),
            "Try the same write after restore.".into(),
        )
        .await
        .unwrap();
    let mut denied = false;
    loop {
        let event = support::next_event(&mut events).await;
        if let EventKind::ToolCompleted { result, .. } = event.kind {
            assert!(result.is_error);
            denied = true;
        } else if matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
    }
    assert!(denied);
    assert!(!directory.path().join(".bashrc").exists());
    restored.shutdown().await;
}

#[tokio::test]
async fn a_new_parent_session_denial_blocks_the_childs_final_admission() {
    use tickloop_agent::permissions::PermissionScope;
    let directory = tempfile::tempdir().unwrap();
    let input = json!({"file_path":".bashrc","content":"fixture=denied"});
    let store = directory.path().join("state");
    let agent = Agent::open_embedded(
        store.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![
            call(
                "delegate",
                "DelegateAgent",
                json!({"task":"Prepare the scoped write for approval."}),
            ),
            call("child-write", "Write", input.clone()),
            call("parent-denial", "Write", input.clone()),
            text("The child operation was blocked."),
        ])),
    )
    .unwrap();
    let parent = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(parent, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let delegation = agent
        .submit(parent, None, "Delegate the write.".into())
        .await
        .unwrap();
    let (child, child_request) = loop {
        let event = support::next_event(&mut events).await;
        if let EventKind::PermissionRequested { request } = event.kind {
            assert_ne!(event.chat_id, parent);
            break (event.chat_id, request);
        }
    };
    agent.interrupt(parent).await.unwrap();
    loop {
        let event = support::next_event(&mut events).await;
        if event.chat_id == parent && matches!(event.kind, EventKind::Interrupted) {
            break;
        }
    }
    let denial = agent
        .submit(
            parent,
            Some(delegation.loop_id),
            "Deny the same write for this session.".into(),
        )
        .await
        .unwrap();
    loop {
        let event = support::next_event(&mut events).await;
        if event.run_id == denial.run_id
            && let EventKind::PermissionRequested { request } = event.kind
        {
            agent
                .answer_permission_with_scope(parent, request.id, false, PermissionScope::Session)
                .await
                .unwrap();
            break;
        }
    }
    agent
        .answer_permission(child, child_request.id, true)
        .await
        .unwrap();
    loop {
        let event = support::next_event(&mut events).await;
        if event.chat_id == child {
            assert!(
                !matches!(event.kind, EventKind::ToolStarted { .. }),
                "a later parent Session denial was bypassed at final admission"
            );
            if matches!(
                event.kind,
                EventKind::Completed { .. } | EventKind::Failed(_) | EventKind::Interrupted
            ) {
                break;
            }
        }
    }
    assert!(!directory.path().join(".bashrc").exists());
    agent.shutdown().await;
    let restored = Agent::open_embedded(
        store,
        config(),
        Arc::new(ScriptedModel::new(vec![
            call("restored-current-denial", "Write", input),
            text("The later parent denial remains in effect."),
        ])),
    )
    .unwrap();
    let snapshot = restored.resume_chat(child).await.unwrap();
    assert_eq!(
        snapshot
            .delegation
            .unwrap()
            .ceiling
            .session_denials
            .unwrap()
            .len(),
        1
    );
    let mut events = restored.subscribe();
    restored
        .submit(
            child,
            Some(snapshot.loops[0].id),
            "Try the write after restoring.".into(),
        )
        .await
        .unwrap();
    let mut denied = false;
    loop {
        let event = support::next_event(&mut events).await;
        assert!(!matches!(
            event.kind,
            EventKind::PermissionRequested { .. } | EventKind::ToolStarted { .. }
        ));
        if let EventKind::ToolCompleted { result, .. } = event.kind {
            assert!(result.is_error);
            denied = true;
        } else if matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
    }
    assert!(denied);
    assert!(!directory.path().join(".bashrc").exists());
    restored.shutdown().await;
}

#[tokio::test]
async fn model_delegates_a_real_child_and_observes_its_result() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("task.txt"), "verified child evidence").unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "delegate",
            "DelegateAgent",
            json!({"task":"Read task.txt and report the evidence.","name":" File CHECK ","description":"Check file evidence"}),
        ),
        call("read", "Read", json!({"file_path":"task.txt"})),
        text("verified child evidence"),
        text("The child read the evidence."),
        text("The child accepts follow-up input."),
    ]));
    let store = directory.path().join("state");
    let agent = Agent::open_embedded(store.clone(), config(), model.clone()).unwrap();
    let parent = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(parent, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let parent_receipt = agent
        .submit(parent, None, "Delegate the file check.".into())
        .await
        .unwrap();
    let result = loop {
        let event = support::next_event(&mut events).await;
        if event.chat_id == parent
            && let EventKind::ToolCompleted { call_id, result } = event.kind
            && call_id == "delegate"
        {
            break result;
        }
    };
    assert!(!result.is_error, "{}", result.content);
    let result: Value = serde_json::from_str(&result.content).unwrap();
    assert_eq!(result["status"], "completed");
    assert_eq!(result["result"], "verified child evidence");
    let child = serde_json::from_value(result["child_chat_id"].clone()).unwrap();
    assert_ne!(parent, child);
    let child_snapshot = agent.snapshot(child).await.unwrap();
    assert!(child_snapshot.branch_origin.is_none());
    assert_eq!(
        child_snapshot.workdir,
        directory.path().canonicalize().unwrap()
    );
    assert_eq!(child_snapshot.loops.len(), 1);
    assert_ne!(child_snapshot.loops[0].id, parent_receipt.loop_id);
    let metadata = child_snapshot.delegation.clone().unwrap();
    let registration = agent.subagent_registration(child).await.unwrap().unwrap();
    assert_eq!(registration.handle, "file-check");
    assert_eq!(registration.description, "Check file evidence");
    assert_eq!(metadata.parent_chat_id, parent);
    assert_eq!(metadata.parent_loop_id, parent_receipt.loop_id);
    assert_eq!(metadata.parent_run_id, parent_receipt.run_id);
    assert_eq!(metadata.parent_call_id, "delegate");
    loop {
        let event = support::next_event(&mut events).await;
        if event.chat_id == parent && matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
    }
    agent
        .submit(
            child,
            Some(child_snapshot.loops[0].id),
            "Continue with the same scope.".into(),
        )
        .await
        .unwrap();
    loop {
        let event = support::next_event(&mut events).await;
        if event.chat_id == child && matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
    }
    {
        let requests = model.requests.lock().unwrap();
        assert!(
            requests[0]
                .tools
                .iter()
                .any(|tool| tool.name == "DelegateAgent")
        );
        assert!(
            !requests[1]
                .tools
                .iter()
                .any(|tool| tool.name == "DelegateAgent")
        );
    }
    agent.shutdown().await;
    let transcript = std::fs::read_to_string(support::transcript_file(&store, child)).unwrap();
    assert!(transcript.contains("\"type\":\"delegation\""));
    assert!(transcript.contains("verified child evidence"));
    let restored =
        Agent::open_embedded(store, config(), Arc::new(ScriptedModel::new(vec![]))).unwrap();
    let child_snapshot = restored.resume_chat(child).await.unwrap();
    let restored_metadata = child_snapshot.delegation.unwrap();
    assert_eq!(restored_metadata.delegation_id, metadata.delegation_id);
    assert_eq!(
        restored
            .subagent_registration(child)
            .await
            .unwrap()
            .unwrap()
            .handle,
        "file-check"
    );
    assert_eq!(child_snapshot.policy.mode, ExecutionMode::FullAccess);
    assert!(
        restored
            .set_directories(child, directory.path().into(), vec![])
            .await
            .is_err()
    );
    restored.shutdown().await;
}

#[tokio::test]
async fn child_follow_up_and_direct_shell_cannot_expand_the_frozen_ceiling() {
    use tickloop_agent::permissions::{Decision, PermissionRule};
    use tickloop_agent::runtime::UserShellState;
    let directory = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let outside_file = outside.path().join("private.txt");
    std::fs::write(&outside_file, "outside evidence").unwrap();
    std::fs::write(directory.path().join("scope.txt"), "scoped evidence").unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "delegate",
            "DelegateAgent",
            json!({"task":"Check the inherited task boundary."}),
        ),
        call("outside", "Read", json!({"file_path":outside_file})),
        call(
            "escape",
            "Bash",
            json!({"command":"python3 -c 'print(123)'"}),
        ),
        call(
            "denied",
            "Write",
            json!({"file_path":"denied.txt","content":"must not write"}),
        ),
        text("The child honored the scope."),
        text("The scope check completed."),
        call("new-restriction", "Read", json!({"file_path":"scope.txt"})),
        text("The new parent approval restriction was honored."),
    ]));
    let agent = Agent::new_embedded(config(), model);
    let parent = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(parent, ExecutionMode::FullAccess)
        .await
        .unwrap();
    agent
        .set_permission_rules(
            parent,
            vec![PermissionRule {
                id: uuid::Uuid::new_v4(),
                decision: Decision::Deny,
                tool: "Write".into(),
                specifier: None,
                input: None,
                loop_id: None,
                persistent: false,
            }],
        )
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(parent, None, "Delegate the bounded check.".into())
        .await
        .unwrap();
    let mut child = None;
    let mut refused = std::collections::BTreeSet::new();
    loop {
        let event = support::next_event(&mut events).await;
        if event.chat_id != parent {
            child = Some(event.chat_id);
        }
        if event.chat_id != parent
            && let EventKind::ToolCompleted { call_id, result } = &event.kind
        {
            assert!(result.is_error, "{call_id}: {}", result.content);
            refused.insert(call_id.clone());
        }
        if event.chat_id == parent && matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
    }
    let child = child.unwrap();
    assert_eq!(
        refused,
        ["outside", "escape", "denied"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    );
    assert!(!directory.path().join("denied.txt").exists());
    assert!(
        agent
            .set_directories(child, outside.path().into(), vec![])
            .await
            .is_err()
    );
    assert!(agent.set_permission_rules(child, vec![]).await.is_err());
    assert!(
        agent
            .set_execution_mode(child, ExecutionMode::FullAccess)
            .await
            .is_err()
    );
    assert!(agent.compact(child).await.is_err());
    agent
        .set_managed_permission_constraints(
            parent,
            vec![PermissionRule {
                id: uuid::Uuid::new_v4(),
                decision: Decision::Ask,
                tool: "Read".into(),
                specifier: None,
                input: None,
                loop_id: None,
                persistent: false,
            }],
        )
        .await
        .unwrap();
    let child_loop = agent.snapshot(child).await.unwrap().loops[0].id;
    let follow_up = agent
        .submit(child, Some(child_loop), "Check scope.txt again.".into())
        .await
        .unwrap();
    let mut newly_restricted = false;
    loop {
        let event = support::next_event(&mut events).await;
        if event.run_id != follow_up.run_id {
            continue;
        }
        if let EventKind::ToolCompleted { call_id, result } = &event.kind
            && call_id == "new-restriction"
        {
            assert!(result.is_error);
            assert!(result.content.contains("current approval restriction"));
            newly_restricted = true;
        }
        if matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
    }
    assert!(newly_restricted);
    for command in [
        format!("cat {}", outside_file.display()),
        "python3 -c 'print(123)'".into(),
    ] {
        let error = agent
            .submit_user_shell_for_goal(child, None, command, false)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Unsupported"));
    }
    let shell = agent
        .submit_user_shell_for_goal(child, None, "touch direct.txt".into(), false)
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if agent
                .snapshot(child)
                .await
                .unwrap()
                .user_shells
                .iter()
                .any(|record| {
                    record.id == shell.command_id && record.state == UserShellState::Completed
                })
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(directory.path().join("direct.txt").exists());
    agent.shutdown().await;
}

#[tokio::test]
async fn permission_and_question_requests_belong_to_the_child() {
    use tickloop_agent::permissions::{Decision, PermissionRule};
    use tickloop_agent::runtime::QuestionAnswer;
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "delegate",
            "DelegateAgent",
            json!({"task":"Write the approved report and ask its cadence."}),
        ),
        call(
            "write",
            "Write",
            json!({"file_path":"report.txt","content":"approved child report"}),
        ),
        call(
            "question",
            "AskUserQuestion",
            support::single_question("Which cadence?"),
        ),
        text("The approved weekly report is ready."),
        text("The child returned the approved report."),
    ]));
    let agent = Agent::new_embedded(config(), model);
    let parent = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(parent, ExecutionMode::FullAccess)
        .await
        .unwrap();
    agent
        .set_permission_rules(
            parent,
            vec![PermissionRule {
                id: uuid::Uuid::new_v4(),
                decision: Decision::Ask,
                tool: "Write".into(),
                specifier: None,
                input: None,
                loop_id: None,
                persistent: false,
            }],
        )
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(parent, None, "Delegate the report.".into())
        .await
        .unwrap();
    let mut child = None;
    let mut asked = false;
    loop {
        let event = support::next_event(&mut events).await;
        match event.kind {
            EventKind::PermissionRequested { request } => {
                assert_ne!(event.chat_id, parent);
                child = Some(event.chat_id);
                assert_eq!(request.run_id, event.run_id);
                assert!(
                    agent
                        .answer_permission(parent, request.id, true)
                        .await
                        .is_err()
                );
                agent
                    .answer_permission(event.chat_id, request.id, true)
                    .await
                    .unwrap();
            }
            EventKind::QuestionAsked { request } => {
                assert_eq!(Some(event.chat_id), child);
                assert_eq!(request.chat_id, event.chat_id);
                assert!(
                    agent
                        .answer_question(parent, request.id, vec![QuestionAnswer::option("Weekly")])
                        .await
                        .is_err()
                );
                agent
                    .answer_question(
                        event.chat_id,
                        request.id,
                        vec![QuestionAnswer::option("Weekly")],
                    )
                    .await
                    .unwrap();
                asked = true;
            }
            EventKind::Completed { .. } if event.chat_id == parent => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert!(asked);
    assert_eq!(
        std::fs::read_to_string(directory.path().join("report.txt")).unwrap(),
        "approved child report"
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn console_opens_the_real_child_in_place_and_routes_follow_up_with_its_ceiling() {
    use tickloop_agent::agent_access::{
        AgentAccess, AgentCommand, AgentDetailQuery, AgentFilter, CommandEnvelope, CommandResult,
        ConfigurationChange, DisplayUpdate, EmbeddedAgentAccess, HostLocation, LaunchSpec,
        ObservationRequest, ObservationValue, Page, RequestKind, RequestRef, UserInput, ViewRef,
    };
    use tickloop_agent::agent_console::{AgentConsole, Destination, Intent};
    use tickloop_agent::runtime::{LoopState, QuestionAnswer};
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "delegate",
            "DelegateAgent",
            json!({"task":"Ask the user for the report cadence."}),
        ),
        call(
            "question",
            "AskUserQuestion",
            support::single_question("Which cadence?"),
        ),
        text("The child confirmed Weekly."),
        text("The actual child completed."),
        text("The visible child accepted the follow-up in the original Loop."),
    ]));
    let runtime = Agent::new_embedded(config(), model);
    let access = EmbeddedAgentAccess::new(runtime.clone());
    let receipt = access
        .execute(CommandEnvelope {
            owner_epoch: access.owner_epoch(),
            command_id: uuid::Uuid::new_v4(),
            command: AgentCommand::Create {
                launch: LaunchSpec::embedded(directory.path().into()),
                initial: None,
                origin: None,
            },
        })
        .await
        .unwrap();
    let Some(CommandResult::Created {
        execution: parent, ..
    }) = receipt.result
    else {
        panic!("{receipt:?}");
    };
    for command in [
        AgentCommand::Configure {
            provider: None,
            execution: parent,
            change: ConfigurationChange::ExecutionMode {
                mode: ExecutionMode::FullAccess,
            },
        },
        AgentCommand::Submit {
            execution: parent,
            target_loop: None,
            input: UserInput::Text {
                text: "Delegate the cadence question.".into(),
                contexts: vec![],
                paths: vec![],
            },
            origin: None,
        },
    ] {
        let receipt = access
            .execute(CommandEnvelope {
                owner_epoch: access.owner_epoch(),
                command_id: uuid::Uuid::new_v4(),
                command,
            })
            .await
            .unwrap();
        assert!(receipt.error.is_none(), "{receipt:?}");
    }
    let child = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let observation = access
                .observe(ObservationRequest::Overview {
                    filter: AgentFilter::default(),
                    page: Page::default(),
                    after: None,
                })
                .await
                .unwrap();
            if let ObservationValue::Overview(entries) = observation.initial
                && let Some(child) = entries
                    .into_iter()
                    .find(|entry| entry.parent == Some(parent) && entry.waiting_questions == 1)
            {
                break child;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_ne!(child.execution, parent);
    assert!(child.capabilities.contains(&"scoped_shell".into()));
    assert!(!child.capabilities.contains(&"history".into()));
    let observation = access
        .observe(ObservationRequest::Agent {
            execution: child.execution,
            detail: AgentDetailQuery::Snapshot,
            after: None,
        })
        .await
        .unwrap();
    let ObservationValue::Agent(snapshot) = observation.initial else {
        panic!("expected child snapshot");
    };
    let child_loop = snapshot.conversation.loops[0].id;
    let metadata = snapshot.conversation.delegation.clone().unwrap();
    let host = HostLocation {
        host_instance: "controlled-console-host".into(),
        socket: directory.path().join("console-host.sock"),
        workspace: Some("workspace".into()),
        tab: Some("other-tab".into()),
        pane: Some("external-child-pane".into()),
        terminal: Some("external-child-terminal".into()),
        client: Some("console-client".into()),
    };
    let display = access
        .show_view(DisplayUpdate::Displayed {
            view: ViewRef {
                session_id: uuid::Uuid::new_v4(),
                view_id: uuid::Uuid::new_v4(),
            },
            expected_version: 0,
            execution: child.execution,
            host: Some(host.clone()),
        })
        .await
        .unwrap();
    assert!(display.displayed);
    let mut console = AgentConsole::new(
        Arc::new(access.clone()),
        access.owner_epoch(),
        directory.path().into(),
        Some(HostLocation {
            tab: Some("current-tab".into()),
            pane: Some("current-parent-pane".into()),
            terminal: Some("current-parent-terminal".into()),
            ..host
        }),
    );
    console.visible_ready(Some(parent));
    let projection = console.refresh().await.unwrap();
    assert_eq!(projection.waiting_questions, 1);
    let family = console.family(parent);
    assert_eq!(family.len(), 2);
    let child_row = family
        .iter()
        .find(|row| row.execution == child.execution)
        .unwrap();
    assert_eq!(child_row.parent, Some(parent));
    assert_eq!(child_row.views.len(), 1);
    let effect = console
        .interpret(Intent::Open(child.execution))
        .unwrap()
        .unwrap();
    assert!(matches!(effect.destination, Destination::Present));
    assert!(console.revalidate(&effect).await.unwrap());
    assert!(console.complete(&effect));
    assert_eq!(console.projection().visible, Some(child.execution));
    let child_family = console.family(child.execution);
    assert_eq!(child_family.len(), 2);
    assert!(child_family.iter().any(|row| row.execution == parent));
    let request = RequestRef {
        execution: child.execution,
        request_id: snapshot.questions[0].id,
        kind: RequestKind::Question,
        version: 1,
    };
    let wrong = access
        .execute(CommandEnvelope {
            owner_epoch: access.owner_epoch(),
            command_id: uuid::Uuid::new_v4(),
            command: AgentCommand::AnswerQuestion {
                request: RequestRef {
                    execution: parent,
                    ..request
                },
                answers: vec![QuestionAnswer::option("Weekly")],
            },
        })
        .await
        .unwrap();
    assert!(wrong.error.is_some());
    let shell = access
        .execute(CommandEnvelope {
            owner_epoch: access.owner_epoch(),
            command_id: uuid::Uuid::new_v4(),
            command: AgentCommand::Submit {
                execution: child.execution,
                target_loop: None,
                input: UserInput::Shell {
                    command: "python3 -c 'print(123)'".into(),
                    background: false,
                },
                origin: None,
            },
        })
        .await
        .unwrap();
    assert!(shell.error.unwrap().message.contains("Unsupported"));
    let answered = access
        .execute(CommandEnvelope {
            owner_epoch: access.owner_epoch(),
            command_id: uuid::Uuid::new_v4(),
            command: AgentCommand::AnswerQuestion {
                request,
                answers: vec![QuestionAnswer::option("Weekly")],
            },
        })
        .await
        .unwrap();
    assert!(answered.error.is_none(), "{answered:?}");
    let parent_snapshot = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let observation = access
                .observe(ObservationRequest::Agent {
                    execution: parent,
                    detail: AgentDetailQuery::Snapshot,
                    after: None,
                })
                .await
                .unwrap();
            if let ObservationValue::Agent(snapshot) = observation.initial
                && snapshot.conversation.loops[0].state == LoopState::Completed
            {
                break snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let parent_result = parent_snapshot
        .conversation
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|block| match block {
            tickloop_agent::model::Content::ToolResult {
                tool_use_id,
                content,
                is_error: false,
            } if tool_use_id == "delegate" => {
                Some(serde_json::from_str::<Value>(&content.text()).unwrap())
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(parent_result["result"], "The child confirmed Weekly.");
    assert_eq!(parent_result["status"], "completed");
    assert_eq!(
        parent_result["delegation_id"],
        serde_json::to_value(metadata.delegation_id).unwrap()
    );
    let original_child_run: uuid::Uuid =
        serde_json::from_value(parent_result["child_run_id"].clone()).unwrap();
    let follow_up = access
        .execute(CommandEnvelope {
            owner_epoch: access.owner_epoch(),
            command_id: uuid::Uuid::new_v4(),
            command: AgentCommand::Submit {
                execution: console.projection().visible.unwrap(),
                target_loop: Some(child_loop),
                input: UserInput::Text {
                    text: "Continue the same bounded task.".into(),
                    contexts: vec![],
                    paths: vec![],
                },
                origin: None,
            },
        })
        .await
        .unwrap();
    assert!(follow_up.error.is_none(), "{follow_up:?}");
    let Some(CommandResult::Submitted(follow_up)) = follow_up.result else {
        panic!("expected actual child submission");
    };
    assert_eq!(follow_up.loop_id, child_loop);
    assert_ne!(follow_up.run_id, original_child_run);
    let child_snapshot = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let observation = access
                .observe(ObservationRequest::Agent {
                    execution: child.execution,
                    detail: AgentDetailQuery::Snapshot,
                    after: None,
                })
                .await
                .unwrap();
            if let ObservationValue::Agent(snapshot) = observation.initial
                && snapshot.events.iter().any(|event| {
                    event.run_id == follow_up.run_id
                        && matches!(event.kind, EventKind::Completed { .. })
                })
            {
                break snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(child_snapshot.conversation.loops.len(), 1);
    assert_eq!(
        serde_json::to_value(child_snapshot.conversation.delegation.as_ref().unwrap()).unwrap(),
        serde_json::to_value(&metadata).unwrap()
    );
    let forbidden = access
        .execute(CommandEnvelope {
            owner_epoch: access.owner_epoch(),
            command_id: uuid::Uuid::new_v4(),
            command: AgentCommand::Configure {
                provider: None,
                execution: console.projection().visible.unwrap(),
                change: ConfigurationChange::ReplaceRules { rules: vec![] },
            },
        })
        .await
        .unwrap();
    assert!(forbidden.error.unwrap().message.contains("Unsupported"));
    console.refresh().await.unwrap();
    assert_eq!(console.family(parent).len(), 2);
    runtime.shutdown().await;
}

#[tokio::test]
async fn interrupting_the_parent_wait_does_not_stop_or_redelegate_the_child() {
    use tickloop_agent::runtime::{LoopState, QuestionAnswer};
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "delegate",
            "DelegateAgent",
            json!({"task":"Ask which cadence and continue independently."}),
        ),
        call(
            "question",
            "AskUserQuestion",
            support::single_question("Which cadence?"),
        ),
        text("The child completed after the parent stopped waiting."),
    ]));
    let agent = Agent::new_embedded(config(), model);
    let parent = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(parent, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let receipt = agent
        .submit(parent, None, "Delegate and wait.".into())
        .await
        .unwrap();
    let (child, request) = loop {
        let event = support::next_event(&mut events).await;
        if let EventKind::QuestionAsked { request } = event.kind {
            break (event.chat_id, request);
        }
    };
    agent.interrupt(parent).await.unwrap();
    loop {
        let event = support::next_event(&mut events).await;
        if event.chat_id == parent && matches!(event.kind, EventKind::Interrupted) {
            break;
        }
    }
    assert_eq!(
        agent
            .snapshot(parent)
            .await
            .unwrap()
            .loops
            .iter()
            .find(|goal| goal.id == receipt.loop_id)
            .unwrap()
            .state,
        LoopState::Interrupted
    );
    assert_eq!(
        agent.snapshot(child).await.unwrap().loops[0].state,
        LoopState::WaitingQuestion
    );
    agent
        .answer_question(child, request.id, vec![QuestionAnswer::option("Weekly")])
        .await
        .unwrap();
    loop {
        let event = support::next_event(&mut events).await;
        if event.chat_id == child && matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
    }
    assert_eq!(agent.list_chats().await.len(), 2);
    assert_eq!(
        agent.snapshot(child).await.unwrap().loops[0].state,
        LoopState::Completed
    );
    agent.shutdown().await;
}
