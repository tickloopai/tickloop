use serde_json::{Value, json};
use std::sync::Arc;
use tickloop_agent_runtime::Agent;
use tickloop_agent_settings::local_agent::LocalAgent;

use tickloop_agent_protocol::{EventKind, ExecutionMode};
use tickloop_agent_test_support::{ScriptedModel, call, config, text};

#[tokio::test]
async fn handles_use_short_description_fallback_and_reserve_parent_keywords() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "one",
            "Subagent",
            json!({"task":"Audit the report in detail","description":"Audit revenue growth"}),
        ),
        text("one done"),
        call(
            "two",
            "Subagent",
            json!({"task":"Audit another report","description":"Audit revenue growth"}),
        ),
        text("two done"),
        call(
            "three",
            "Subagent",
            json!({"task":"Check metadata","name":"parent"}),
        ),
        text("three done"),
        text("parent done"),
    ]));
    let core = Agent::new_embedded(config(), model);
    let parent = core.create_chat(directory.path().into()).await.unwrap();
    let mut events = core.subscribe();
    let receipt = core
        .submit(parent, "Audit the reports".into())
        .await
        .unwrap();
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
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
                    "Subagent",
                    json!({"task":"Check input","name":name}),
                ),
                text("invalid handled"),
            ])),
        );
        let parent = core.create_chat(directory.path().into()).await.unwrap();
        let mut events = core.subscribe();
        let receipt = core.submit(parent, "Check handles".into()).await.unwrap();
        let mut refused = false;
        loop {
            let event = tickloop_agent_test_support::next_event(&mut events).await;
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
            "Subagent",
            json!({"task":"Audit the first file","name":"Audit"}),
        ),
        text("first done"),
        call(
            "second",
            "Subagent",
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
        .submit(parent, "Audit both files".into())
        .await
        .unwrap();
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
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
                "Subagent",
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
        .submit(parent, "Audit the third file".into())
        .await
        .unwrap();
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
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

    /// The Tick tools of a worker in the Herdr View this socket serves, with a daemon that runs
    /// no Ticks for it.
    fn tools(&self) -> tickloop_agent_runtime::hosted::tools::HostedTickTools {
        self.tools_behind(&self.path.with_extension("daemon.sock"))
    }

    /// The same tools behind the daemon at `daemon`.
    fn tools_behind(
        &self,
        daemon: &std::path::Path,
    ) -> tickloop_agent_runtime::hosted::tools::HostedTickTools {
        tick_tools(daemon).with_herdr_views(tickloop_agent_runtime::hosted::views::HerdrViews::new(
            tickhost_connect::layout::HerdrLayout::new(
                tickhost_connect::herdr::HerdrContext::without_view(self.path.clone()),
            ),
            std::path::Path::new("/tmp/unused/tickloop"),
        ))
    }

    fn install(&self, agent: &Agent) {
        agent.install_host_tools(Arc::new(self.tools())).unwrap();
    }

    async fn stop(self) {
        self.cancel.cancel();
        self.task.await.unwrap();
    }
}

/// Tick tools behind the daemon at `daemon`, which answers every request with a listing of no
/// Ticks.
fn tick_tools(daemon: &std::path::Path) -> tickloop_agent_runtime::hosted::tools::HostedTickTools {
    use tickhost_connect::hosted::{BUSINESS_FRAME_BYTES, read_frame, write_frame};
    use tickloop_agent_runtime::hosted::{
        tools::HostedTickTools,
        worker_control::{DaemonControl, WorkerControlGrant},
    };
    if !daemon.exists() {
        let listener = tokio::net::UnixListener::bind(daemon).unwrap();
        let host_endpoint = daemon.with_extension("host.sock");
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (reader, mut writer) = stream.into_split();
                let mut reader = tokio::io::BufReader::new(reader);
                if let Ok(Some(request)) = read_frame(&mut reader, BUSINESS_FRAME_BYTES).await {
                    let page = json!({"generation":"empty","total":0,"items":[],"endpoint":host_endpoint,"lost":[]});
                    let answer = json!({"id":request["id"],"result":page});
                    let _ = write_frame(&mut writer, &answer, BUSINESS_FRAME_BYTES).await;
                }
            }
        });
    }
    HostedTickTools::new(
        DaemonControl::new(WorkerControlGrant {
            endpoint: daemon.into(),
            daemon_epoch: uuid::Uuid::new_v4(),
            binding: uuid::Uuid::new_v4(),
            capability: "0".repeat(64),
            host_endpoint: daemon.with_extension("host.sock"),
        }),
        vec![],
    )
}

fn install_routed_authorities(agent: &Agent, directory: &std::path::Path) -> ScopedHostSocket {
    let socket = ScopedHostSocket::start(directory.join("routed.sock"));
    socket.install(agent);
    socket
}

async fn access_command(
    access: &tickloop_agent_daemon::EmbeddedAgentAccess,
    command: tickloop_agent_protocol::AgentCommand,
) -> tickloop_agent_protocol::CommandResult {
    use tickloop_agent_protocol::{AgentAccess, CommandEnvelope};
    let receipt = access
        .execute(CommandEnvelope {
            daemon_epoch: access.daemon_epoch(),
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
    use tickloop_agent_daemon::EmbeddedAgentAccess;
    use tickloop_agent_protocol::{
        AgentCommand, CommandResult, ConfigurationChange, HostLocation, HostWorkContext,
        InteractionSource, LaunchSpec, TranscriptRef, UserInput, ViewRef,
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
                "Subagent",
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
        let event = tickloop_agent_test_support::next_event(&mut events).await;
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
    ] {
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
        let CommandResult::Loaded { execution, .. } = access_command(
            &access,
            AgentCommand::LoadRecord {
                record: TranscriptRef {
                    store_root: store.clone(),
                    chat_id: child.into(),
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
            let event = tickloop_agent_test_support::next_event(&mut events).await;
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
                    assert_eq!(value["ticks"], json!([]), "{case}");
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

/// A child freezes, per tool, the authorities of the hosts present when it was delegated.
/// Herdr views added afterwards are never offered to it, while the frozen Tickhost still serves
/// it, and a record frozen with one authority per tool is no longer restored.
#[tokio::test]
async fn a_child_never_gains_herdr_views_added_after_its_delegation() {
    use tickloop_agent_daemon::EmbeddedAgentAccess;
    use tickloop_agent_protocol::{
        AgentCommand, CommandResult, ConfigurationChange, HostWorkContext, LaunchSpec,
        TranscriptRef, UserInput,
    };
    let directory = tempfile::Builder::new()
        .prefix("tl-host-")
        .tempdir_in("/tmp")
        .unwrap();
    let store = directory.path().join("state");
    let herdr = ScopedHostSocket::start(directory.path().join("h.sock"));
    let launch = || {
        let mut launch = LaunchSpec::embedded(directory.path().into());
        launch.host_work_context = Some(HostWorkContext {
            socket: herdr.path.clone(),
            workspace: Some("W1".into()),
        });
        launch
    };
    let agent = Agent::open_embedded(
        store.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![
            call("delegate", "Subagent", json!({"task":"Watch the Ticks."})),
            text("Ready."),
            text("Delegated."),
        ])),
    )
    .unwrap();
    agent
        .install_host_tools(Arc::new(tick_tools(&directory.path().join("d.sock"))))
        .unwrap();
    let access = EmbeddedAgentAccess::new(agent.clone());
    let CommandResult::Created {
        execution: parent, ..
    } = access_command(
        &access,
        AgentCommand::Create {
            launch: launch(),
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
            input: UserInput::Text {
                text: "Delegate the read.".into(),
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
        let event = tickloop_agent_test_support::next_event(&mut events).await;
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
    let frozen = serde_json::to_value(
        agent
            .snapshot(child)
            .await
            .unwrap()
            .delegation
            .unwrap()
            .ceiling
            .host_tools,
    )
    .unwrap();
    for tool in ["TickRead", "TickCall", "TickManage"] {
        assert_eq!(
            frozen[tool].as_array().map(Vec::len),
            Some(1),
            "{tool}: only the Tickhost was present: {frozen}"
        );
    }
    assert!(frozen.get("ReadWorkspace").is_none(), "{frozen}");
    access.shutdown().await;

    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "child-hosted",
            "TickRead",
            json!({"tick_instance_id":tick_protocol::InstanceRef::hosted(uuid::Uuid::new_v4())}),
        ),
        text("Checked."),
    ]));
    let restored = Agent::open_embedded(store.clone(), config(), model.clone()).unwrap();
    restored
        .install_host_tools(Arc::new(
            herdr.tools_behind(&directory.path().join("d.sock")),
        ))
        .unwrap();
    let access = EmbeddedAgentAccess::for_worker(restored.clone(), uuid::Uuid::new_v4());
    let CommandResult::Loaded { execution, .. } = access_command(
        &access,
        AgentCommand::LoadRecord {
            record: TranscriptRef {
                store_root: store.clone(),
                chat_id: child.into(),
            },
            as_new: false,
            launch: Some(launch()),
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
            input: UserInput::Text {
                text: "Read a hosted Tick.".into(),
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
    let mut results = std::collections::BTreeMap::new();
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        if event.run_id != run.run_id {
            continue;
        }
        match event.kind {
            EventKind::ToolCompleted {
                call_id, result, ..
            } => {
                results.insert(call_id, result);
            }
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    access.shutdown().await;
    for request in model.requests.lock().unwrap().iter() {
        let offered = request
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>();
        assert!(offered.contains(&"TickRead"), "{offered:?}");
        assert!(!offered.contains(&"ReadWorkspace"), "{offered:?}");
    }
    let hosted = &results["child-hosted"];
    assert!(
        hosted.content.contains("is not a running hosted Tick"),
        "the frozen Tickhost still serves it: {}",
        hosted.content
    );
    assert!(
        herdr.requests.lock().unwrap().is_empty(),
        "the child never reached Herdr"
    );

    let transcript = tickloop_agent_test_support::transcript_file(&store, child.into());
    let single_authority = std::fs::read_to_string(&transcript)
        .unwrap()
        .lines()
        .map(|line| {
            let mut entry: Value = serde_json::from_str(line).unwrap();
            if entry["type"] == "delegation" {
                for authorities in entry["delegation"]["ceiling"]["host_tools"]
                    .as_object_mut()
                    .unwrap()
                    .values_mut()
                {
                    *authorities = authorities[0].clone();
                }
            }
            serde_json::to_string(&entry).unwrap()
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(&transcript, single_authority).unwrap();
    let reopened =
        Agent::open_embedded(store, config(), Arc::new(ScriptedModel::new(vec![]))).unwrap();
    let result = reopened.resume_chat(child).await;
    assert!(
        result.is_err(),
        "a child frozen with one authority per tool was restored"
    );
    reopened.shutdown().await;
    herdr.stop().await;
}

#[tokio::test]
async fn replacing_the_herdr_socket_blocks_a_prepared_workspace_read_before_dispatch() {
    use tickloop_agent_protocol::HostCallContext;
    use tickloop_agent_runtime::host_tools::{HostCallResult, HostToolProvider};
    let directory = tempfile::Builder::new()
        .prefix("tl-host-")
        .tempdir_in("/tmp")
        .unwrap();
    let original = ScopedHostSocket::start(directory.path().join("host.sock"));
    let provider = original.tools();
    let local_files = tickloop_agent_tools::Tools::new(directory.path().join("chat"))
        .local_file_reader(
            &tickloop_agent_tools::Workspace {
                folder: Some(directory.path().into()),
                additional_directories: Vec::new(),
            },
            Arc::new(NoLocalReads),
        );
    let call = provider
        .prepare_call_in_scope(
            uuid::Uuid::new_v4().into(),
            &tickloop_agent_runtime::host_tools::HostCallScope {
                local_files: local_files.as_ref(),
                context: Some(&HostCallContext {
                    folder: directory.path().into(),
                    workspace_id: Some("W1".into()),
                    interaction: None,
                }),
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
            uuid::Uuid::new_v4().into(),
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
    use tickloop_agent_protocol::PermissionScope;
    let directory = tempfile::tempdir().unwrap();
    let store = directory.path().join("state");
    let denied_input = json!({"file_path":".bashrc","content":"fixture=denied"});
    let model = Arc::new(ScriptedModel::new(vec![
        call("parent-denied", "Write", denied_input.clone()),
        call(
            "delegate",
            "Subagent",
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
    agent
        .submit(parent, "Try the scoped write.".into())
        .await
        .unwrap();
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
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
            tickloop_agent_test_support::next_event(&mut events)
                .await
                .kind,
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
        .submit(parent, "Delegate the denied write.".into())
        .await
        .unwrap();
    let mut child = None;
    let mut denial_returned = false;
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        if event.chat_id != parent {
            child = Some(event.chat_id);
            if let EventKind::ToolCompleted {
                call_id, result, ..
            } = &event.kind
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
        .submit(child, "Try the same write after restore.".into())
        .await
        .unwrap();
    let mut denied = false;
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
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
    use tickloop_agent_protocol::PermissionScope;
    let directory = tempfile::tempdir().unwrap();
    let input = json!({"file_path":".bashrc","content":"fixture=denied"});
    let store = directory.path().join("state");
    let agent = Agent::open_embedded(
        store.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![
            call(
                "delegate",
                "Subagent",
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
    agent
        .submit(parent, "Delegate the write.".into())
        .await
        .unwrap();
    let (child, child_request) = loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        if let EventKind::PermissionRequested { request } = event.kind {
            assert_ne!(event.chat_id, parent);
            break (event.chat_id, request);
        }
    };
    agent.interrupt(parent).await.unwrap();
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        if event.chat_id == parent && matches!(event.kind, EventKind::Interrupted) {
            break;
        }
    }
    let denial = agent
        .submit(parent, "Deny the same write for this session.".into())
        .await
        .unwrap();
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
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
        let event = tickloop_agent_test_support::next_event(&mut events).await;
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
        .submit(child, "Try the write after restoring.".into())
        .await
        .unwrap();
    let mut denied = false;
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
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
            "Subagent",
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
        .submit(parent, "Delegate the file check.".into())
        .await
        .unwrap();
    let result = loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        if event.chat_id == parent
            && let EventKind::ToolCompleted {
                call_id, result, ..
            } = event.kind
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
        child_snapshot.folder,
        Some(directory.path().canonicalize().unwrap())
    );
    assert_ne!(child_snapshot.chat_loop.id, parent_receipt.loop_id);
    let metadata = child_snapshot.delegation.clone().unwrap();
    let registration = agent.subagent_registration(child).await.unwrap().unwrap();
    assert_eq!(registration.handle, "file-check");
    assert_eq!(registration.description, "Check file evidence");
    assert_eq!(metadata.parent_chat_id, parent);
    assert_eq!(metadata.parent_loop_id, parent_receipt.loop_id);
    assert_eq!(metadata.parent_run_id, parent_receipt.run_id);
    assert_eq!(metadata.parent_call_id, "delegate");
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        if event.chat_id == parent && matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
    }
    agent
        .submit(child, "Continue with the same scope.".into())
        .await
        .unwrap();
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        if event.chat_id == child && matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
    }
    {
        let requests = model.requests.lock().unwrap();
        assert!(requests[0].tools.iter().any(|tool| tool.name == "Subagent"));
        assert!(requests[1].tools.iter().any(|tool| tool.name == "Subagent"));
    }
    agent.shutdown().await;
    let transcript = std::fs::read_to_string(tickloop_agent_test_support::transcript_file(
        &store,
        child.into(),
    ))
    .unwrap();
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
async fn child_file_scope_and_denials_remain_fixed_while_full_access_shell_is_inherited() {
    use tickloop_agent_protocol::UserShellState;
    use tickloop_agent_protocol::{Decision, PermissionRule};
    let directory = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let outside_file = outside.path().join("private.txt");
    std::fs::write(&outside_file, "outside evidence").unwrap();
    std::fs::write(directory.path().join("scope.txt"), "scoped evidence").unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "delegate",
            "Subagent",
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
    agent.install_own_tickhost();
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
        .submit(parent, "Delegate the bounded check.".into())
        .await
        .unwrap();
    let mut child = None;
    let mut refused = std::collections::BTreeSet::new();
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        if event.chat_id != parent {
            child = Some(event.chat_id);
        }
        if event.chat_id != parent
            && let EventKind::ToolCompleted {
                call_id, result, ..
            } = &event.kind
        {
            if call_id == "escape" {
                assert!(!result.is_error, "{}", result.content);
            } else {
                assert!(result.is_error, "{call_id}: {}", result.content);
                refused.insert(call_id.clone());
            }
        }
        if event.chat_id == parent && matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
    }
    let child = child.unwrap();
    assert_eq!(
        refused,
        ["outside", "denied"]
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
    let follow_up = agent
        .submit(child, "Check scope.txt again.".into())
        .await
        .unwrap();
    let mut newly_restricted = false;
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        if event.run_id != follow_up.run_id {
            continue;
        }
        if let EventKind::PermissionRequested { request } = &event.kind {
            agent
                .answer_permission(child, request.id, false)
                .await
                .unwrap();
            newly_restricted = true;
        }
        if let EventKind::ToolCompleted {
            call_id, result, ..
        } = &event.kind
            && call_id == "new-restriction"
        {
            assert!(result.is_error);
            assert!(result.content.contains("permission") || result.content.contains("denied"));
            newly_restricted = true;
        }
        if matches!(
            event.kind,
            EventKind::Completed { .. } | EventKind::Interrupted
        ) {
            break;
        }
    }
    assert!(newly_restricted);
    for command in [
        format!("cat {}", outside_file.display()),
        "python3 -c 'print(123)'".into(),
    ] {
        agent
            .submit_user_shell(child, command, false)
            .await
            .unwrap();
    }
    let shell = agent
        .submit_user_shell(child, "touch direct.txt".into(), false)
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
    use tickloop_agent_protocol::QuestionAnswer;
    use tickloop_agent_protocol::{Decision, PermissionRule};
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "delegate",
            "Subagent",
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
            tickloop_agent_test_support::single_question("Which cadence?"),
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
        .submit(parent, "Delegate the report.".into())
        .await
        .unwrap();
    let mut child = None;
    let mut asked = false;
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        match event.kind {
            EventKind::PermissionRequested { request } => {
                assert_ne!(event.chat_id, parent);
                child = Some(event.chat_id);
                assert_eq!(request.execution.run_id(), Some(event.run_id));
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
    use tickloop_agent_console::agent_console::{AgentConsole, Destination, Intent};
    use tickloop_agent_daemon::EmbeddedAgentAccess;
    use tickloop_agent_protocol::{
        AgentAccess, AgentCommand, AgentDetailQuery, AgentFilter, CommandEnvelope, CommandResult,
        ConfigurationChange, DisplayUpdate, HostLocation, LaunchSpec, ObservationRequest,
        ObservationValue, Page, PendingRequestKind, RequestRef, UserInput, ViewRef,
    };
    use tickloop_agent_protocol::{LoopState, QuestionAnswer};
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "delegate",
            "Subagent",
            json!({"task":"Ask the user for the report cadence."}),
        ),
        call(
            "question",
            "AskUserQuestion",
            tickloop_agent_test_support::single_question("Which cadence?"),
        ),
        text("The child confirmed Weekly."),
        text("The actual child completed."),
        text("The visible child accepted the follow-up in the original Loop."),
    ]));
    let runtime = Agent::new_embedded(config(), model);
    let access = EmbeddedAgentAccess::new(runtime.clone());
    let receipt = access
        .execute(CommandEnvelope {
            daemon_epoch: access.daemon_epoch(),
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
                daemon_epoch: access.daemon_epoch(),
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
    let child_loop = snapshot.conversation.chat_loop.id;
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
        access.daemon_epoch(),
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
        kind: PendingRequestKind::Question,
        version: 1,
    };
    let wrong = access
        .execute(CommandEnvelope {
            daemon_epoch: access.daemon_epoch(),
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
            daemon_epoch: access.daemon_epoch(),
            command_id: uuid::Uuid::new_v4(),
            command: AgentCommand::Submit {
                execution: child.execution,
                input: UserInput::Shell {
                    command: "python3 -c 'print(123)'".into(),
                    background: false,
                },
                origin: None,
            },
        })
        .await
        .unwrap();
    assert!(shell.error.is_none());
    let answered = access
        .execute(CommandEnvelope {
            daemon_epoch: access.daemon_epoch(),
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
                && snapshot.conversation.chat_loop.state == LoopState::Idle
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
            tickloop_agent_protocol::Content::ToolResult {
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
        tickloop_ids::IdContext::new().model_text(tickloop_agent_protocol::DelegationId::from(
            metadata.delegation_id
        ))
    );
    let original_child_run =
        tickloop_agent_protocol::RunId::parse(parent_result["child_run_id"].as_str().unwrap())
            .unwrap()
            .uuid();
    let follow_up = access
        .execute(CommandEnvelope {
            daemon_epoch: access.daemon_epoch(),
            command_id: uuid::Uuid::new_v4(),
            command: AgentCommand::Submit {
                execution: console.projection().visible.unwrap(),
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
    assert_eq!(
        serde_json::to_value(child_snapshot.conversation.delegation.as_ref().unwrap()).unwrap(),
        serde_json::to_value(&metadata).unwrap()
    );
    let forbidden = access
        .execute(CommandEnvelope {
            daemon_epoch: access.daemon_epoch(),
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
    use tickloop_agent_protocol::{LoopState, QuestionAnswer};
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "delegate",
            "Subagent",
            json!({"task":"Ask which cadence and continue independently."}),
        ),
        call(
            "question",
            "AskUserQuestion",
            tickloop_agent_test_support::single_question("Which cadence?"),
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
    agent
        .submit(parent, "Delegate and wait.".into())
        .await
        .unwrap();
    let (child, request) = loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        if let EventKind::QuestionAsked { request } = event.kind {
            break (event.chat_id, request);
        }
    };
    agent.interrupt(parent).await.unwrap();
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        if event.chat_id == parent && matches!(event.kind, EventKind::Interrupted) {
            break;
        }
    }
    assert_eq!(
        agent.snapshot(parent).await.unwrap().chat_loop.state,
        LoopState::Interrupted
    );
    assert_eq!(
        agent.snapshot(child).await.unwrap().chat_loop.state,
        LoopState::WaitingQuestion
    );
    agent
        .answer_question(child, request.id, vec![QuestionAnswer::option("Weekly")])
        .await
        .unwrap();
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        if event.chat_id == child && matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
    }
    assert_eq!(agent.list_chats().await.len(), 2);
    assert_eq!(
        agent.snapshot(child).await.unwrap().chat_loop.state,
        LoopState::Idle
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn missing_frozen_tool_authority_refuses_child_restore_instead_of_becoming_root() {
    let directory = tempfile::tempdir().unwrap();
    let store = directory.path().join("state");
    let model = Arc::new(ScriptedModel::new(vec![
        call("delegate", "Subagent", json!({"task":"Inspect fixture"})),
        text("Child done."),
        text("Parent done."),
    ]));
    let agent = Agent::open_embedded(store.clone(), config(), model).unwrap();
    let parent = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(parent, "Delegate fixture inspection".into())
        .await
        .unwrap();
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        if event.chat_id == parent && matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
    }
    let child = agent
        .list_chats()
        .await
        .into_iter()
        .find(|chat| chat.id != parent)
        .unwrap()
        .id;
    agent.shutdown().await;
    let transcript = tickloop_agent_test_support::transcript_file(&store, child.into());
    let original = std::fs::read_to_string(&transcript).unwrap();
    for remove_record in [false, true] {
        let rewritten = original
            .lines()
            .filter_map(|line| {
                let mut entry: Value = serde_json::from_str(line).unwrap();
                if entry["type"] == "delegation" {
                    if remove_record {
                        return None;
                    }
                    entry["delegation"]["ceiling"]
                        .as_object_mut()
                        .unwrap()
                        .remove("host_tools");
                }
                Some(serde_json::to_string(&entry).unwrap())
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(&transcript, rewritten).unwrap();
        let restored = Agent::open_embedded(
            store.clone(),
            config(),
            Arc::new(ScriptedModel::new(vec![])),
        )
        .unwrap();
        let result = restored.resume_chat(child).await;
        assert!(
            result.is_err(),
            "a child with missing frozen authority was restored as an unrestricted root"
        );
        assert!(result.unwrap_err().to_string().contains("delegat"));
        restored.shutdown().await;
    }
}

#[tokio::test]
async fn compacted_child_retains_authorization_with_legal_metadata_whitespace_and_field_order() {
    let directory = tempfile::tempdir().unwrap();
    let store = directory.path().join("state");
    let agent = Agent::open_embedded(
        store.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![
            call("delegate", "Subagent", json!({"task":"Fixture task"})),
            text("Child done."),
            text("Parent done."),
        ])),
    )
    .unwrap();
    let parent = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(parent, "Delegate fixture task".into())
        .await
        .unwrap();
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        if event.chat_id == parent && matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
    }
    let child = agent
        .list_chats()
        .await
        .into_iter()
        .find(|chat| chat.id != parent)
        .unwrap()
        .id;
    let expected = serde_json::to_value(
        agent
            .snapshot(child)
            .await
            .unwrap()
            .delegation
            .unwrap()
            .ceiling,
    )
    .unwrap();
    agent.shutdown().await;
    let transcript = tickloop_agent_test_support::transcript_file(&store, child.into());
    let original = std::fs::read_to_string(&transcript).unwrap();
    for reorder in [false, true] {
        let mut initial_user = None;
        let mut rewritten = String::new();
        for line in original.lines() {
            let entry: Value = serde_json::from_str(line).unwrap();
            if entry["type"] == "user" && initial_user.is_none() {
                initial_user = Some(entry.clone());
            }
            if entry["type"] == "delegation" {
                if reorder {
                    rewritten.push_str(&entry.to_string());
                } else {
                    rewritten.push(' ');
                    rewritten.push_str(line);
                }
            } else {
                rewritten.push_str(line);
            }
            rewritten.push('\n');
        }
        rewritten
            .push_str(&json!({"type":"unknown","padding":"x".repeat(6 * 1024 * 1024)}).to_string());
        rewritten.push('\n');
        let mut boundary = initial_user.unwrap();
        let boundary_id = uuid::Uuid::new_v4();
        boundary["uuid"] = json!(boundary_id);
        boundary["parentUuid"] = Value::Null;
        boundary["subtype"] = json!("compact_boundary");
        boundary["type"] = json!("system");
        boundary["message"] =
            json!({"role":"system","content":[{"type":"text","text":"Context summary"}]});
        boundary
            .as_object_mut()
            .unwrap()
            .remove("agentMessageOrigin");
        let mut serialized = boundary.clone();
        serialized.as_object_mut().unwrap().remove("type");
        rewritten.push_str(&format!(
            "{{\"type\":\"system\",{}\n",
            serialized.to_string().trim_start_matches('{')
        ));
        boundary["uuid"] = json!(uuid::Uuid::new_v4());
        boundary["parentUuid"] = json!(boundary_id);
        boundary["type"] = json!("user");
        boundary.as_object_mut().unwrap().remove("subtype");
        boundary["message"] =
            json!({"role":"user","content":[{"type":"text","text":"Continue fixture"}]});
        rewritten.push_str(&boundary.to_string());
        rewritten.push('\n');
        std::fs::write(&transcript, rewritten).unwrap();
        let restored = Agent::open_embedded(
            store.clone(),
            config(),
            Arc::new(ScriptedModel::new(vec![])),
        )
        .unwrap();
        let snapshot = restored.resume_chat(child).await.unwrap();
        let delegation = snapshot
            .delegation
            .expect("compaction discarded delegated authority and restored a root");
        assert_eq!(serde_json::to_value(delegation.ceiling).unwrap(), expected);
        restored.shutdown().await;
    }
}

#[tokio::test]
async fn a_full_access_child_can_run_an_interpreter_under_its_frozen_authority() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "launch",
            "Subagent",
            json!({"task":"Check interpreter output","name":"audit"}),
        ),
        call(
            "python",
            "Bash",
            json!({"command":"python3 -c 'print(7319)'","description":"Check interpreter output"}),
        ),
        text("Child finished"),
        text("Root finished"),
    ]));
    let core = Agent::new_embedded(config(), model);
    core.install_own_tickhost();
    let routed = install_routed_authorities(&core, directory.path());
    let root = core.create_chat(directory.path().into()).await.unwrap();
    core.set_execution_mode(root, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let mut events = core.subscribe();
    let receipt = core
        .submit(root, "Run the bounded interpreter check".into())
        .await
        .unwrap();
    let mut result = None;
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        if let EventKind::ToolCompleted {
            call_id,
            result: output,
            ..
        } = event.kind
        {
            if call_id == "python" {
                result = Some(output);
            }
        } else if event.run_id == receipt.run_id
            && matches!(event.kind, EventKind::Completed { .. })
        {
            break;
        }
    }
    let child = core.subagents(root).await.unwrap()[0].child_chat_id;
    let ceiling = core
        .snapshot(child)
        .await
        .unwrap()
        .delegation
        .unwrap()
        .ceiling;
    assert_eq!(ceiling.host_tools["TickRead"].len(), 1);
    assert_eq!(ceiling.host_tools["TickManage"].len(), 2);
    assert!(!ceiling.host_tools.contains_key("Bash"));
    core.shutdown().await;
    routed.stop().await;
    let result = result.expect("the real delegated Bash call must return");
    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.contains("7319"), "{}", result.content);
}

#[tokio::test]
async fn a_child_interpreter_asks_for_its_own_normal_permission_and_once_runs_it() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "launch",
            "Subagent",
            json!({"task":"Check interpreter with approval","name":"audit"}),
        ),
        call(
            "python",
            "Bash",
            json!({"command":"python3 -c 'print(8426)'","description":"Check approved interpreter"}),
        ),
        text("Child finished"),
        text("Root finished"),
    ]));
    let core = Agent::new_embedded(config(), model);
    core.install_own_tickhost();
    let routed = install_routed_authorities(&core, directory.path());
    let root = core.create_chat(directory.path().into()).await.unwrap();
    core.set_execution_mode(root, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    let mut events = core.subscribe();
    let receipt = core
        .submit(root, "Run an interpreter after approval".into())
        .await
        .unwrap();
    let (child, request) = loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        if let EventKind::PermissionRequested { request } = event.kind {
            assert_ne!(event.chat_id, root);
            assert_eq!(request.name, "Bash");
            break (event.chat_id, request);
        }
        assert!(
            !(event.run_id == receipt.run_id && matches!(event.kind, EventKind::Completed { .. })),
            "unknown child Bash must enter normal approval, not end as Unsupported"
        );
    };
    assert!(core.snapshot(child).await.unwrap().commands.is_empty());
    assert!(
        core.answer_permission(root, request.id, true)
            .await
            .is_err()
    );
    core.answer_permission(child, request.id, true)
        .await
        .unwrap();
    let mut result = None;
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        if let EventKind::ToolCompleted {
            call_id,
            result: output,
            ..
        } = event.kind
        {
            if call_id == "python" {
                result = Some(output);
            }
        } else if event.run_id == receipt.run_id
            && matches!(event.kind, EventKind::Completed { .. })
        {
            break;
        }
    }
    core.shutdown().await;
    routed.stop().await;
    let result = result.unwrap();
    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.contains("8426"));
}

struct HeldChildModel {
    hold_at: usize,
    model: ScriptedModel,
    calls: std::sync::atomic::AtomicUsize,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
#[async_trait::async_trait]
impl tickloop_agent_model::ModelClient for HeldChildModel {
    fn identity(&self) -> tickloop_agent_protocol::ModelIdentity {
        self.model.identity()
    }
    async fn generate_title(
        &self,
        request: tickloop_agent_model::ModelRequest,
        events: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
        self.model.generate_title(request, events, cancel).await
    }
    async fn complete(
        &self,
        request: tickloop_agent_model::ModelRequest,
        events: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
        if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == self.hold_at {
            self.entered.notify_one();
            tokio::select! { ()=self.release.notified()=>{}, ()=cancel.cancelled()=>return Err(tickloop_agent_model::ModelError::Cancelled) }
        }
        self.model.complete(request, events, cancel).await
    }
}

#[tokio::test]
async fn parent_mode_tightening_turns_the_childs_frozen_full_access_bash_into_ask() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(HeldChildModel {
        hold_at: 1,
        model: ScriptedModel::new(vec![
            call(
                "launch",
                "Subagent",
                json!({"task":"Check mode tightening"}),
            ),
            call(
                "python",
                "Bash",
                json!({"command":"python3 -c 'print(1957)'"}),
            ),
            text("Child done"),
            text("Root done"),
        ]),
        calls: std::sync::atomic::AtomicUsize::new(0),
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let core = Agent::new_embedded(config(), model.clone());
    let root = core.create_chat(directory.path().into()).await.unwrap();
    core.set_execution_mode(root, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let mut events = core.subscribe();
    core.submit(root, "Run a check under current authority".into())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), model.entered.notified())
        .await
        .unwrap();
    core.set_execution_mode(root, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    model.release.notify_one();
    let observed = loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        match event.kind {
            EventKind::PermissionRequested { request } if request.name == "Bash" => {
                break Some((event.chat_id, request));
            }
            EventKind::ToolStarted { call_id, .. } if call_id == "python" => break None,
            _ => {}
        }
    };
    if let Some((child, request)) = &observed {
        core.answer_permission(*child, request.id, false)
            .await
            .unwrap();
    }
    core.shutdown().await;
    assert!(
        observed.is_some(),
        "a parent tightened to Ask must still constrain its already-created Full Access child"
    );
}

#[tokio::test]
async fn direct_child_shell_returns_a_receipt_and_uses_the_same_child_permission_route() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "launch",
            "Subagent",
            json!({"task":"Wait for direct human Shell","name":"audit"}),
        ),
        text("Child ready"),
        text("Root ready"),
    ]));
    let core = Agent::new_embedded(config(), model.clone());
    core.install_own_tickhost();
    let root = core.create_chat(directory.path().into()).await.unwrap();
    core.set_execution_mode(root, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    let mut events = core.subscribe();
    let root_run = core
        .submit(root, "Create a child for direct commands".into())
        .await
        .unwrap();
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        if event.run_id == root_run.run_id && matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
    }
    let child = core.subagents(root).await.unwrap()[0].child_chat_id;
    let sent = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        core.submit_user_shell(child, "python3 -c 'print(4268)'".into(), false),
    )
    .await
    .unwrap();
    if sent.is_err() {
        core.shutdown().await;
    }
    let receipt =
        sent.expect("direct child Shell must be accepted without awaiting or bypassing approval");
    let request = loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        if event.chat_id == child
            && let EventKind::PermissionRequested { request } = event.kind
        {
            break request;
        }
    };
    assert!(
        core.snapshot(child)
            .await
            .unwrap()
            .user_shells
            .iter()
            .all(|shell| shell.pid.is_none())
    );
    core.answer_permission(child, request.id, true)
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let snapshot = core.snapshot(child).await.unwrap();
            if snapshot.user_shells.iter().any(|shell| {
                shell.id == receipt.command_id
                    && shell.state == tickloop_agent_protocol::UserShellState::Completed
            }) {
                break snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let output = core
        .read_user_shell_output(child, receipt.command_id, 0, 1024)
        .await
        .unwrap();
    core.shutdown().await;
    assert!(output.content.contains("4268"));
    assert_eq!(
        model.requests.lock().unwrap().len(),
        3,
        "direct Shell permission must not fabricate a model turn"
    );
}

#[tokio::test]
async fn child_interpreters_honor_the_frozen_explicit_allow_and_deny_rules() {
    use tickloop_agent_protocol::{Decision, PermissionRule};
    let command = "python3 -c 'print(5438)'";
    for decision in [Decision::Allow, Decision::Deny] {
        let directory = tempfile::tempdir().unwrap();
        let model = Arc::new(ScriptedModel::new(vec![
            call(
                "launch",
                "Subagent",
                json!({"task":"Check explicit Bash policy"}),
            ),
            call("python", "Bash", json!({"command":command})),
            text("Child done"),
            text("Root done"),
        ]));
        let core = Agent::new_embedded(config(), model);
        core.install_own_tickhost();
        let root = core.create_chat(directory.path().into()).await.unwrap();
        core.set_execution_mode(root, ExecutionMode::AcceptEdits)
            .await
            .unwrap();
        core.set_permission_rules(
            root,
            vec![PermissionRule {
                id: uuid::Uuid::new_v4(),
                decision,
                tool: "Bash".into(),
                specifier: Some(command.into()),
                input: None,
                loop_id: None,
                persistent: false,
            }],
        )
        .await
        .unwrap();
        let mut events = core.subscribe();
        let receipt = core
            .submit(root, "Run according to the explicit Shell rule".into())
            .await
            .unwrap();
        let mut result = None;
        loop {
            let event = tickloop_agent_test_support::next_event(&mut events).await;
            match event.kind {
                EventKind::PermissionRequested { request } => {
                    core.answer_permission(event.chat_id, request.id, false)
                        .await
                        .unwrap();
                    panic!("a frozen {decision:?} rule must not become a different approval path");
                }
                EventKind::ToolCompleted {
                    call_id,
                    result: output,
                    ..
                } if call_id == "python" => result = Some(output),
                EventKind::Completed { .. } if event.run_id == receipt.run_id => break,
                _ => {}
            }
        }
        core.shutdown().await;
        let output = result.unwrap();
        assert_eq!(
            output.is_error,
            decision == Decision::Deny,
            "{}",
            output.content
        );
        if decision == Decision::Allow {
            assert!(output.content.contains("5438"));
        }
    }
}

#[tokio::test]
async fn a_once_answer_cannot_override_a_parent_bash_denial_added_while_the_child_waits() {
    use tickloop_agent_protocol::{Decision, PermissionRule};
    let directory = tempfile::tempdir().unwrap();
    let command =
        r#"python3 -c 'from pathlib import Path; Path("not-admitted").write_text("bad")'"#;
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "launch",
            "Subagent",
            json!({"task":"Wait for interpreter permission"}),
        ),
        call("python", "Bash", json!({"command":command})),
        text("Child done"),
        text("Root done"),
    ]));
    let core = Agent::new_embedded(config(), model);
    let root = core.create_chat(directory.path().into()).await.unwrap();
    core.set_execution_mode(root, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    let mut events = core.subscribe();
    core.submit(root, "Keep the inherited Shell denial current".into())
        .await
        .unwrap();
    let (child, request) = loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        if let EventKind::PermissionRequested { request } = event.kind {
            break (event.chat_id, request);
        }
    };
    core.set_permission_rules(
        root,
        vec![PermissionRule {
            id: uuid::Uuid::new_v4(),
            decision: Decision::Deny,
            tool: "Bash".into(),
            specifier: None,
            input: None,
            loop_id: None,
            persistent: false,
        }],
    )
    .await
    .unwrap();
    core.answer_permission(child, request.id, true)
        .await
        .unwrap();
    let mut refused = false;
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        match event.kind {
            EventKind::ToolStarted { call_id, .. } if call_id == "python" => {
                panic!("late parent Deny must win over child Once")
            }
            EventKind::ToolCompleted {
                call_id, result, ..
            } if call_id == "python" => {
                refused = result.is_error;
                break;
            }
            EventKind::Failed(reason) if event.chat_id == child => {
                assert!(reason.contains("denial"), "{reason}");
                refused = true;
                break;
            }
            EventKind::Completed { .. } if event.chat_id == child => break,
            _ => {}
        }
    }
    core.shutdown().await;
    assert!(refused);
    assert!(!directory.path().join("not-admitted").exists());
}

struct BackgroundResultModel {
    scripted: ScriptedModel,
}
#[async_trait::async_trait]
impl tickloop_agent_model::ModelClient for BackgroundResultModel {
    fn identity(&self) -> tickloop_agent_protocol::ModelIdentity {
        self.scripted.identity()
    }
    async fn generate_title(
        &self,
        request: tickloop_agent_model::ModelRequest,
        events: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
        self.scripted.generate_title(request, events, cancel).await
    }
    async fn complete(
        &self,
        request: tickloop_agent_model::ModelRequest,
        events: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
        use tickloop_agent_model::ModelOutput;
        use tickloop_agent_protocol::{Content, Role, Usage};
        let already_ready = request.messages.iter().any(|message| {
            message.role == Role::Assistant
                && message
                    .content
                    .iter()
                    .any(|content| matches!(content,Content::Text{text} if text=="Child ready"))
        });
        let notification = request.messages.iter().rev().find(|message|message.role==Role::User).is_some_and(|message|message.content.iter().all(|content|matches!(content,Content::Text{text} if text.starts_with("<task-notification>"))));
        if already_ready && notification {
            return Ok(ModelOutput {
                content: text("Output notification received"),
                usage: Usage::default(),
                stop_reason: "end_turn".into(),
                private_source: None,
            });
        }
        self.scripted.complete(request, events, cancel).await
    }
}

#[tokio::test]
async fn a_child_reads_its_announced_background_output_without_permission() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(BackgroundResultModel {
        scripted: ScriptedModel::new(vec![
            call(
                "launch",
                "Subagent",
                json!({"task":"Keep a background result","name":"result"}),
            ),
            call(
                "background-output",
                "Bash",
                json!({"command":"echo child-background-result","run_in_background":true}),
            ),
            text("Child ready"),
            text("Root ready"),
        ]),
    });
    let core = Agent::new_embedded(config(), model.clone());
    core.install_own_tickhost();
    let routed = install_routed_authorities(&core, directory.path());
    let root = core.create_chat(directory.path().into()).await.unwrap();
    core.set_execution_mode(root, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    let mut events = core.subscribe();
    let receipt = core
        .submit(root, "Create a result reader".into())
        .await
        .unwrap();
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        if event.run_id == receipt.run_id && matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
    }
    let child = core.subagents(root).await.unwrap()[0].child_chat_id;
    let path = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let snapshot = core.snapshot(child).await.unwrap();
            let command = snapshot.commands.last().unwrap();
            if command.output_bytes > 0
                && command.state == tickloop_agent_protocol::CommandState::Completed
            {
                break command.output_ref.clone();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    model
        .scripted
        .enqueue(call("read-result", "Read", json!({"file_path":path})));
    model.scripted.enqueue(text("Read complete"));
    let receipt = core
        .submit(child, "Read your announced output file".into())
        .await
        .unwrap();
    let mut result = None;
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        match event.kind {
            EventKind::PermissionRequested { .. } => panic!("own session output must not prompt"),
            EventKind::ToolCompleted {
                call_id,
                result: output,
                ..
            } if call_id == "read-result" => result = Some(output),
            EventKind::Completed { .. } if event.run_id == receipt.run_id => break,
            _ => {}
        }
    }
    core.shutdown().await;
    routed.stop().await;
    let result = result.unwrap();
    assert!(!result.is_error, "{}", result.content);
    assert!(
        result.content.contains("child-background-result"),
        "{}",
        result.content
    );
}

#[tokio::test]
async fn stopping_a_direct_child_shell_waiting_for_permission_cancels_its_request() {
    for stop_loop in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let model = Arc::new(ScriptedModel::new(vec![
            call(
                "launch",
                "Subagent",
                json!({"task":"Await Shell permission"}),
            ),
            text("Child ready"),
            text("Root ready"),
        ]));
        let core = Agent::new_embedded(config(), model);
        let root = core.create_chat(directory.path().into()).await.unwrap();
        core.set_execution_mode(root, ExecutionMode::AcceptEdits)
            .await
            .unwrap();
        let mut events = core.subscribe();
        let receipt = core
            .submit(root, "Create a cancellable child Shell".into())
            .await
            .unwrap();
        loop {
            let event = tickloop_agent_test_support::next_event(&mut events).await;
            if event.run_id == receipt.run_id && matches!(event.kind, EventKind::Completed { .. }) {
                break;
            }
        }
        let child = core.subagents(root).await.unwrap()[0].child_chat_id;
        let shell = core
            .submit_user_shell(
                child,
                "python3 -c 'print(unsupported_after_cancel)'".into(),
                false,
            )
            .await
            .unwrap();
        let request = loop {
            let event = tickloop_agent_test_support::next_event(&mut events).await;
            if let EventKind::PermissionRequested { request } = event.kind {
                break request;
            }
        };
        let queued = if stop_loop {
            Some(
                core.submit_user_shell(child, "touch queued-after-stop".into(), false)
                    .await
                    .unwrap(),
            )
        } else {
            None
        };
        let stopped = if stop_loop {
            core.stop(child).await.unwrap();
            core.snapshot(child)
                .await
                .unwrap()
                .user_shells
                .iter()
                .find(|record| record.id == shell.command_id)
                .unwrap()
                .clone()
        } else {
            core.stop_user_shell(child, shell.command_id).await.unwrap()
        };
        assert_eq!(
            stopped.state,
            tickloop_agent_protocol::UserShellState::Failed
        );
        assert!(
            core.answer_permission(child, request.id, true)
                .await
                .is_err()
        );
        let snapshot = core.snapshot(child).await.unwrap();
        assert!(snapshot.permissions.is_empty());
        if let Some(queued) = queued {
            assert_eq!(
                snapshot
                    .user_shells
                    .iter()
                    .find(|shell| shell.id == queued.command_id)
                    .unwrap()
                    .state,
                tickloop_agent_protocol::UserShellState::Failed
            );
            assert!(!directory.path().join("queued-after-stop").exists());
        }
        assert!(snapshot.user_shells.iter().all(|shell| shell.pid.is_none()));
        core.shutdown().await;
    }
}

#[tokio::test]
async fn resuming_only_a_child_loads_current_parent_authority_before_shell_admission() {
    let directory = tempfile::tempdir().unwrap();
    let store = directory.path().join("state");
    let core = Agent::open_embedded(
        store.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![
            call(
                "launch",
                "Subagent",
                json!({"task":"Check resumed authority"}),
            ),
            text("Child ready"),
            text("Root ready"),
        ])),
    )
    .unwrap();
    let root = core.create_chat(directory.path().into()).await.unwrap();
    core.set_execution_mode(root, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let mut events = core.subscribe();
    let receipt = core
        .submit(root, "Create a child before tightening".into())
        .await
        .unwrap();
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        if event.run_id == receipt.run_id && matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
    }
    let child = core.subagents(root).await.unwrap()[0].child_chat_id;
    core.set_execution_mode(root, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    core.shutdown().await;
    let restored =
        Agent::open_embedded(store, config(), Arc::new(ScriptedModel::new(vec![]))).unwrap();
    restored.resume_chat(child).await.unwrap();
    assert_eq!(
        restored.snapshot(root).await.unwrap().policy.mode,
        ExecutionMode::AcceptEdits
    );
    let mut events = restored.subscribe();
    let command = restored
        .submit_user_shell(child, "python3 -c 'print(8841)'".into(), false)
        .await
        .unwrap();
    let request = loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        if let EventKind::PermissionRequested { request } = event.kind {
            assert_eq!(event.chat_id, child);
            break request;
        }
    };
    assert!(
        restored
            .snapshot(child)
            .await
            .unwrap()
            .user_shells
            .iter()
            .find(|shell| shell.id == command.command_id)
            .unwrap()
            .pid
            .is_none()
    );
    restored
        .answer_permission(child, request.id, false)
        .await
        .unwrap();
    restored.shutdown().await;
}

#[tokio::test]
async fn a_direct_child_shell_session_grant_applies_to_the_same_prepared_command() {
    use tickloop_agent_protocol::PermissionScope;
    let directory = tempfile::tempdir().unwrap();
    let core = Agent::new_embedded(
        config(),
        Arc::new(ScriptedModel::new(vec![
            call(
                "launch",
                "Subagent",
                json!({"task":"Reuse a Shell session grant"}),
            ),
            text("Child ready"),
            text("Root ready"),
        ])),
    );
    core.install_own_tickhost();
    let root = core.create_chat(directory.path().into()).await.unwrap();
    let mut events = core.subscribe();
    let receipt = core.submit(root, "Create a child".into()).await.unwrap();
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        if event.run_id == receipt.run_id && matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
    }
    let child = core.subagents(root).await.unwrap()[0].child_chat_id;
    let command = "python3 -c 'print(4268)'";
    let first = core
        .submit_user_shell(child, command.into(), false)
        .await
        .unwrap();
    let request = loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        if let EventKind::PermissionRequested { request } = event.kind {
            break request;
        }
    };
    core.answer_permission_with_scope(child, request.id, true, PermissionScope::Session)
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if core
                .snapshot(child)
                .await
                .unwrap()
                .user_shells
                .iter()
                .any(|shell| {
                    shell.id == first.command_id
                        && shell.state == tickloop_agent_protocol::UserShellState::Completed
                })
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let second = core
        .submit_user_shell(child, command.into(), false)
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let snapshot = core.snapshot(child).await.unwrap();
            assert!(
                snapshot.permissions.is_empty(),
                "an exact Session grant must not prompt again"
            );
            if snapshot.user_shells.iter().any(|shell| {
                shell.id == second.command_id
                    && shell.state == tickloop_agent_protocol::UserShellState::Completed
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    core.shutdown().await;
}

#[tokio::test]
async fn a_parent_loop_scoped_bash_denial_stays_frozen_after_the_parent_removes_it() {
    use tickloop_agent_protocol::{Decision, PermissionRule};
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(HeldChildModel {
        hold_at: 2,
        model: ScriptedModel::new(vec![
            text("Root goal ready"),
            call(
                "launch",
                "Subagent",
                json!({"task":"Keep the scoped Bash denial"}),
            ),
            call(
                "scoped-python",
                "Bash",
                json!({"command":"python3 -c 'print(7391)'"}),
            ),
            text("Child done"),
            text("Root done"),
        ]),
        calls: std::sync::atomic::AtomicUsize::new(0),
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let core = Agent::new_embedded(config(), model.clone());
    let root = core.create_chat(directory.path().into()).await.unwrap();
    core.set_execution_mode(root, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let mut events = core.subscribe();
    let initial = core
        .submit(root, "Establish the parent Loop".into())
        .await
        .unwrap();
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        if event.run_id == initial.run_id && matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
    }
    core.set_permission_rules(
        root,
        vec![PermissionRule {
            id: uuid::Uuid::new_v4(),
            decision: Decision::Deny,
            tool: "Bash".into(),
            specifier: None,
            input: None,
            loop_id: Some(initial.loop_id.into()),
            persistent: false,
        }],
    )
    .await
    .unwrap();
    let receipt = core
        .submit(root, "Delegate under that Loop denial".into())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), model.entered.notified())
        .await
        .unwrap();
    core.set_permission_rules(root, vec![]).await.unwrap();
    model.release.notify_one();
    let mut denied = false;
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        match event.kind {
            EventKind::ToolStarted { call_id, .. } if call_id == "scoped-python" => {
                panic!("a frozen parent Loop denial must remain a denial")
            }
            EventKind::ToolCompleted {
                call_id, result, ..
            } if call_id == "scoped-python" => denied = result.is_error,
            EventKind::Failed(reason) if event.chat_id != root => {
                assert!(
                    reason.contains("denies") || reason.contains("denial"),
                    "{reason}"
                );
                denied = true;
            }
            EventKind::Completed { .. } if event.run_id == receipt.run_id => break,
            _ => {}
        }
    }
    core.shutdown().await;
    assert!(denied);
}

#[tokio::test]
async fn stop_subagent_is_successful_when_its_loop_already_cancelled_a_pending_shell() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "launch",
            "Subagent",
            json!({"task":"Wait for direct Shell","name":"waiting"}),
        ),
        text("Child ready"),
        text("Root ready"),
    ]));
    let core = Agent::new_embedded(config(), model.clone());
    let root = core.create_chat(directory.path().into()).await.unwrap();
    let mut events = core.subscribe();
    let initial = core.submit(root, "Create a child".into()).await.unwrap();
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        if event.run_id == initial.run_id && matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
    }
    let child = core.subagents(root).await.unwrap()[0].child_chat_id;
    core.submit_user_shell(child, "python3 -c 'print(8841)'".into(), false)
        .await
        .unwrap();
    let request = loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        if let EventKind::PermissionRequested { request } = event.kind {
            break request;
        }
    };
    model.enqueue(call(
        "stop-pending",
        "StopSubagent",
        json!({"to":"waiting"}),
    ));
    model.enqueue(text("Root stopped child"));
    core.submit(root, "Stop the child".into()).await.unwrap();
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        if let EventKind::ToolCompleted {
            call_id, result, ..
        } = event.kind
            && call_id == "stop-pending"
        {
            assert!(!result.is_error, "{}", result.content);
            break;
        }
    }
    assert!(
        core.answer_permission(child, request.id, true)
            .await
            .is_err()
    );
    assert!(core.snapshot(child).await.unwrap().permissions.is_empty());
    core.shutdown().await;
}

#[tokio::test]
async fn a_child_preserves_the_parents_frozen_persistent_shell_allow_rule() {
    use tickloop_agent_protocol::{Decision, PermissionRule};
    let directory = tempfile::tempdir().unwrap();
    let command = "python3 -c 'print(5438)'";
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "launch",
            "Subagent",
            json!({"task":"Use the configured interpreter rule"}),
        ),
        call("configured-python", "Bash", json!({"command":command})),
        text("Child done"),
        text("Root done"),
    ]));
    let core = Agent::open_embedded(directory.path().join("state"), config(), model).unwrap();
    core.install_own_tickhost();
    let root = core.create_chat(directory.path().into()).await.unwrap();
    core.set_permission_rules(
        root,
        vec![PermissionRule {
            id: uuid::Uuid::new_v4(),
            decision: Decision::Allow,
            tool: "Bash".into(),
            specifier: Some(command.into()),
            input: None,
            loop_id: None,
            persistent: true,
        }],
    )
    .await
    .unwrap();
    let mut events = core.subscribe();
    let receipt = core
        .submit(root, "Use the saved Shell permission".into())
        .await
        .unwrap();
    let mut result = None;
    loop {
        let event = tickloop_agent_test_support::next_event(&mut events).await;
        match event.kind {
            EventKind::PermissionRequested { .. } => {
                panic!("a frozen matching persistent Allow must not be deleted by refresh")
            }
            EventKind::ToolCompleted {
                call_id,
                result: output,
                ..
            } if call_id == "configured-python" => result = Some(output),
            EventKind::Completed { .. } if event.run_id == receipt.run_id => break,
            _ => {}
        }
    }
    core.shutdown().await;
    let result = result.unwrap();
    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.contains("5438"));
}

/// Refuses every local file read; the host calls these tests prepare send no file.
struct NoLocalReads;

#[async_trait::async_trait]
impl tickloop_agent_tools::LocalReadAdmission for NoLocalReads {
    async fn admit(&self, _: &tickloop_agent_tools::PreparedTool) -> Result<bool, String> {
        Err("no local file is read here".into())
    }
}
