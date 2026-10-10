//! Hosted Ticks through the real CLI, daemon and worker processes: the worker asks its daemon over
//! the authenticated channel, the daemon starts the Tickhost on demand, and the Tick runs hidden
//! in a private terminal with no Herdr anywhere.

use crate::support;

use std::{
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use serde_json::{Value, json};
use support::hosted::{counter_program, launch_command};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

const FIXTURE_TIMEOUT: Duration = Duration::from_secs(30);

fn alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

async fn gone(pid: u32) -> bool {
    for _ in 0..400 {
        if !alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

async fn read_request(stream: &mut TcpStream) -> (String, Value) {
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        let count = stream.read(&mut buffer).await.unwrap();
        assert_ne!(count, 0, "fixture request ended before its body");
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..end]);
            let length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|value| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            if bytes.len() >= end + 4 + length {
                return (
                    headers.lines().next().unwrap().to_owned(),
                    serde_json::from_slice(&bytes[end + 4..end + 4 + length])
                        .unwrap_or(Value::Null),
                );
            }
        }
    }
}

fn model_stream(block: Value) -> String {
    let stop = if block["type"] == "tool_use" {
        "tool_use"
    } else {
        "end_turn"
    };
    [
        json!({"type":"message_start","message":{"id":"fixture","type":"message","role":"assistant","content":[],"usage":{"input_tokens":1,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":block}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":stop},"usage":{"output_tokens":1}}),
        json!({"type":"message_stop"}),
    ]
    .iter()
    .map(|event| format!("data: {event}\n\n"))
    .collect()
}

/// Every tool result the model has seen, in order.
fn tool_results(request: &Value) -> Vec<String> {
    request["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|message| message["content"].as_array().cloned().unwrap_or_default())
        .filter(|block| block["type"] == "tool_result")
        .map(|block| match &block["content"] {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        })
        .collect()
}

fn hosted_instance(results: &[String]) -> Option<String> {
    results.iter().find_map(|result| {
        let start = result.find("hosted:")?;
        Some(result[start..start + "hosted:".len() + 36].to_owned())
    })
}

#[derive(Default)]
struct Observed {
    calls: AtomicUsize,
    host_before_open: std::sync::atomic::AtomicBool,
    results: Mutex<Vec<String>>,
}

#[tokio::test(flavor = "multi_thread")]
async fn a_worker_opens_reads_calls_and_closes_a_hidden_tick_through_its_daemon() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let directory = tempfile::Builder::new()
        .prefix("tht")
        .tempdir_in("/private/tmp")
        .unwrap();
    let root = directory.path().canonicalize().unwrap();
    let observed = Arc::new(Observed::default());
    let server_observed = observed.clone();
    let host_runtime = root.join("h");
    let server_host_runtime = host_runtime.clone();
    let open_command = json!(launch_command(counter_program()));
    let server = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let observed = server_observed.clone();
            let open_command = open_command.clone();
            let host_socket = server_host_runtime.join("host.sock");
            tokio::spawn(async move {
                let (path, request) = read_request(&mut stream).await;
                assert!(path.starts_with("POST /v1/messages "), "{path}");
                let results = tool_results(&request);
                *observed.results.lock().unwrap() = results.clone();
                observed.calls.fetch_add(1, Ordering::SeqCst);
                let instance = hosted_instance(&results);
                let block = match (results.len(), instance) {
                    (0, _) => {
                        observed
                            .host_before_open
                            .store(host_socket.exists(), Ordering::SeqCst);
                        json!({"type":"tool_use","id":"open","name":"ModifyTick","input":{"action":"open","command":open_command,"placement":{"type":"hidden"}}})
                    }
                    (1, Some(instance)) => {
                        json!({"type":"tool_use","id":"read","name":"ReadTick","input":{"tick_instance_id":instance}})
                    }
                    (2, Some(instance)) => {
                        json!({"type":"tool_use","id":"call","name":"CallTickTool","input":{"tick_instance_id":instance,"tool":"increment","args":{"amount":2}}})
                    }
                    (3, Some(instance)) => {
                        json!({"type":"tool_use","id":"close","name":"ModifyTick","input":{"action":"close","tick_instance_id":instance}})
                    }
                    _ => json!({"type":"text","text":"Hosted counter finished."}),
                };
                let body = model_stream(block);
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            });
        }
    });
    let config_root = root.join("state");
    let config_path = root.join("model.json");
    std::fs::write(&config_path, serde_json::to_vec(&json!({"name":"fixture","protocol":"messages","base_url":origin,"model":"fixture","api_key_env":"","max_output_tokens":256})).unwrap()).unwrap();
    let runtime_directory = support::cli_runtime::directory();
    let home = root.join("home");
    std::fs::create_dir(&home).unwrap();
    std::fs::set_permissions(
        &home,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
    )
    .unwrap();
    let mut child = support::fixture_cli::tokio_command()
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &home)
        .env("TICKLOOP_DAEMON_RUNTIME_DIR", &runtime_directory)
        .env("TICKLOOP_TICKHOST_RUNTIME_DIR", &host_runtime)
        .env("TICKLOOP_CONFIG_DIR", &config_root)
        .args([
            "--working-directory-is-trusted",
            "--mode",
            "fullAccess",
            "--transport-fixture",
        ])
        .arg(&config_path)
        .arg("--cwd")
        .arg(&root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"Open the counter.\n").await.unwrap();
    let finished = tokio::time::timeout(FIXTURE_TIMEOUT, async {
        while observed.calls.load(Ordering::SeqCst) < 5 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    let results = observed.results.lock().unwrap().clone();
    stdin.write_all(b"/exit\n").await.unwrap();
    drop(stdin);
    let output = tokio::time::timeout(FIXTURE_TIMEOUT, child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    let host_gone = tokio::time::timeout(Duration::from_secs(10), async {
        while host_runtime.join("host.sock").exists() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    support::cli_runtime::shutdown_daemon(&runtime_directory)
        .await
        .unwrap();
    server.abort();
    assert!(
        finished.is_ok(),
        "model calls {}; results {results:#?}; stderr {}",
        observed.calls.load(Ordering::SeqCst),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(results.len(), 4, "{results:#?}");
    assert!(
        !observed.host_before_open.load(Ordering::SeqCst),
        "the daemon starts no host before the first open"
    );
    let opened: Value = serde_json::from_str(&results[0]).unwrap();
    assert_eq!(opened["status"], "opened", "{results:#?}");
    assert_eq!(opened["host"], "tickhost");
    assert!(opened["business_pid"].as_u64().is_some());
    let read: Value = serde_json::from_str(&results[1]).unwrap();
    assert_eq!(read["data"]["value"], 0, "{results:#?}");
    assert!(read["tools"].to_string().contains("increment"));
    let called: Value = serde_json::from_str(&results[2]).unwrap();
    assert_eq!(called["result"]["value"], 2, "{results:#?}");
    let closed: Value = serde_json::from_str(&results[3]).unwrap();
    assert_eq!(closed["status"], "closed", "{results:#?}");
    assert!(
        host_gone.is_ok(),
        "the host exits once its last Tick is closed"
    );
}

mod channel {
    use std::{collections::BTreeMap, path::Path, sync::Arc, time::Duration};

    use async_trait::async_trait;
    use serde_json::{Value, json};
    use tickhost_connect::hosted::{CAPABILITY_DATA, CONTROL_TIMEOUT, HostConnection, Role};
    use tickloop_agent_daemon::{
        DaemonServer, EmbeddedAgentAccess, WorkerFactory, build_runtime_with_client,
        worker_control::WorkerControl,
    };
    use tickloop_agent_model::ModelClient;
    use tickloop_agent_protocol::ToolResult;
    use tickloop_agent_protocol::{
        AccessError, AgentAccess, AgentCommand, CommandEnvelope, CommandResult, EnvironmentValue,
        EventKind, HostEndpointIdentity, HostToolAuthority, HostWorkContext, LaunchSpec,
        TickLinkState, TickSubscriptionChange,
    };
    use tickloop_agent_runtime::{
        Agent,
        host_tools::{HostCallResult, HostToolProvider},
        hosted::{
            controller::HostController,
            lifecycle::DaemonLifecycle,
            tools::HostedTickTools,
            worker_control::{
                DaemonControl, HostedAdmission, WorkerControlGrant, WorkerRequest, root_identity,
            },
        },
        tick_adapter::TickAdapters,
        tick_stream::{
            CONNECTS_PER_WATCH, HostedTickStream, RECONNECT_PAUSE, RECONNECTS_PER_WATCH,
            TickConnectionState, TickStreamEvent, TickStreamPort,
        },
        tick_tool_router::TickToolRouter,
    };
    use tokio::sync::{Mutex, mpsc};
    use tokio_util::sync::CancellationToken;
    use uuid::Uuid;

    use super::support::hosted::{
        counter_program, counter_request, launch_command, python_program, tick_environment,
    };

    /// The Herdr Workspace a Herdr worker's Chats work in.
    const HERDR_WORKSPACE: &str = "workspace-a";
    /// How long a hosted test waits for its next event. It outlasts SubscribeTick's 10-second
    /// wait for a Watch to become ready, during which a run emits nothing.
    const HOSTED_EVENT_WAIT: Duration = Duration::from_secs(30);

    /// How long a hosted test waits for a `high` subscription to deliver: the first delivery
    /// comes one period after subscribing.
    fn high_delivery_wait() -> Duration {
        tickloop_agent_protocol::TickSubscriptionFrequency::High
            .period()
            .unwrap()
            + Duration::from_secs(15)
    }

    /// Workers in this process, each holding the grant the daemon would hand a real worker and,
    /// as a real worker with a config root does, telling the daemon when StopSubagent stops a child.
    struct GrantedWorkers {
        control: Arc<WorkerControl>,
        model: Arc<dyn ModelClient>,
        /// A Herdr endpoint the workers' Views run in; such a worker is built the way the
        /// production worker is, with both its Herdr and its hosted Tick tools.
        herdr: Option<std::path::PathBuf>,
        grants: Mutex<Vec<WorkerControlGrant>>,
        runtimes: Mutex<Vec<Agent>>,
    }

    #[async_trait]
    impl WorkerFactory for GrantedWorkers {
        async fn start(
            &self,
            launch: &LaunchSpec,
            epoch: Uuid,
        ) -> Result<Arc<dyn AgentAccess>, AccessError> {
            let grant = self.control.grant();
            let runtime = match &self.herdr {
                Some(socket) => {
                    let mut launch = launch.clone();
                    launch.host_work_context = Some(HostWorkContext {
                        socket: socket.clone(),
                        workspace: Some(HERDR_WORKSPACE.into()),
                    });
                    launch.environment = vec![EnvironmentValue {
                        name: b"PATH".to_vec(),
                        value: b"/usr/bin:/bin".to_vec(),
                    }];
                    build_runtime_with_client(&launch, self.model.clone(), Some(grant.clone()))
                        .await
                        .map_err(AccessError::unavailable)?
                }
                None => {
                    let runtime = Agent::new_embedded(
                        tickloop_agent_test_support::config(),
                        self.model.clone(),
                    );
                    runtime
                        .install_stopped_child_hook(Arc::new(DaemonLifecycle::new(grant.clone())));
                    let tools = Arc::new(HostedTickTools::new(
                        DaemonControl::new(grant.clone()),
                        tick_environment(),
                    ));
                    runtime
                        .install_host_tools(Arc::new(TickToolRouter::new(
                            None,
                            Some(tools.clone()),
                        )))
                        .unwrap();
                    runtime.install_hosted_ticks(tools.clone());
                    runtime
                        .install_tick_stream(Arc::new(TickAdapters::builtin(
                            runtime.clone(),
                            None,
                            None,
                            Some(tools),
                        )))
                        .await;
                    runtime
                }
            };
            let access: Arc<dyn AgentAccess> =
                Arc::new(EmbeddedAgentAccess::for_worker(runtime.clone(), epoch));
            self.control
                .register(&grant, access.clone(), launch.config_root.as_deref())
                .await;
            self.grants.lock().await.push(grant);
            self.runtimes.lock().await.push(runtime);
            Ok(access)
        }
        async fn exited(&self, _: &Arc<dyn AgentAccess>) -> bool {
            false
        }
    }

    fn list(chat: Uuid) -> WorkerRequest {
        WorkerRequest::ListHostedTicks { chat, cursor: None }
    }

    /// A real daemon with its worker-control endpoint and in-process workers, one per Chat.
    struct Daemon {
        _directory: tempfile::TempDir,
        root: std::path::PathBuf,
        config_root: std::path::PathBuf,
        server: DaemonServer,
        epoch: Uuid,
        chats: Vec<Uuid>,
        executions: Vec<tickloop_agent_protocol::ExecutionRef>,
        grants: Vec<WorkerControlGrant>,
        runtimes: Vec<Agent>,
        controller: Arc<HostController>,
    }

    impl Daemon {
        async fn start(chats: usize) -> Self {
            Self::start_with(
                Arc::new(tickloop_agent_test_support::ScriptedModel::new(Vec::new())),
                super::support::hosted::IDLE_TIMEOUT,
                chats,
            )
            .await
        }

        async fn start_with(
            model: Arc<dyn ModelClient>,
            idle_timeout: Duration,
            chats: usize,
        ) -> Self {
            Self::start_configured(model, idle_timeout, None, chats).await
        }

        /// A daemon whose workers run in a Herdr View at `herdr`, wired as production wires them.
        async fn start_in_herdr(
            model: Arc<dyn ModelClient>,
            herdr: std::path::PathBuf,
            chats: usize,
        ) -> Self {
            Self::start_configured(
                model,
                super::support::hosted::IDLE_TIMEOUT,
                Some(herdr),
                chats,
            )
            .await
        }

        async fn start_configured(
            model: Arc<dyn ModelClient>,
            idle_timeout: Duration,
            herdr: Option<std::path::PathBuf>,
            chats: usize,
        ) -> Self {
            let directory = tempfile::Builder::new()
                .prefix("thc")
                .tempdir_in("/private/tmp")
                .unwrap();
            let root = directory.path().canonicalize().unwrap();
            let config_root = root.join("state");
            std::fs::create_dir(&config_root).unwrap();
            let epoch = Uuid::new_v4();
            let controller = Arc::new(HostController::new(
                &root,
                tickhost::paths::HostPaths {
                    runtime: root.join("h"),
                },
                epoch,
                super::support::fixture_cli::tickhost(),
                idle_timeout,
            ));
            let control = Arc::new(WorkerControl::new(&root, epoch, controller.clone()));
            let listener = tokio::net::UnixListener::bind(control.endpoint()).unwrap();
            let factory = Arc::new(GrantedWorkers {
                control: control.clone(),
                model,
                herdr,
                grants: Mutex::new(Vec::new()),
                runtimes: Mutex::new(Vec::new()),
            });
            let daemon = DaemonServer::new(epoch, factory.clone());
            control.attach_daemon(daemon.clone());
            tokio::spawn(control.clone().serve(listener));
            let mut created_chats = Vec::new();
            let mut executions = Vec::new();
            for _ in 0..chats {
                let mut launch = LaunchSpec::embedded(root.clone());
                launch.config_root = Some(config_root.clone());
                let created = daemon
                    .execute(CommandEnvelope {
                        daemon_epoch: epoch,
                        command_id: Uuid::new_v4(),
                        command: AgentCommand::Create {
                            launch,
                            initial: None,
                            origin: None,
                        },
                    })
                    .await
                    .unwrap();
                let Some(CommandResult::Created {
                    snapshot,
                    execution,
                    ..
                }) = created.result
                else {
                    panic!("create failed: {created:?}");
                };
                created_chats.push(snapshot.conversation.id);
                executions.push(execution);
            }
            let grants = factory.grants.lock().await.clone();
            let runtimes = factory.runtimes.lock().await.clone();
            Self {
                _directory: directory,
                root,
                config_root,
                server: daemon,
                epoch,
                chats: created_chats,
                executions,
                grants,
                runtimes,
                controller,
            }
        }

        /// Runs a command the way a View sends it through the daemon to the Chat's worker.
        async fn command(&self, command: AgentCommand) -> CommandResult {
            self.try_command(command).await.unwrap()
        }

        async fn try_command(
            &self,
            command: AgentCommand,
        ) -> Result<CommandResult, tickloop_agent_protocol::AccessError> {
            self.server
                .execute(CommandEnvelope {
                    daemon_epoch: self.epoch,
                    command_id: Uuid::new_v4(),
                    command,
                })
                .await
                .unwrap()
                .into_result()
        }

        /// The host owner of `chat` under this daemon's config root.
        fn tick_owner(&self, chat: Uuid) -> tickhost::host::TickOwner {
            tickhost::host::TickOwner {
                root: root_identity(&self.config_root).unwrap(),
                chat,
            }
        }

        /// Opens a counter Tick for `chat` directly through the daemon's host controller, after
        /// the daemon's first-use reconcile of the root so the Tick is not taken for an orphan.
        async fn open_counter(&self, chat: Uuid) -> tickhost::host::OpenedTick {
            self.controller
                .open(counter_request(self.tick_owner(chat), &self.root))
                .await
                .unwrap()
        }

        /// The owner state file of `chat`, if the registry keeps one.
        fn owner_state(&self, chat: Uuid) -> Option<Value> {
            let path = self
                .config_root
                .join("tickhost/chats")
                .join(chat.to_string())
                .join("owner.json");
            std::fs::read(path)
                .ok()
                .map(|bytes| serde_json::from_slice(&bytes).unwrap())
        }

        /// Hosted Tick tools of the first worker, as a separate client of its grant.
        fn tools(&self) -> HostedTickTools {
            HostedTickTools::new(
                DaemonControl::new(self.grants[0].clone()),
                tick_environment(),
            )
        }

        /// A data connection of the first Chat's running `instance`, with the token its daemon
        /// lists.
        async fn data_connection(&self, instance: &Value) -> HostConnection {
            let listed = DaemonControl::new(self.grants[0].clone())
                .request(list(self.chats[0]))
                .await
                .unwrap();
            let row = listed["items"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| &row["instance"] == instance)
                .unwrap();
            HostConnection::connect(
                Path::new(listed["endpoint"].as_str().unwrap()),
                Role::Instance {
                    instance: serde_json::from_value(instance.clone()).unwrap(),
                    data_token: row["data_token"].as_str().unwrap().to_owned(),
                },
                &[CAPABILITY_DATA],
            )
            .await
            .unwrap()
        }
    }

    impl Drop for Daemon {
        fn drop(&mut self) {
            super::support::stop_tickhost(&self.root.join("h"), &self.root.join("tickhost.key"));
        }
    }

    /// Prepares and executes one hosted Tick tool for `chat` the way an approved model call
    /// would.
    async fn call_tool(
        tools: &HostedTickTools,
        chat: Uuid,
        name: &str,
        input: Value,
    ) -> HostCallResult {
        let prepared = tools
            .prepare_call(chat, name, &input, CancellationToken::new())
            .await
            .unwrap();
        tools
            .execute_prepared(chat, prepared, CancellationToken::new())
            .await
    }

    /// Runs one hosted Tick tool for `chat` that must complete, and returns its answer.
    async fn run_tool(tools: &HostedTickTools, chat: Uuid, name: &str, input: Value) -> Value {
        match call_tool(tools, chat, name, input).await {
            HostCallResult::Completed(result) | HostCallResult::CompletedWithEffect(result, _) => {
                serde_json::from_str(&result.content).unwrap()
            }
            HostCallResult::NotDispatched(error) | HostCallResult::OutcomeUnknown(error) => {
                panic!("hosted {name} failed: {error}")
            }
        }
    }

    /// The ModifyTick input that opens `program` hidden in the daemon's Tickhost.
    fn open_command(daemon: &Daemon, program: Vec<String>) -> Value {
        json!({"action":"open","command":launch_command(program),"cwd":daemon.root,"placement":{"type":"hidden"}})
    }

    fn counter_command(daemon: &Daemon) -> Value {
        open_command(daemon, counter_program())
    }

    /// Submits `text` to `chat` and, once its run completes, returns by call id the tool results
    /// of that run and of any child it delegated to in the foreground.
    async fn run_turn(runtime: &Agent, chat: Uuid, text: &str) -> BTreeMap<String, ToolResult> {
        let mut events = runtime.subscribe();
        let receipt = runtime.submit(chat, None, text.into()).await.unwrap();
        let mut results = BTreeMap::new();
        loop {
            let event =
                tickloop_agent_test_support::next_event_within(&mut events, HOSTED_EVENT_WAIT)
                    .await;
            match event.kind {
                EventKind::ToolCompleted { call_id, result } => {
                    results.insert(call_id, result);
                }
                EventKind::Completed { .. } if event.run_id == receipt.run_id => return results,
                EventKind::Failed(error) if event.run_id == receipt.run_id => panic!("{error}"),
                _ => {}
            }
        }
    }

    #[tokio::test]
    async fn only_a_granted_worker_acts_and_only_for_its_own_chats() {
        // Refused requests never reach a host, and a list starts none: only an open does.
        let daemon = Daemon::start(2).await;
        let (root, chats) = (daemon.root.clone(), daemon.chats.clone());
        let (first, second) = (daemon.grants[0].clone(), daemon.grants[1].clone());

        let foreign = DaemonControl::new(second.clone())
            .request(list(chats[0]))
            .await;
        assert_eq!(
            foreign.unwrap_err().code,
            "Forbidden",
            "another worker's Chat"
        );
        let invented = DaemonControl::new(first.clone())
            .request(list(Uuid::new_v4()))
            .await;
        assert_eq!(invented.unwrap_err().code, "Forbidden", "an invented Chat");
        let mut forged = first.clone();
        forged.capability = "0".repeat(64);
        assert_eq!(
            DaemonControl::new(forged)
                .request(list(chats[0]))
                .await
                .unwrap_err()
                .code,
            "Forbidden"
        );
        let mut earlier = first.clone();
        earlier.daemon_epoch = Uuid::new_v4();
        assert_eq!(
            DaemonControl::new(earlier)
                .request(list(chats[0]))
                .await
                .unwrap_err()
                .code,
            "EpochChanged"
        );

        let listed = DaemonControl::new(first.clone())
            .request(list(chats[0]))
            .await
            .unwrap();
        assert_eq!(listed["total"], 0);
        assert_eq!(
            listed["endpoint"],
            serde_json::json!(root.join("h/host.sock"))
        );
        let listed = DaemonControl::new(second.clone())
            .request(list(chats[1]))
            .await
            .unwrap();
        assert_eq!(listed["total"], 0, "each worker sees its own Chat");

        let elsewhere = DaemonControl::new(second.clone())
            .request(WorkerRequest::OpenHostedTick {
                chat: chats[1],
                operation: Uuid::new_v4(),
                command: vec!["/usr/bin/true".into()],
                cwd: root.clone(),
                environment: Vec::new(),
                title: None,
                admission: HostedAdmission {
                    tool: "ModifyTick".into(),
                    authority: HostToolAuthority::Native {
                        endpoint: HostEndpointIdentity::Tickhost {
                            endpoint: "/elsewhere/host.sock".into(),
                            uid: unsafe { libc::geteuid() },
                        },
                    },
                },
            })
            .await
            .unwrap_err();
        assert_eq!(
            elsewhere.code, "Forbidden",
            "an open prepared for another host is not admitted"
        );
        let reconcile = {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
            let mut stream = tokio::net::UnixStream::connect(&first.endpoint)
                .await
                .unwrap();
            let envelope = json!({"id":1,"envelope":{"daemon_epoch":first.daemon_epoch,"binding":first.binding,"capability":first.capability,"request":{"method":"reconcile","chat":chats[0]}}});
            stream
                .write_all(format!("{envelope}\n").as_bytes())
                .await
                .unwrap();
            let mut line = String::new();
            tokio::io::BufReader::new(stream)
                .read_line(&mut line)
                .await
                .unwrap();
            serde_json::from_str::<Value>(&line).unwrap()
        };
        assert_eq!(
            reconcile["error"]["code"], "InvalidArguments",
            "a worker has no reconcile or other management method: {reconcile}"
        );
        assert!(
            !root.join("h/host.lock").exists(),
            "no request but an admitted open starts a Tickhost"
        );
    }

    #[tokio::test]
    async fn an_open_approved_for_a_replaced_working_directory_is_refused() {
        let directory = tempfile::Builder::new()
            .prefix("thw")
            .tempdir_in("/private/tmp")
            .unwrap();
        let work = directory.path().join("work");
        std::fs::create_dir(&work).unwrap();
        let tools = HostedTickTools::new(
            DaemonControl::new(WorkerControlGrant {
                endpoint: directory.path().join("w.sock"),
                daemon_epoch: Uuid::new_v4(),
                binding: Uuid::new_v4(),
                capability: "0".repeat(64),
                host_endpoint: directory.path().join("host.sock"),
                intent_renewal: std::time::Duration::from_secs(100),
            }),
            Vec::new(),
        );
        let chat = Uuid::new_v4();
        let approved = tools
            .prepare_call(
                chat,
                "ModifyTick",
                &json!({"action":"open","command":["/bin/sh"],"cwd":work,"placement":{"type":"hidden"}}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        std::fs::write(
            work.join("new-entry"),
            "a changed directory is still the same one",
        )
        .unwrap();
        tools
            .revalidate_call(chat, &approved, CancellationToken::new())
            .await
            .expect("new entries do not replace the directory");
        std::fs::rename(&work, directory.path().join("work-aside")).unwrap();
        std::fs::create_dir(&work).unwrap();
        let replaced = tools
            .revalidate_call(chat, &approved, CancellationToken::new())
            .await
            .unwrap_err();
        assert!(replaced.contains("working directory changed"), "{replaced}");
        assert!(matches!(
            tools
                .execute_prepared(chat, approved, CancellationToken::new())
                .await,
            HostCallResult::NotDispatched(_)
        ));
    }

    /// Answers a worker's lifecycle requests the way a lost connection would: the open and the
    /// close are read and then dropped unanswered; their receipts say opened and still dispatched.
    #[tokio::test]
    async fn a_lost_lifecycle_answer_is_settled_only_from_its_receipt() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let directory = tempfile::Builder::new()
            .prefix("thr")
            .tempdir_in("/private/tmp")
            .unwrap();
        let endpoint = directory.path().join("w.sock");
        let listener = tokio::net::UnixListener::bind(&endpoint).unwrap();
        let grant = WorkerControlGrant {
            endpoint,
            daemon_epoch: Uuid::new_v4(),
            binding: Uuid::new_v4(),
            capability: "0".repeat(64),
            host_endpoint: directory.path().join("host.sock"),
            intent_renewal: std::time::Duration::from_secs(100),
        };
        let chat = Uuid::new_v4();
        let instance = tick_protocol::InstanceRef::hosted(Uuid::new_v4());
        let opened = tickhost::host::OpenedTick {
            instance: instance.clone(),
            terminal: tickhost_connect::hosted::HostedTerminal {
                endpoint: directory.path().join("host.sock"),
                epoch: Uuid::new_v4(),
                terminal: Uuid::new_v4(),
            },
            business_pid: 4242,
            data_token: "1".repeat(64),
        };
        let daemon = tokio::spawn(async move {
            let mut requests = Vec::new();
            let mut answers = [
                None,
                Some(json!(tickhost::host::Receipt::Opened(opened))),
                None,
                Some(json!(tickhost::host::Receipt::Dispatched)),
            ]
            .into_iter();
            while let Ok(Ok((stream, _))) =
                tokio::time::timeout(Duration::from_millis(500), listener.accept()).await
            {
                let (reader, mut writer) = stream.into_split();
                let mut line = String::new();
                tokio::io::BufReader::new(reader)
                    .read_line(&mut line)
                    .await
                    .unwrap();
                let frame: Value = serde_json::from_str(&line).unwrap();
                let request = frame["envelope"]["request"].clone();
                requests.push(request.clone());
                if let Some(Some(receipt)) = answers.next() {
                    let answer = json!({"id":frame["id"],"result":{"operation":request["operation"],"receipt":receipt}});
                    writer
                        .write_all(format!("{answer}\n").as_bytes())
                        .await
                        .unwrap();
                }
            }
            requests
        });
        let tools = HostedTickTools::new(DaemonControl::new(grant), Vec::new());
        let settled = call_tool(
            &tools,
            chat,
            "ModifyTick",
            json!({"action":"open","command":["/bin/sh"],"cwd":"/private/tmp","placement":{"type":"hidden"}}),
        )
        .await;
        let HostCallResult::CompletedWithEffect(result, _) = settled else {
            panic!("the opened receipt settles the open");
        };
        assert!(
            result.content.contains(&instance.to_string()),
            "{}",
            result.content
        );
        let unknown = call_tool(
            &tools,
            chat,
            "ModifyTick",
            json!({"action":"close","tick_instance_id":instance}),
        )
        .await;
        let HostCallResult::OutcomeUnknown(message) = unknown else {
            panic!("a receipt still dispatched leaves the close unknown");
        };
        assert!(message.contains("do not repeat"), "{message}");
        let requests = daemon.await.unwrap();
        let methods = requests
            .iter()
            .map(|request| request["method"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            methods,
            [
                "open_hosted_tick",
                "get_hosted_operation_receipt",
                "close_hosted_tick",
                "get_hosted_operation_receipt"
            ],
            "nothing is repeated, only receipts are read"
        );
        assert_eq!(requests[0]["operation"], requests[1]["operation"]);
        assert_eq!(requests[2]["operation"], requests[3]["operation"]);
        assert_eq!(
            requests[0]["admission"]["tool"], "ModifyTick",
            "lifecycle requests carry their admission facts"
        );
    }

    /// Its catalog grows, under a new revision, when `evolve` is called.
    const EVOLVING: &str = r#"
import json, sys
value, revision = 0, 1
tools = [{'name':'increment','description':'Increment','input_schema':{'type':'object'},'mutates':True},{'name':'evolve','description':'Change the catalog','input_schema':{'type':'object'},'mutates':False}]
for line in sys.stdin:
    request = json.loads(line)
    method = request['method']
    if method == 'tick/describe':
        result = {'protocol':3,'revision':str(revision),'kind':'example.evolving','title':'Evolving','description':'Evolving','recover_with':None,'tools':tools}
    elif method == 'tick/read':
        result = {'seq':value,'at':'2026-10-07T00:00:00Z','data':{'value':value}}
    elif request['params'].get('revision') != str(revision):
        print(json.dumps({'jsonrpc':'2.0','id':request['id'],'error':{'code':-32004,'message':'catalog changed'}}), flush=True)
        continue
    else:
        tool = request['params']['tool']
        if tool == 'increment':
            value += 1
        elif tool == 'evolve':
            tools = tools + [{'name':'decrement','description':'Decrement','input_schema':{'type':'object'},'mutates':True}]
            revision += 1
        result = {'value':value}
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}), flush=True)
"#;

    #[tokio::test(flavor = "multi_thread")]
    async fn a_call_approved_before_its_catalog_changed_is_never_dispatched() {
        let daemon = Daemon::start(1).await;
        let chat = daemon.chats[0];
        let tools = daemon.tools();
        let script = daemon.root.join("evolving.py");
        std::fs::write(&script, EVOLVING).unwrap();
        let opened = run_tool(
            &tools,
            chat,
            "ModifyTick",
            open_command(&daemon, python_program(&script, &[])),
        )
        .await;
        let instance = opened["tick_instance_id"].clone();
        let records = launch_records(&daemon.root, chat);
        assert_eq!(records.len(), 1, "the daemon keeps a durable launch record");
        assert_eq!(records[0]["state"], "live", "{}", records[0]);
        assert_eq!(records[0]["instance"], instance);
        let approved = tools
            .prepare_call(
                chat,
                "CallTickTool",
                &json!({"tick_instance_id":instance,"tool":"increment","args":{}}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            approved
                .approval_facts()
                .tick_instance
                .as_ref()
                .map(|id| json!(id)),
            Some(instance.clone())
        );
        assert!(approved.approval_facts().external_declaration.is_some());
        run_tool(
            &tools,
            chat,
            "CallTickTool",
            json!({"tick_instance_id":instance,"tool":"evolve","args":{}}),
        )
        .await;
        let HostCallResult::NotDispatched(changed) = tools
            .execute_prepared(chat, approved, CancellationToken::new())
            .await
        else {
            panic!("a call prepared against the earlier catalog revision is refused unrun");
        };
        assert!(changed.contains("catalog changed"), "{changed}");
        let read = run_tool(
            &tools,
            chat,
            "ReadTick",
            json!({"tick_instance_id":instance}),
        )
        .await;
        assert_eq!(read["data"]["value"], 0, "the stale approval never ran");
        let other_chat = Uuid::new_v4();
        let HostCallResult::NotDispatched(refused) = call_tool(
            &tools,
            other_chat,
            "ReadTick",
            json!({"tick_instance_id":instance}),
        )
        .await
        else {
            panic!("another Chat of this worker never reuses the daemon's binding");
        };
        assert!(refused.contains("Forbidden"), "{refused}");
        assert!(read["tools"].to_string().contains("decrement"));
        let closed = run_tool(
            &tools,
            chat,
            "ModifyTick",
            json!({"action":"close","tick_instance_id":instance}),
        )
        .await;
        assert_eq!(closed["status"], "closed");
        assert!(
            launch_records(&daemon.root, chat).is_empty(),
            "a closed Tick leaves no launch record"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn auto_cleanup_supplies_the_creation_exception_only_for_the_same_loop() {
        use tickloop_agent_protocol::ExecutionMode;
        use tickloop_agent_test_support::{
            ScriptedModel, auto_classification, call, next_event, text,
        };
        for same_loop in [true, false] {
            let model = Arc::new(ScriptedModel::new(Vec::new()));
            let daemon =
                Daemon::start_with(model.clone(), super::support::hosted::IDLE_TIMEOUT, 1).await;
            let (runtime, chat) = (daemon.runtimes[0].clone(), daemon.chats[0]);
            runtime
                .set_execution_mode(chat, ExecutionMode::Auto)
                .await
                .unwrap();
            let cleanup = if same_loop {
                auto_classification(false)
            } else {
                json!({"decision":"block_soft","rule_id":"builtin.soft_deny.irreversible_local_destruction","reason":"Fixture review requires a same-Loop creation receipt for this cleanup exception"})
            };
            let reviewer = Arc::new(ScriptedModel::new(vec![
                text(&auto_classification(false).to_string()),
                text(&cleanup.to_string()),
                text(&cleanup.to_string()),
            ]));
            runtime.set_auto_reviewer(reviewer.clone()).await;
            model.enqueue(call("open", "ModifyTick", counter_command(&daemon)));
            model.enqueue(text("opened"));
            let results = run_turn(
                &runtime,
                chat,
                "Open the task counter and close it after checking it",
            )
            .await;
            let opened: Value = serde_json::from_str(&results["open"].content).unwrap();
            assert!(!results["open"].is_error, "{opened}");
            let instance = opened["tick_instance_id"].clone();
            let loop_id = runtime
                .snapshot(chat)
                .await
                .unwrap()
                .loops
                .last()
                .unwrap()
                .id;
            model.enqueue(call(
                "close",
                "ModifyTick",
                json!({"action":"close","tick_instance_id":instance}),
            ));
            model.enqueue(text("closed"));
            let mut events = runtime.subscribe();
            let receipt = runtime
                .submit(
                    chat,
                    same_loop.then_some(loop_id),
                    "Close the task counter".into(),
                )
                .await
                .unwrap();
            let mut prompted = false;
            loop {
                let event = next_event(&mut events).await;
                match event.kind {
                    EventKind::PermissionRequested { request } => {
                        prompted = true;
                        runtime
                            .answer_permission(chat, request.id, false)
                            .await
                            .unwrap();
                    }
                    EventKind::Completed { .. } | EventKind::Failed(_) | EventKind::Interrupted
                        if event.run_id == receipt.run_id =>
                    {
                        break;
                    }
                    _ => {}
                }
            }
            assert!(!prompted);
            {
                let requests = reviewer.requests.lock().unwrap();
                let tickloop_agent_protocol::Content::Text { text } =
                    &requests[1].messages[1].content[0]
                else {
                    panic!()
                };
                let snapshot: Value = serde_json::from_str(text).unwrap();
                assert_eq!(snapshot["creation_receipt"].is_object(), same_loop);
                assert!(
                    snapshot["auto_policy"]["rules"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|rule| rule["id"] == "builtin.soft_allow.session_created_job_cleanup")
                );
                assert_eq!(snapshot["prepared_facts"]["tick_instance"], instance);
                assert_eq!(snapshot["prepared_facts"]["closes_tick"], true);
            }
            if same_loop {
                assert!(super::gone(opened["business_pid"].as_u64().unwrap() as u32).await);
            } else {
                assert_eq!(
                    run_tool(
                        &daemon.tools(),
                        chat,
                        "ReadTick",
                        json!({"tick_instance_id":instance})
                    )
                    .await["data"]["value"],
                    0
                );
            }
        }
    }

    /// A real root stops its direct child with StopSubagent; the worker's hook tells the daemon, which
    /// checks the family itself and ends only the child's Ticks.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_root_stopping_its_direct_child_ends_only_the_childs_ticks() {
        use tickloop_agent_protocol::{Event, Receipt};
        use tickloop_agent_test_support::routed::{
            RoutedModel, has_tool_result, history_text, state, tool_result,
        };
        const STOP_PROMPT: &str = "Stop the waiter now";
        async fn finished(events: &mut tokio::sync::broadcast::Receiver<Event>, receipt: &Receipt) {
            loop {
                let event = tickloop_agent_test_support::next_event(events).await;
                if event.run_id == receipt.run_id {
                    match event.kind {
                        EventKind::Completed { .. } => return,
                        EventKind::Failed(error) => panic!("run failed: {error}"),
                        _ => {}
                    }
                }
            }
        }
        let model = Arc::new(RoutedModel::new(|request| {
            if !state(request)["delegation"].is_null() {
                if !has_tool_result(request, "ready") {
                    tickloop_agent_test_support::call(
                        "ready",
                        "SendMessage",
                        json!({"to":"parent","message":"Child is ready to wait"}),
                    )
                } else {
                    tickloop_agent_test_support::text("Awaiting family context.")
                }
            } else if !has_tool_result(request, "delegate") {
                tickloop_agent_test_support::call(
                    "delegate",
                    "Subagent",
                    json!({"task":"Wait to be stopped","name":"waiter"}),
                )
            } else if history_text(request).contains(STOP_PROMPT)
                && !has_tool_result(request, "stop")
            {
                tickloop_agent_test_support::call("stop", "StopSubagent", json!({"to":"waiter"}))
            } else {
                tickloop_agent_test_support::text("Done")
            }
        }));
        let daemon = Daemon::start_with(model, super::support::hosted::IDLE_TIMEOUT, 1).await;
        let (runtime, root) = (daemon.runtimes[0].clone(), daemon.chats[0]);
        DaemonControl::new(daemon.grants[0].clone())
            .request(list(root))
            .await
            .unwrap();
        let mut events = runtime.subscribe();
        let receipt = runtime
            .submit(root, None, "Delegate a waiter".into())
            .await
            .unwrap();
        finished(&mut events, &receipt).await;
        let child = runtime.subagents(root).await.unwrap()[0].child_chat_id;
        let child_tick = daemon.open_counter(child).await;
        let root_tick = daemon.open_counter(root).await;
        for (claimed_root, claimed_child) in [(root, child), (root, root)] {
            let refused = DaemonControl::new(daemon.grants[0].clone())
                .request(WorkerRequest::EndStoppedChild {
                    chat: claimed_root,
                    child: claimed_child,
                    operation: Uuid::new_v4(),
                })
                .await
                .unwrap_err();
            assert_eq!(
                refused.code, "Forbidden",
                "only a stopped direct child is ended"
            );
        }
        assert!(super::alive(child_tick.business_pid));

        let receipt = runtime
            .submit(root, None, STOP_PROMPT.into())
            .await
            .unwrap();
        finished(&mut events, &receipt).await;

        let stopped = tool_result(&runtime.snapshot(root).await.unwrap().messages, "stop").unwrap();
        assert_eq!(stopped["status"], "stopped");
        assert!(stopped.get("hosted_ticks_error").is_none(), "{stopped}");
        assert!(
            super::gone(child_tick.business_pid).await,
            "the child's Tick ends"
        );
        assert!(
            super::alive(root_tick.business_pid),
            "the root keeps its Tick"
        );
        assert_eq!(daemon.owner_state(child).unwrap()["state"], "stopped");
        assert_eq!(daemon.owner_state(root), None);
        let tools = daemon.tools();
        let HostCallResult::NotDispatched(refused) =
            call_tool(&tools, child, "ModifyTick", counter_command(&daemon)).await
        else {
            panic!("a stopped child opens nothing before it continues");
        };
        assert!(refused.contains("OwnerEnded"), "{refused}");
    }

    /// A stop whose end never reached the host leaves the Chat Stopping while its Tick runs on.
    /// Continuing the Chat restores only the Chat: the earlier Tick ends before it owns a new one.
    #[tokio::test(flavor = "multi_thread")]
    async fn continuing_a_chat_whose_stop_never_reached_the_host_ends_its_earlier_ticks_first() {
        let model = Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![
            tickloop_agent_test_support::text("Continued"),
        ]));
        let daemon = Daemon::start_with(model, super::support::hosted::IDLE_TIMEOUT, 1).await;
        let (runtime, chat) = (daemon.runtimes[0].clone(), daemon.chats[0]);
        let tools = daemon.tools();
        let earlier = run_tool(&tools, chat, "ModifyTick", counter_command(&daemon)).await;
        let earlier_pid = earlier["business_pid"].as_u64().unwrap() as u32;
        std::fs::write(
            daemon
                .config_root
                .join("tickhost/chats")
                .join(chat.to_string())
                .join("owner.json"),
            serde_json::to_vec(&json!({"state":"stopping","operation":Uuid::new_v4()})).unwrap(),
        )
        .unwrap();
        let HostCallResult::NotDispatched(refused) =
            call_tool(&tools, chat, "ModifyTick", counter_command(&daemon)).await
        else {
            panic!("a stopped Chat opens nothing before it continues");
        };
        assert!(refused.contains("OwnerEnded"), "{refused}");
        assert!(super::alive(earlier_pid), "the end never reached the host");

        let mut events = runtime.subscribe();
        runtime.submit(chat, None, "Continue".into()).await.unwrap();
        loop {
            match tickloop_agent_test_support::next_event(&mut events)
                .await
                .kind
            {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        let reopened = run_tool(&tools, chat, "ModifyTick", counter_command(&daemon)).await;
        assert!(
            super::gone(earlier_pid).await,
            "the Tick of the stopped Chat ends"
        );
        assert!(super::alive(
            reopened["business_pid"].as_u64().unwrap() as u32
        ));
        let records = launch_records(&daemon.root, chat);
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0]["instance"], reopened["tick_instance_id"]);
        assert_eq!(daemon.owner_state(chat).unwrap()["state"], "active");
    }

    /// A Chat's Watch intents go through its daemon, which records them with the launch and
    /// hands them to the host: any one Watch keeps an otherwise idle Tick, the last one ending
    /// lets it idle out, and the next complete listing drops its record. Only the Chat that owns
    /// the instance can protect it.
    #[tokio::test(flavor = "multi_thread")]
    async fn watch_intents_through_the_daemon_keep_a_tick_until_the_last_watch_ends() {
        let daemon = Daemon::start_with(
            Arc::new(tickloop_agent_test_support::ScriptedModel::new(Vec::new())),
            Duration::from_secs(2),
            2,
        )
        .await;
        let (chat, other_chat) = (daemon.chats[0], daemon.chats[1]);
        let tools = daemon.tools();
        let opened = run_tool(&tools, chat, "ModifyTick", counter_command(&daemon)).await;
        let instance: tick_protocol::InstanceRef =
            serde_json::from_value(opened["tick_instance_id"].clone()).unwrap();
        let business_pid = opened["business_pid"].as_u64().unwrap() as u32;
        let watches = |chat, watches: Vec<Uuid>| WorkerRequest::SetWatchIntents {
            chat,
            instance: instance.clone(),
            watches: watches.into_iter().collect(),
        };
        let (first_watch, second_watch) = (Uuid::new_v4(), Uuid::new_v4());
        DaemonControl::new(daemon.grants[0].clone())
            .request(watches(chat, vec![first_watch, second_watch]))
            .await
            .unwrap();
        let recorded = launch_records(&daemon.root, chat);
        assert_eq!(recorded[0]["watches"].as_array().unwrap().len(), 2);
        let foreign = DaemonControl::new(daemon.grants[1].clone())
            .request(watches(chat, Vec::new()))
            .await
            .unwrap_err();
        assert_eq!(foreign.code, "Forbidden", "another worker's Chat");
        let not_its_tick = DaemonControl::new(daemon.grants[1].clone())
            .request(watches(other_chat, Vec::new()))
            .await
            .unwrap_err();
        assert_eq!(not_its_tick.code, "TargetGone", "another Chat's Tick");

        // A worker renews its intents well within the idle timeout, as its grant says.
        let renewal = daemon.grants[0].intent_renewal;
        assert!(renewal < Duration::from_secs(1));
        let renew = |current: Vec<Uuid>, rounds: u32| {
            let request = watches(chat, current);
            let grant = daemon.grants[0].clone();
            async move {
                for _ in 0..rounds {
                    DaemonControl::new(grant.clone())
                        .request(request.clone())
                        .await
                        .unwrap();
                    tokio::time::sleep(renewal).await;
                }
            }
        };
        renew(vec![first_watch, second_watch], 5).await;
        assert!(super::alive(business_pid), "a Watch keeps the idle Tick");
        renew(vec![second_watch], 5).await;
        assert!(super::alive(business_pid), "the remaining Watch keeps it");
        DaemonControl::new(daemon.grants[0].clone())
            .request(watches(chat, Vec::new()))
            .await
            .unwrap();
        assert!(
            super::gone(business_pid).await,
            "it idles out after the last Watch"
        );
        assert_eq!(launch_records(&daemon.root, chat).len(), 1);
        DaemonControl::new(daemon.grants[0].clone())
            .request(list(chat))
            .await
            .unwrap();
        assert!(
            launch_records(&daemon.root, chat).is_empty(),
            "an idled-out Tick cannot be reopened and leaves no record"
        );
    }

    /// A Loop subscribed to a hosted Tick: the quiet Tick costs no model call, its Watch keeps it
    /// from idling, an update wakes the Loop with the new content, and stopping the Loop ends
    /// the Watch so the Tick idles out.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_hosted_tick_update_reaches_the_loop_subscribed_to_it() {
        let model = Arc::new(tickloop_agent_test_support::ScriptedModel::new(Vec::new()));
        let daemon = Daemon::start_with(model.clone(), Duration::from_secs(2), 1).await;
        let (runtime, chat) = (daemon.runtimes[0].clone(), daemon.chats[0]);
        let tools = daemon.tools();
        let opened = run_tool(&tools, chat, "ModifyTick", counter_command(&daemon)).await;
        let instance = opened["tick_instance_id"].clone();
        let business_pid = opened["business_pid"].as_u64().unwrap() as u32;
        model.enqueue(tickloop_agent_test_support::call(
            "subscribe",
            "SubscribeTick",
            json!({"tick_instance_id":instance}),
        ));
        model.enqueue(tickloop_agent_test_support::text("watching"));
        let mut events = runtime.subscribe();
        runtime
            .submit(chat, None, "Watch the counter".into())
            .await
            .unwrap();
        loop {
            match tickloop_agent_test_support::next_event(&mut events)
                .await
                .kind
            {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        let subscriptions = runtime.tick_subscriptions(chat).await.unwrap();
        assert_eq!(subscriptions.len(), 1, "{subscriptions:?}");
        let watched = async {
            for _ in 0..200 {
                let records = launch_records(&daemon.root, chat);
                if records[0]["watches"]
                    .as_array()
                    .is_some_and(|watches| watches.len() == 1)
                {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            false
        };
        assert!(watched.await, "the Watch reached the daemon as an intent");

        tokio::time::sleep(Duration::from_millis(5500)).await;
        assert!(super::alive(business_pid), "the Watch keeps the quiet Tick");
        assert_eq!(
            model.requests.lock().unwrap().len(),
            2,
            "a quiet Tick wakes nothing"
        );
        model.enqueue(tickloop_agent_test_support::text("update observed"));
        run_tool(
            &tools,
            chat,
            "CallTickTool",
            json!({"tick_instance_id":instance,"tool":"increment","args":{"amount":7}}),
        )
        .await;
        loop {
            let event = tokio::time::timeout(high_delivery_wait(), events.recv())
                .await
                .expect("the update wakes the Loop")
                .unwrap();
            match event.kind {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        {
            let requests = model.requests.lock().unwrap();
            assert_eq!(requests.len(), 3);
            let woken = serde_json::to_string(&requests[2].messages).unwrap();
            assert!(
                woken.contains("\\\"value\\\":7") || woken.contains("\"value\":7"),
                "{woken}"
            );
        }

        runtime.stop(chat, subscriptions[0].loop_id).await.unwrap();
        assert!(
            super::gone(business_pid).await,
            "without its Watch the Tick idles out"
        );
    }

    /// A Herdr endpoint that records every request it receives and serves an empty Workspace,
    /// answering anything else as unsupported.
    struct RecordingHerdr {
        socket: std::path::PathBuf,
        methods: Arc<std::sync::Mutex<Vec<String>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl RecordingHerdr {
        fn start(socket: std::path::PathBuf) -> Self {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
            let listener = tokio::net::UnixListener::bind(&socket).unwrap();
            let methods = Arc::new(std::sync::Mutex::new(Vec::new()));
            let recorded = methods.clone();
            let task = tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let mut stream = BufReader::new(stream);
                    let mut line = String::new();
                    if stream.read_line(&mut line).await.is_err() {
                        continue;
                    }
                    let Ok(request) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    let method = request["method"].as_str().unwrap_or_default().to_owned();
                    recorded.lock().unwrap().push(method.clone());
                    let response = if method == "pane.list" {
                        json!({"id":request["id"],"result":{"panes":[]}})
                    } else {
                        json!({"id":request["id"],"error":{"code":"UnknownMethod","message":method}})
                    };
                    let _ = stream
                        .get_mut()
                        .write_all(format!("{response}\n").as_bytes())
                        .await;
                }
            });
            Self {
                socket,
                methods,
                task,
            }
        }

        fn methods(&self) -> Vec<String> {
            self.methods.lock().unwrap().clone()
        }
    }

    impl Drop for RecordingHerdr {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    /// A worker in a Herdr View opens a Tick hidden in the Tickhost unless asked for a
    /// placement: the Tick joins the Chat's Monitor without a subscription or any Herdr layout,
    /// streams to the Loop that subscribes to it, and closes through its host. Only an explicit
    /// placement asks Herdr for a visible Pane.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_herdr_worker_opens_hidden_by_default_and_streams_the_hosted_tick() {
        use tickloop_agent_protocol::ExecutionMode;
        use tickloop_agent_protocol::{ConfigurationChange, EventKind};
        let directory = tempfile::Builder::new()
            .prefix("thh")
            .tempdir_in("/private/tmp")
            .unwrap();
        let herdr = RecordingHerdr::start(directory.path().join("herdr.sock"));
        let model = Arc::new(tickloop_agent_test_support::ScriptedModel::new(Vec::new()));
        let daemon = Daemon::start_in_herdr(model.clone(), herdr.socket.clone(), 1).await;
        let (runtime, chat) = (daemon.runtimes[0].clone(), daemon.chats[0]);
        daemon
            .command(AgentCommand::Configure {
                provider: None,
                execution: daemon.executions[0],
                change: ConfigurationChange::ExecutionMode {
                    mode: ExecutionMode::FullAccess,
                },
            })
            .await;
        let mut open = counter_command(&daemon);
        open.as_object_mut().unwrap().remove("placement");
        model.enqueue(tickloop_agent_test_support::call(
            "open",
            "ModifyTick",
            open,
        ));
        model.enqueue(tickloop_agent_test_support::text("opened"));
        let results = run_turn(&runtime, chat, "Open the counter").await;
        assert!(!results["open"].is_error, "{}", results["open"].content);
        let opened: Value = serde_json::from_str(&results["open"].content).unwrap();
        assert_eq!(opened["status"], "opened", "{opened}");
        assert_eq!(opened["host"], "tickhost", "{opened}");
        assert_eq!(opened["visibility"], "hidden", "{opened}");
        let instance = opened["tick_instance_id"].as_str().unwrap().to_owned();
        assert!(instance.starts_with("hosted:"), "{instance}");
        assert!(
            herdr.methods().is_empty(),
            "a hidden open asks Herdr nothing: {:?}",
            herdr.methods()
        );
        let members = runtime.snapshot(chat).await.unwrap().monitor.members;
        assert_eq!(
            members
                .iter()
                .map(|member| (member.instance.as_str(), member.title.as_deref()))
                .collect::<Vec<_>>(),
            vec![(instance.as_str(), Some("Counter"))],
            "the hidden Tick joins this Chat's Monitor under its declared title"
        );
        assert!(
            runtime.tick_subscriptions(chat).await.unwrap().is_empty(),
            "opening never subscribes"
        );

        model.enqueue(tickloop_agent_test_support::call(
            "subscribe",
            "SubscribeTick",
            json!({"tick_instance_id":instance}),
        ));
        model.enqueue(tickloop_agent_test_support::text("watching"));
        let results = run_turn(&runtime, chat, "Watch the counter").await;
        assert!(
            !results["subscribe"].is_error,
            "{}",
            results["subscribe"].content
        );
        assert_eq!(runtime.tick_subscriptions(chat).await.unwrap().len(), 1);
        let mut events = runtime.subscribe();
        model.enqueue(tickloop_agent_test_support::text("update observed"));
        run_tool(
            &daemon.tools(),
            chat,
            "CallTickTool",
            json!({"tick_instance_id":instance,"tool":"increment","args":{"amount":7}}),
        )
        .await;
        loop {
            let event = tokio::time::timeout(high_delivery_wait(), events.recv())
                .await
                .expect("the hosted update wakes the Loop")
                .unwrap();
            match event.kind {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        let woken = serde_json::to_string(&model.requests.lock().unwrap()[4].messages).unwrap();
        assert!(
            woken.contains("\\\"value\\\":7") || woken.contains("\"value\":7"),
            "{woken}"
        );

        model.enqueue(tickloop_agent_test_support::call(
            "visible",
            "ModifyTick",
            json!({"action":"open","command":["/bin/cat"],"cwd":daemon.root,"workspace_id":HERDR_WORKSPACE,"placement":{"type":"new_tab"}}),
        ));
        model.enqueue(tickloop_agent_test_support::call(
            "close",
            "ModifyTick",
            json!({"action":"close","tick_instance_id":instance}),
        ));
        model.enqueue(tickloop_agent_test_support::text("done"));
        let results = run_turn(&runtime, chat, "Show cat, then close the counter").await;
        assert!(
            !herdr.methods().is_empty(),
            "an explicit placement opens through Herdr"
        );
        assert!(
            !results["visible"].content.contains("tickhost"),
            "{}",
            results["visible"].content
        );
        let closed: Value = serde_json::from_str(&results["close"].content).unwrap();
        assert_eq!(closed["status"], "closed", "{closed}");
        assert_eq!(closed["host"], "tickhost", "{closed}");
        assert_eq!(closed["tick_instance_id"], instance);
    }

    /// A child delegated in a Herdr worker inherits both backends' authorities for its tools,
    /// yet hosted Ticks stay with the Chat that opened them: the child cannot open one, and
    /// reading its parent's is refused by ownership rather than by a Herdr Workspace check.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_delegated_child_cannot_open_or_use_its_parents_hosted_tick() {
        use tickloop_agent_protocol::ConfigurationChange;
        use tickloop_agent_protocol::ExecutionMode;
        let directory = tempfile::Builder::new()
            .prefix("thd")
            .tempdir_in("/private/tmp")
            .unwrap();
        let herdr = RecordingHerdr::start(directory.path().join("herdr.sock"));
        let model = Arc::new(tickloop_agent_test_support::ScriptedModel::new(Vec::new()));
        let daemon = Daemon::start_in_herdr(model.clone(), herdr.socket.clone(), 1).await;
        let (runtime, chat) = (daemon.runtimes[0].clone(), daemon.chats[0]);
        daemon
            .command(AgentCommand::Configure {
                provider: None,
                execution: daemon.executions[0],
                change: ConfigurationChange::ExecutionMode {
                    mode: ExecutionMode::FullAccess,
                },
            })
            .await;
        let parents = daemon.tools();
        let opened = run_tool(&parents, chat, "ModifyTick", counter_command(&daemon)).await;
        let instance = opened["tick_instance_id"].clone();
        let mut child_open = counter_command(&daemon);
        child_open.as_object_mut().unwrap().remove("placement");
        for reply in [
            tickloop_agent_test_support::call(
                "delegate",
                "Subagent",
                json!({"task":"Inspect the counter."}),
            ),
            tickloop_agent_test_support::call(
                "child-read",
                "ReadTick",
                json!({"tick_instance_id":instance}),
            ),
            tickloop_agent_test_support::call("child-open", "ModifyTick", child_open),
            tickloop_agent_test_support::text("The child is done."),
            tickloop_agent_test_support::text("Delegated."),
        ] {
            model.enqueue(reply);
        }
        let mut events = runtime.subscribe();
        let results = run_turn(&runtime, chat, "Delegate the inspection").await;
        let child = loop {
            let event = events.try_recv().unwrap();
            if event.chat_id != chat {
                break event.chat_id;
            }
        };
        assert!(
            !results["delegate"].is_error,
            "{}",
            results["delegate"].content
        );
        let read = &results["child-read"];
        assert!(read.is_error, "{}", read.content);
        assert!(
            read.content
                .contains("not a running hosted Tick of this Chat"),
            "{}",
            read.content
        );
        let open = &results["child-open"];
        assert!(open.is_error, "{}", open.content);
        assert!(
            open.content.contains("Unsupported") && open.content.contains("launch"),
            "{}",
            open.content
        );
        let ceiling = serde_json::to_value(
            &runtime
                .snapshot(child)
                .await
                .unwrap()
                .delegation
                .unwrap()
                .ceiling
                .host_tools,
        )
        .unwrap();
        for (tool, backends) in [
            ("ReadTick", 2),
            ("CallTickTool", 2),
            ("ModifyTick", 2),
            ("ReadWorkspace", 1),
        ] {
            assert_eq!(
                ceiling[tool].as_array().map(Vec::len),
                Some(backends),
                "{tool}: {ceiling}"
            );
        }
        let read = run_tool(
            &parents,
            chat,
            "ReadTick",
            json!({"tick_instance_id":instance}),
        )
        .await;
        assert_eq!(read["data"]["value"], 0, "the parent's Tick is untouched");
    }

    /// A model reopens a Tick lost with its host through ModifyTick: the reopen waits for
    /// approval like any change, starts a new hidden instance in place of the lost one, and joins
    /// the Chat's Monitor under the lost Tick's title without any subscription.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_model_reopens_a_lost_tick_only_after_approval_as_a_new_instance() {
        let model = Arc::new(tickloop_agent_test_support::ScriptedModel::new(Vec::new()));
        let daemon =
            Daemon::start_with(model.clone(), super::support::hosted::IDLE_TIMEOUT, 1).await;
        let (runtime, chat) = (daemon.runtimes[0].clone(), daemon.chats[0]);
        let tools = daemon.tools();
        let opened = run_tool(&tools, chat, "ModifyTick", counter_command(&daemon)).await;
        let lost = opened["tick_instance_id"].as_str().unwrap().to_owned();
        let lost_title = launch_records(&daemon.root, chat)[0]["title"].clone();
        let business_pid = opened["business_pid"].as_u64().unwrap() as u32;
        let launcher = tickhost::process::sample(business_pid).unwrap().parent;
        let host = tickhost::process::sample(launcher).unwrap().parent;
        unsafe { libc::kill(host as libc::pid_t, libc::SIGKILL) };
        assert!(super::gone(business_pid).await);
        let host_paths = tickhost::paths::HostPaths {
            runtime: daemon.root.join("h"),
        };

        model.enqueue(tickloop_agent_test_support::call(
            "reopen",
            "ModifyTick",
            json!({"action":"reopen","tick_instance_id":lost}),
        ));
        model.enqueue(tickloop_agent_test_support::text("reopened"));
        let mut events = runtime.subscribe();
        let receipt = runtime
            .submit(chat, None, "Reopen the counter".into())
            .await
            .unwrap();
        let mut result = None;
        loop {
            let event = tickloop_agent_test_support::next_event(&mut events).await;
            match event.kind {
                EventKind::PermissionRequested { request } => {
                    assert_eq!(request.input["action"], "reopen");
                    assert!(
                        !host_paths.host_running().unwrap(),
                        "nothing starts before approval"
                    );
                    runtime
                        .answer_permission(chat, request.id, true)
                        .await
                        .unwrap();
                }
                EventKind::ToolCompleted {
                    result: completed, ..
                } => result = Some(completed),
                EventKind::Completed { .. } if event.run_id == receipt.run_id => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        let result = result.unwrap();
        assert!(!result.is_error, "{}", result.content);
        let reopened: Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(reopened["status"], "opened", "{reopened}");
        assert_eq!(reopened["visibility"], "hidden");
        assert_eq!(reopened["replaces"], lost);
        let instance = reopened["tick_instance_id"].as_str().unwrap();
        assert_ne!(instance, lost, "a new instance");
        assert_ne!(reopened["business_pid"], opened["business_pid"]);
        let member = runtime
            .snapshot(chat)
            .await
            .unwrap()
            .monitor
            .members
            .into_iter()
            .find(|member| member.instance.as_str() == instance)
            .expect("the reopened Tick joins Monitor");
        assert!(lost_title.is_string(), "the lost Tick keeps a title");
        assert_eq!(
            member.title.as_deref(),
            Some("Counter"),
            "the reopened Tick is named by its declaration: {member:?}"
        );
        assert!(
            runtime.tick_subscriptions(chat).await.unwrap().is_empty(),
            "no subscription follows a reopen"
        );
        run_tool(
            &tools,
            chat,
            "ModifyTick",
            json!({"action":"close","tick_instance_id":instance}),
        )
        .await;
    }

    /// Two Loops watch one hosted Tick: cancelling one keeps the other's Watch and deliveries,
    /// and once the Tick's host dies the remaining Watch fails closed as gone, calling no model
    /// and never rebinding to a later host.
    #[tokio::test(flavor = "multi_thread")]
    async fn cancelling_one_hosted_watch_keeps_the_other_and_a_dead_host_fails_closed() {
        let model = Arc::new(tickloop_agent_test_support::ScriptedModel::new(Vec::new()));
        let daemon =
            Daemon::start_with(model.clone(), super::support::hosted::IDLE_TIMEOUT, 1).await;
        let (runtime, chat) = (daemon.runtimes[0].clone(), daemon.chats[0]);
        let tools = daemon.tools();
        let opened = run_tool(&tools, chat, "ModifyTick", counter_command(&daemon)).await;
        let instance = opened["tick_instance_id"].clone();
        let mut events = runtime.subscribe();
        let run = |name: &'static str| {
            model.enqueue(tickloop_agent_test_support::call(
                name,
                "SubscribeTick",
                json!({"tick_instance_id":instance}),
            ));
            model.enqueue(tickloop_agent_test_support::text("watching"));
        };
        for (name, prompt) in [("a", "Watch it as A"), ("b", "Watch it as B")] {
            run(name);
            let receipt = runtime.submit(chat, None, prompt.into()).await.unwrap();
            loop {
                let event = tickloop_agent_test_support::next_event(&mut events).await;
                if event.run_id == receipt.run_id
                    && matches!(event.kind, EventKind::Completed { .. })
                {
                    break;
                }
            }
        }
        let subscriptions = runtime.tick_subscriptions(chat).await.unwrap();
        assert_eq!(subscriptions.len(), 2, "{subscriptions:?}");
        let watch_count = |count: usize| {
            let root = daemon.root.clone();
            async move {
                for _ in 0..200 {
                    if launch_records(&root, chat)[0]["watches"]
                        .as_array()
                        .is_some_and(|watches| watches.len() == count)
                    {
                        return true;
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                false
            }
        };
        assert!(watch_count(2).await);
        runtime
            .update_tick_subscription(chat, subscriptions[0].id, TickSubscriptionChange::Cancel)
            .await
            .unwrap();
        assert!(watch_count(1).await, "only the cancelled Watch ended");

        let before = model.requests.lock().unwrap().len();
        model.enqueue(tickloop_agent_test_support::text("update observed"));
        run_tool(
            &tools,
            chat,
            "CallTickTool",
            json!({"tick_instance_id":instance,"tool":"increment","args":{"amount":1}}),
        )
        .await;
        loop {
            let event = tokio::time::timeout(high_delivery_wait(), events.recv())
                .await
                .expect("the remaining Watch is delivered")
                .unwrap();
            if matches!(event.kind, EventKind::Completed { .. }) {
                break;
            }
        }
        assert_eq!(model.requests.lock().unwrap().len(), before + 1);

        let business_pid = opened["business_pid"].as_u64().unwrap() as u32;
        let launcher = tickhost::process::sample(business_pid).unwrap().parent;
        let host = tickhost::process::sample(launcher).unwrap().parent;
        unsafe { libc::kill(host as libc::pid_t, libc::SIGKILL) };
        assert!(super::gone(business_pid).await);
        let remaining = subscriptions[1].id;
        let mut is_gone = false;
        for _ in 0..120 {
            let link = runtime
                .tick_subscriptions(chat)
                .await
                .unwrap()
                .into_iter()
                .find(|subscription| subscription.id == remaining)
                .map(|subscription| subscription.link);
            if link == Some(TickLinkState::InstanceGone) {
                is_gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        assert!(is_gone, "the Watch of a Tick whose host died fails closed");
        run_tool(&tools, chat, "ModifyTick", counter_command(&daemon)).await;
        tokio::time::sleep(Duration::from_secs(6)).await;
        assert_eq!(
            model.requests.lock().unwrap().len(),
            before + 1,
            "nothing reached the model after the host died"
        );
    }

    /// Answers `tick/describe` with an error while `refuse-describe` exists in its working
    /// directory, and logs every describe it receives as `refused` or `served`.
    const REFUSING_TICK: &str = r#"
import json, os, sys
for line in sys.stdin:
    request = json.loads(line)
    if request['method'] == 'tick/describe':
        refused = os.path.exists('refuse-describe')
        with open('describes', 'a') as log:
            log.write('refused\n' if refused else 'served\n')
        if refused:
            print(json.dumps({'jsonrpc':'2.0','id':request['id'],'error':{'code':-32000,'message':'describe refused'}}), flush=True)
            continue
        result = {'protocol':3,'revision':'1','kind':'test.refusing','title':'Refusing','description':'Refuses describe on request','recover_with':None,'tools':[]}
    else:
        result = {'seq':0,'at':'2026-10-08T00:00:00Z','data':{}}
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}), flush=True)
"#;

    const REFUSE_DESCRIBE: &str = "refuse-describe";

    fn logged(log: &Path, outcome: &str) -> usize {
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .filter(|line| *line == outcome)
            .count()
    }

    /// A hosted Watch whose registration keeps failing connects once and reconnects three times
    /// in this Agent process, then stays Disconnected even after its Tick can be watched again;
    /// nothing wakes the Loop meanwhile. Only subscribing again admits a new Watch that reaches
    /// the Tick.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_watch_that_used_its_reconnects_stays_disconnected_until_subscribed_again() {
        assert_eq!(
            (RECONNECTS_PER_WATCH, RECONNECT_PAUSE),
            (3, Duration::from_secs(5)),
            "the production reconnect budget"
        );
        let model = Arc::new(tickloop_agent_test_support::ScriptedModel::new(Vec::new()));
        let daemon =
            Daemon::start_with(model.clone(), super::support::hosted::IDLE_TIMEOUT, 1).await;
        let (runtime, chat) = (daemon.runtimes[0].clone(), daemon.chats[0]);
        let script = daemon.root.join("refusing.py");
        std::fs::write(&script, REFUSING_TICK).unwrap();
        let opened = run_tool(
            &daemon.tools(),
            chat,
            "ModifyTick",
            open_command(&daemon, python_program(&script, &[])),
        )
        .await;
        let instance = opened["tick_instance_id"].clone();
        let business_pid = opened["business_pid"].as_u64().unwrap() as u32;
        let describes = daemon.root.join("describes");
        let served_before = logged(&describes, "served");
        std::fs::write(daemon.root.join(REFUSE_DESCRIBE), "").unwrap();

        model.enqueue(tickloop_agent_test_support::call(
            "subscribe",
            "SubscribeTick",
            json!({"tick_instance_id":instance}),
        ));
        model.enqueue(tickloop_agent_test_support::text("watching"));
        let results = run_turn(&runtime, chat, "Watch it").await;
        let subscribed: Value = serde_json::from_str(&results["subscribe"].content).unwrap();
        assert_eq!(subscribed["ready"], false, "{subscribed}");
        let _ = tokio::time::timeout(HOSTED_EVENT_WAIT, async {
            while logged(&describes, "refused") < CONNECTS_PER_WATCH {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        })
        .await;
        std::fs::remove_file(daemon.root.join(REFUSE_DESCRIBE)).unwrap();
        tokio::time::sleep(RECONNECT_PAUSE + Duration::from_secs(2)).await;

        assert_eq!(
            logged(&describes, "refused"),
            CONNECTS_PER_WATCH,
            "the Watch connects once and reconnects three times"
        );
        assert_eq!(
            logged(&describes, "served"),
            served_before,
            "a Watch out of reconnects never tries the recovered Tick"
        );
        let rows = runtime.tick_subscriptions(chat).await.unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].link, TickLinkState::Disconnected);
        assert_eq!(model.requests.lock().unwrap().len(), 2, "nothing woke");
        assert!(super::alive(business_pid), "the Tick keeps running");

        runtime
            .update_tick_subscription(chat, rows[0].id, TickSubscriptionChange::Cancel)
            .await
            .unwrap();
        model.enqueue(tickloop_agent_test_support::call(
            "again",
            "SubscribeTick",
            json!({"tick_instance_id":instance}),
        ));
        model.enqueue(tickloop_agent_test_support::text("watching again"));
        let results = run_turn(&runtime, chat, "Watch it again").await;
        let again: Value = serde_json::from_str(&results["again"].content).unwrap();
        assert_eq!(again["ready"], true, "{again}");
        assert_eq!(
            logged(&describes, "served"),
            served_before + 1,
            "the new Watch registered at once"
        );
        let rows = runtime.tick_subscriptions(chat).await.unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].link, TickLinkState::Live);
    }

    /// Numbers in the update `grow` publishes: written as `1e15` the update line stays within
    /// the 512 KiB Tick Update limit, while each re-encodes as `1000000000000000.0`, far over it.
    const GROWN_VALUES: usize = 100_000;

    const GROWING_TICK: &str = r#"
import json, sys
for line in sys.stdin:
    request = json.loads(line)
    method = request['method']
    if method == 'tick/describe':
        result = {'protocol':3,'revision':'1','kind':'test.growing','title':'Growing','description':'Publishes one large update','recover_with':None,'tools':[{'name':'grow','description':'Publish a large update','input_schema':{'type':'object'},'mutates':False}]}
    elif method == 'tick/read':
        result = {'seq':0,'at':'2026-10-08T00:00:00Z','data':{}}
    else:
        values = ','.join(['1e15'] * int(sys.argv[1]))
        sys.stdout.write('{"jsonrpc":"2.0","method":"tick/update","params":{"seq":1,"at":"2026-10-08T00:00:01Z","data":{"values":[%s]}}}\n' % values)
        result = {'grown':True}
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}), flush=True)
"#;

    /// The position in `seen` of the first event from `since` on that `wanted` matches; events
    /// are received into `seen` until one does.
    async fn wait_for(
        events: &mut mpsc::Receiver<TickStreamEvent>,
        seen: &mut Vec<TickStreamEvent>,
        since: usize,
        what: &str,
        wanted: impl Fn(&TickStreamEvent) -> bool,
    ) -> usize {
        if let Some(position) = seen[since..].iter().position(&wanted) {
            return since + position;
        }
        tokio::time::timeout(HOSTED_EVENT_WAIT, async {
            loop {
                let event = events.recv().await.expect("the stream stays open");
                let found = wanted(&event);
                seen.push(event);
                if found {
                    return seen.len() - 1;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{what} never arrived: {seen:#?}"))
    }

    fn is_ready(event: &TickStreamEvent, watch: Uuid) -> bool {
        matches!(event, TickStreamEvent::Ready { watch: ready } if *ready == watch)
    }

    fn is_state(event: &TickStreamEvent, watch: Uuid, wanted: TickConnectionState) -> bool {
        matches!(event, TickStreamEvent::State { state, audience, .. } if *state == wanted && audience.contains(&watch))
    }

    /// An update over the 512 KiB limit once encoded, though its line is within it, is a
    /// protocol violation of its own Tick: the host never forwards it, the call that published
    /// it has an unknown outcome, and that Tick's Watch becomes IdentityUncertain without
    /// delivering it, while the Watch of another Tick in the same stream keeps delivering its
    /// updates and that Tick keeps running.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_update_over_the_limit_once_encoded_ends_only_its_own_ticks_link() {
        let daemon = Daemon::start(1).await;
        let chat = daemon.chats[0];
        let tools = Arc::new(daemon.tools());
        let script = daemon.root.join("growing.py");
        std::fs::write(&script, GROWING_TICK).unwrap();
        let growing = run_tool(
            &tools,
            chat,
            "ModifyTick",
            open_command(
                &daemon,
                python_program(&script, &[GROWN_VALUES.to_string()]),
            ),
        )
        .await;
        let counter = run_tool(&tools, chat, "ModifyTick", counter_command(&daemon)).await;
        let (large, small) = (Uuid::new_v4(), Uuid::new_v4());
        let stream = HostedTickStream::new(tools.clone());
        let authority = tools
            .tool_authorities("ReadTick")
            .unwrap()
            .pop_first()
            .unwrap();
        stream.set_watch_chats(BTreeMap::from([(large, chat), (small, chat)]));
        stream.set_watch_authorities(BTreeMap::from([
            (large, authority.clone()),
            (small, authority),
        ]));
        let mut handle = stream.open();
        handle.desired.send_replace(BTreeMap::from([
            (
                large,
                serde_json::from_value(growing["tick_instance_id"].clone()).unwrap(),
            ),
            (
                small,
                serde_json::from_value(counter["tick_instance_id"].clone()).unwrap(),
            ),
        ]));
        let mut seen = Vec::new();
        for watch in [large, small] {
            wait_for(
                &mut handle.events,
                &mut seen,
                0,
                "a Watch's baseline",
                |event| is_ready(event, watch),
            )
            .await;
        }
        let mut raw = daemon.data_connection(&growing["tick_instance_id"]).await;
        raw.request("watch", json!({}), CONTROL_TIMEOUT)
            .await
            .unwrap();

        let grown = seen.len();
        let settled = match call_tool(
            &tools,
            chat,
            "CallTickTool",
            json!({"tick_instance_id":growing["tick_instance_id"],"tool":"grow","args":{}}),
        )
        .await
        {
            HostCallResult::OutcomeUnknown(_) => None,
            HostCallResult::Completed(result) | HostCallResult::CompletedWithEffect(result, _) => {
                Some(format!("completed: {}", result.content))
            }
            HostCallResult::NotDispatched(reason) => Some(format!("not dispatched: {reason}")),
        };
        assert_eq!(
            settled, None,
            "the call whose Tick broke the protocol mid-call has an unknown outcome"
        );
        let forwarded = tokio::time::timeout(HOSTED_EVENT_WAIT, raw.next_event())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(
            forwarded["observation"]["state"]["ProtocolError"]
                .as_str()
                .is_some_and(|reason| reason.contains("once encoded")),
            "the host refuses the update as its Tick's protocol violation: {}",
            forwarded["observation"]["state"]
        );
        assert!(
            forwarded["observation"]["update"]["data"]["values"].is_null(),
            "the refused update is never forwarded"
        );
        drop(raw);
        let refused = wait_for(
            &mut handle.events,
            &mut seen,
            grown,
            "the large Watch losing its Tick's identity",
            |event| is_state(event, large, TickConnectionState::IdentityUncertain),
        )
        .await;

        run_tool(
            &tools,
            chat,
            "CallTickTool",
            json!({"tick_instance_id":counter["tick_instance_id"],"tool":"increment","args":{"amount":1}}),
        )
        .await;
        wait_for(
            &mut handle.events,
            &mut seen,
            refused,
            "the other Tick's update",
            |event| {
                matches!(event, TickStreamEvent::Observation { audience, value } if audience.contains(&small) && value.data["value"] == 1)
            },
        )
        .await;
        assert!(
            !seen.iter().any(|event| matches!(event, TickStreamEvent::Observation { audience, .. } if audience.contains(&large))),
            "the refused update is never delivered: {seen:#?}"
        );
        assert!(
            super::alive(counter["business_pid"].as_u64().unwrap() as u32),
            "the other Tick keeps running"
        );
    }

    /// A View's Monitor lists its Chat's hosted Ticks with a process sample of each and closes
    /// the one its user picks; the other keeps running.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_monitor_lists_hosted_ticks_with_samples_and_closes_the_one_picked() {
        let daemon = Daemon::start(1).await;
        let (chat, execution) = (daemon.chats[0], daemon.executions[0]);
        let tools = daemon.tools();
        let closed = run_tool(&tools, chat, "ModifyTick", counter_command(&daemon)).await;
        let kept = run_tool(&tools, chat, "ModifyTick", counter_command(&daemon)).await;
        let CommandResult::HostedTicks(members) = daemon
            .command(AgentCommand::ListHostedTicks { execution })
            .await
        else {
            panic!("hosted Ticks expected");
        };
        assert_eq!(members.len(), 2);
        for opened in [&closed, &kept] {
            let member = members
                .iter()
                .find(|member| json!(member.instance) == opened["tick_instance_id"])
                .unwrap();
            let sample = member.resources.as_ref().expect("a process sample");
            assert_eq!(json!(sample.business_pid), opened["business_pid"]);
            assert!(sample.rss_bytes.is_some_and(|bytes| bytes > 0));
            assert_eq!(member.updated_at_ms, None, "no Tick Update yet");
        }
        run_tool(
            &tools,
            chat,
            "CallTickTool",
            json!({"tick_instance_id":kept["tick_instance_id"],"tool":"increment","args":{"amount":1}}),
        )
        .await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let members = loop {
            let CommandResult::HostedTicks(members) = daemon
                .command(AgentCommand::ListHostedTicks { execution })
                .await
            else {
                panic!("hosted Ticks expected");
            };
            let kept_updated = members.iter().any(|member| {
                json!(member.instance) == kept["tick_instance_id"] && member.updated_at_ms.is_some()
            });
            if kept_updated || std::time::Instant::now() > deadline {
                break members;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        };
        let updated = |opened: &serde_json::Value| {
            members
                .iter()
                .find(|member| json!(member.instance) == opened["tick_instance_id"])
                .unwrap()
                .updated_at_ms
        };
        assert!(
            updated(&kept).is_some(),
            "the host knows when a Tick nobody subscribes to last updated"
        );
        assert_eq!(updated(&closed), None);
        let instance = serde_json::from_value(closed["tick_instance_id"].clone()).unwrap();
        assert!(matches!(
            daemon
                .command(AgentCommand::CloseHostedTick {
                    execution,
                    instance,
                })
                .await,
            CommandResult::Done
        ));
        assert!(super::gone(closed["business_pid"].as_u64().unwrap() as u32).await);
        assert!(super::alive(kept["business_pid"].as_u64().unwrap() as u32));
        let records = launch_records(&daemon.root, chat);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["instance"], kept["tick_instance_id"]);
    }

    /// More hosted Ticks than one page holds list completely and exactly once: the daemon hands
    /// its worker the host's listing at most 128 rows a page, and the worker and a View's Monitor
    /// walk every page.
    #[tokio::test(flavor = "multi_thread")]
    async fn more_hosted_ticks_than_a_page_list_completely_through_daemon_and_worker() {
        use std::collections::HashSet;
        use tickhost_connect::page::MAX_PAGE_SIZE;
        use tickloop_agent_protocol::HostedMemberState;
        use tickloop_agent_runtime::hosted::worker_control::HostedTickPage;
        const TICKS: usize = MAX_PAGE_SIZE + 2;
        const OPENS_AT_ONCE: usize = 26;
        let daemon = Daemon::start(1).await;
        let (chat, execution) = (daemon.chats[0], daemon.executions[0]);
        let control = DaemonControl::new(daemon.grants[0].clone());
        control.request(list(chat)).await.unwrap();
        let mut opened = Vec::new();
        while opened.len() < TICKS {
            let mut opens = tokio::task::JoinSet::new();
            for _ in 0..OPENS_AT_ONCE.min(TICKS - opened.len()) {
                let controller = daemon.controller.clone();
                let request = counter_request(daemon.tick_owner(chat), &daemon.root);
                opens.spawn(async move { controller.open(request).await });
            }
            while let Some(result) = opens.join_next().await {
                opened.push(result.unwrap().unwrap());
            }
        }
        let expected = opened
            .iter()
            .map(|tick| tick.instance.clone())
            .collect::<HashSet<_>>();
        assert_eq!(expected.len(), TICKS);

        let mut paged = Vec::new();
        let mut pages = 0;
        let mut cursor = None;
        loop {
            let page: HostedTickPage = serde_json::from_value(
                control
                    .request(WorkerRequest::ListHostedTicks { chat, cursor })
                    .await
                    .unwrap(),
            )
            .unwrap();
            pages += 1;
            assert!(page.page.items.len() <= MAX_PAGE_SIZE);
            assert_eq!(page.page.total, TICKS, "every page counts the full set");
            paged.extend(page.page.items.into_iter().filter_map(|row| row.instance));
            match page.page.next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        assert_eq!(pages, 2);
        assert_eq!(paged.len(), TICKS, "no row is listed twice");
        assert_eq!(paged.into_iter().collect::<HashSet<_>>(), expected);

        let tools = daemon.tools();
        let members = tools.members(chat).await.unwrap();
        let CommandResult::HostedTicks(viewed) = daemon
            .command(AgentCommand::ListHostedTicks { execution })
            .await
        else {
            panic!("hosted Ticks expected");
        };
        for listed in [members, viewed] {
            assert_eq!(listed.len(), TICKS, "no member is listed twice");
            assert!(
                listed
                    .iter()
                    .all(|member| member.state == HostedMemberState::Running)
            );
            assert_eq!(
                listed
                    .into_iter()
                    .map(|member| member.instance)
                    .collect::<HashSet<_>>(),
                expected
            );
        }

        super::support::stop_tickhost(&daemon.root.join("h"), &daemon.root.join("tickhost.key"));
        for tick in &opened {
            assert!(super::gone(tick.business_pid).await, "every Tick ends");
        }
    }

    /// A View reads its Chat's hosted Tick as a screen snapshot and attaches through a one-time
    /// view grant from the daemon; neither hands it the Tick's data token, and another Chat's Tick
    /// is refused.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_view_gets_a_snapshot_and_a_one_time_attach_grant_for_its_chats_tick_only() {
        use tickhost_connect::hosted::{CAPABILITY_DATA, CONTROL_TIMEOUT, HostConnection, Role};
        let daemon = Daemon::start(2).await;
        let (chat, execution, other) =
            (daemon.chats[0], daemon.executions[0], daemon.executions[1]);
        let tools = daemon.tools();
        let opened = run_tool(&tools, chat, "ModifyTick", counter_command(&daemon)).await;
        let instance: tick_protocol::InstanceRef =
            serde_json::from_value(opened["tick_instance_id"].clone()).unwrap();
        let snapshot = loop {
            let CommandResult::HostedTickSnapshot(snapshot) = daemon
                .command(AgentCommand::HostedTickSnapshot {
                    execution,
                    instance: instance.clone(),
                })
                .await
            else {
                panic!("a snapshot expected");
            };
            if snapshot
                .screen
                .lines
                .iter()
                .any(|line| line.contains("Counter: 0"))
            {
                break snapshot;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        assert!(!snapshot.screen.truncated);
        assert!(snapshot.taken_at_ms > 0);
        for command in [
            AgentCommand::HostedTickSnapshot {
                execution: other,
                instance: instance.clone(),
            },
            AgentCommand::AttachHostedTick {
                execution: other,
                instance: instance.clone(),
            },
        ] {
            assert!(
                daemon.try_command(command).await.is_err(),
                "another Chat's Tick is refused"
            );
        }
        let CommandResult::HostedTickView(grant) = daemon
            .command(AgentCommand::AttachHostedTick {
                execution,
                instance: instance.clone(),
            })
            .await
        else {
            panic!("a view grant expected");
        };
        assert_eq!(grant.instance, instance);
        let viewer = || {
            HostConnection::connect(
                &grant.endpoint,
                Role::Viewer {
                    instance: grant.instance.clone(),
                    view: grant.view,
                },
                &[CAPABILITY_DATA],
            )
        };
        let mut view = viewer().await.unwrap();
        assert_eq!(view.epoch(), grant.host_epoch);
        let attached = view
            .request(
                "attach",
                json!({"size":{"rows":24,"columns":80}}),
                CONTROL_TIMEOUT,
            )
            .await
            .unwrap();
        assert_eq!(attached["attached"], true);
        let frame = view.next_event().await.unwrap().unwrap();
        assert_eq!(frame["event"], "frame", "{frame}");
        assert!(viewer().await.is_err(), "the grant works once");
    }

    /// A Tick whose host died is lost: nothing reopens it on its own. Its user reopens it as a new
    /// instance with a new process; the lost instance's subscription stays gone rather than
    /// following it, and asking again never starts a second one.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_tick_lost_with_its_host_reopens_only_explicitly_as_a_new_instance() {
        use tickloop_agent_protocol::{EventKind, HostedMemberState, TickLinkState};
        let model = Arc::new(tickloop_agent_test_support::ScriptedModel::new(Vec::new()));
        let daemon =
            Daemon::start_with(model.clone(), super::support::hosted::IDLE_TIMEOUT, 1).await;
        let (runtime, chat, execution) = (
            daemon.runtimes[0].clone(),
            daemon.chats[0],
            daemon.executions[0],
        );
        let tools = daemon.tools();
        let opened = run_tool(&tools, chat, "ModifyTick", counter_command(&daemon)).await;
        let lost: tick_protocol::InstanceRef =
            serde_json::from_value(opened["tick_instance_id"].clone()).unwrap();
        model.enqueue(tickloop_agent_test_support::call(
            "subscribe",
            "SubscribeTick",
            json!({"tick_instance_id":lost}),
        ));
        model.enqueue(tickloop_agent_test_support::text("watching"));
        let mut events = runtime.subscribe();
        runtime.submit(chat, None, "Watch it".into()).await.unwrap();
        loop {
            if matches!(
                tickloop_agent_test_support::next_event(&mut events)
                    .await
                    .kind,
                EventKind::Completed { .. }
            ) {
                break;
            }
        }

        let business_pid = opened["business_pid"].as_u64().unwrap() as u32;
        let launcher = tickhost::process::sample(business_pid).unwrap().parent;
        let host = tickhost::process::sample(launcher).unwrap().parent;
        let host_paths = tickhost::paths::HostPaths {
            runtime: daemon.root.join("h"),
        };
        unsafe { libc::kill(host as libc::pid_t, libc::SIGKILL) };
        let members = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if !super::alive(business_pid) && !host_paths.host_running().unwrap() {
                    let CommandResult::HostedTicks(listed) = daemon
                        .command(AgentCommand::ListHostedTicks { execution })
                        .await
                    else {
                        panic!("hosted Ticks expected");
                    };
                    if !listed.is_empty() {
                        break listed;
                    }
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("the exited host is reflected in the recorded lost Tick listing");
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].instance, lost);
        assert_eq!(members[0].state, HostedMemberState::Lost);
        assert!(
            !host_paths.host_running().unwrap(),
            "nothing reopened by itself"
        );

        let CommandResult::HostedTickReopened {
            instance: reopened,
            membership,
        } = daemon
            .command(AgentCommand::ReopenHostedTick {
                execution,
                instance: lost.clone(),
            })
            .await
        else {
            panic!("a reopened Tick expected");
        };
        assert_ne!(reopened, lost, "a new instance");
        assert!(
            membership
                .members
                .iter()
                .any(|member| member.instance == reopened)
        );
        let CommandResult::HostedTicks(members) = daemon
            .command(AgentCommand::ListHostedTicks { execution })
            .await
        else {
            panic!("hosted Ticks expected");
        };
        let running = members
            .iter()
            .find(|member| member.instance == reopened)
            .unwrap();
        let new_pid = running.resources.as_ref().unwrap().business_pid;
        assert_ne!(new_pid, business_pid, "a new process");
        assert!(super::alive(new_pid));
        let records = launch_records(&daemon.root, chat);
        assert!(
            records
                .iter()
                .any(|record| record["state"] == "reopened" && record["instance"] == json!(lost))
        );

        let mut is_gone = false;
        for _ in 0..120 {
            let subscriptions = runtime.tick_subscriptions(chat).await.unwrap();
            if subscriptions[0].instance == lost
                && subscriptions[0].link == TickLinkState::InstanceGone
            {
                is_gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        assert!(is_gone, "the lost instance's subscription is not rebound");

        let CommandResult::HostedTickReopened {
            instance: again, ..
        } = daemon
            .command(AgentCommand::ReopenHostedTick {
                execution,
                instance: lost.clone(),
            })
            .await
        else {
            panic!("the earlier reopen expected");
        };
        assert_eq!(
            again, reopened,
            "asking again never starts a second instance"
        );
        let CommandResult::HostedTicks(members) = daemon
            .command(AgentCommand::ListHostedTicks { execution })
            .await
        else {
            panic!("hosted Ticks expected");
        };
        assert_eq!(
            members
                .iter()
                .filter(|member| member.state == HostedMemberState::Running)
                .count(),
            1
        );
    }

    /// A lost Tick whose working directory was replaced after it opened is not reopened: the
    /// user opens it again, under a fresh approval.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_lost_tick_whose_launch_changed_is_not_reopened() {
        let daemon = Daemon::start(1).await;
        let (chat, execution) = (daemon.chats[0], daemon.executions[0]);
        let tools = daemon.tools();
        let work = daemon.root.join("work");
        std::fs::create_dir(&work).unwrap();
        let mut command = counter_command(&daemon);
        command["cwd"] = json!(work);
        let opened = run_tool(&tools, chat, "ModifyTick", command).await;
        let business_pid = opened["business_pid"].as_u64().unwrap() as u32;
        let launcher = tickhost::process::sample(business_pid).unwrap().parent;
        let host = tickhost::process::sample(launcher).unwrap().parent;
        unsafe { libc::kill(host as libc::pid_t, libc::SIGKILL) };
        assert!(super::gone(business_pid).await);
        std::fs::remove_dir(&work).unwrap();
        std::fs::create_dir(&work).unwrap();
        let refused = daemon
            .server
            .execute(CommandEnvelope {
                daemon_epoch: daemon.epoch,
                command_id: Uuid::new_v4(),
                command: AgentCommand::ReopenHostedTick {
                    execution,
                    instance: serde_json::from_value(opened["tick_instance_id"].clone()).unwrap(),
                },
            })
            .await
            .unwrap()
            .into_result()
            .unwrap_err();
        assert!(refused.message.contains("LaunchChanged"), "{refused:?}");
    }

    /// The launch records the daemon keeps for one Chat under the config root.
    fn launch_records(root: &std::path::Path, chat: Uuid) -> Vec<Value> {
        let directory = root.join("state/tickhost/chats").join(chat.to_string());
        let Ok(entries) = std::fs::read_dir(directory) else {
            return Vec::new();
        };
        entries
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("launch-"))
            .map(|entry| serde_json::from_slice(&std::fs::read(entry.path()).unwrap()).unwrap())
            .collect()
    }
}

mod lifecycle {
    use std::{sync::Arc, time::Duration};

    use super::{
        alive, gone,
        support::hosted::{counter_request, tick_request},
    };

    use serde_json::json;
    use tickhost::{
        host::{OpenRequest, OpenedTick, Receipt, TickOwner},
        paths::HostPaths,
    };
    use tickhost_connect::hosted::{CAPABILITY_DATA, CONTROL_TIMEOUT, HostConnection, Role};
    use tickloop_agent_runtime::hosted::controller::{HostController, WatchIntents};
    use uuid::Uuid;

    const FLOOD: &str = r#"
import json, sys
for line in range(1500):
    print(f'line {line}', file=sys.stderr)
sys.stderr.flush()
for line in sys.stdin:
    request = json.loads(line)
    if request['method'] == 'tick/describe':
        result = {'protocol':3,'revision':'1','kind':'example.flood','title':'Flood','description':'Flood','recover_with':None,'tools':[]}
    else:
        result = {'seq':1,'at':'2026-10-07T00:00:00Z','data':{'lines':1500}}
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}), flush=True)
"#;

    struct Fixture {
        directory: tempfile::TempDir,
        paths: HostPaths,
        owner: TickOwner,
        idle_timeout: Duration,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            super::support::stop_tickhost(
                &self.paths.runtime,
                &self.directory.path().join("tickhost.key"),
            );
        }
    }

    impl Fixture {
        fn new() -> Self {
            Self::with_idle_timeout(super::support::hosted::IDLE_TIMEOUT)
        }

        fn with_idle_timeout(idle_timeout: Duration) -> Self {
            let directory = tempfile::Builder::new()
                .prefix("thl")
                .tempdir_in("/private/tmp")
                .unwrap();
            let paths = HostPaths {
                runtime: directory.path().join("h"),
            };
            Self {
                directory,
                paths,
                owner: TickOwner {
                    root: "fixture-root".into(),
                    chat: Uuid::new_v4(),
                },
                idle_timeout,
            }
        }

        /// One daemon generation: same runtime directory and key file, its own epoch.
        fn controller(&self) -> HostController {
            HostController::new(
                self.directory.path(),
                self.paths.clone(),
                Uuid::new_v4(),
                super::support::fixture_cli::tickhost(),
                self.idle_timeout,
            )
        }

        fn request(&self, program: Vec<String>) -> OpenRequest {
            tick_request(self.owner.clone(), self.directory.path(), program)
        }

        fn counter(&self) -> OpenRequest {
            counter_request(self.owner.clone(), self.directory.path())
        }

        async fn reader(&self, opened: &OpenedTick) -> HostConnection {
            HostConnection::connect(
                &opened.terminal.endpoint,
                Role::Instance {
                    instance: opened.instance.clone(),
                    data_token: opened.data_token.clone(),
                },
                &[CAPABILITY_DATA],
            )
            .await
            .unwrap()
        }

        async fn host_exits(&self) -> bool {
            for _ in 0..400 {
                if !self.paths.socket().exists() {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            false
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_daemon_replacement_keeps_the_host_and_tick_and_fences_the_old_daemon() {
        let fixture = Fixture::new();
        let first = fixture.controller();
        let opened = first.open(fixture.counter()).await.unwrap();
        let second = fixture.controller();
        let listed = second.list(&fixture.owner, None).await.unwrap();
        assert_eq!(listed["total"], 1);
        assert_eq!(listed["items"][0]["instance"], json!(opened.instance));
        assert_eq!(
            listed["items"][0]["business_pid"],
            json!(opened.business_pid),
            "the same process keeps running under the new daemon"
        );
        let fenced = first.list(&fixture.owner, None).await.unwrap_err();
        assert_eq!(fenced.code, "Fenced");
        let mut reader = fixture.reader(&opened).await;
        reader
            .request(
                "call",
                json!({"tool":"increment","args":{"amount":1},"revision":"1"}),
                CONTROL_TIMEOUT,
            )
            .await
            .unwrap();
        let receipt = second
            .close(Uuid::new_v4(), &fixture.owner, &opened.instance)
            .await
            .unwrap();
        assert!(matches!(receipt, Receipt::Closed { .. }));
        assert!(gone(opened.business_pid).await);
        assert!(fixture.host_exits().await);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_daemon_reconciles_its_registry_with_the_running_host() {
        use tickloop_agent_runtime::hosted::registry::{
            LaunchRecord, LaunchState, OwnerState, Registry,
        };
        let fixture = Fixture::new();
        let controller = fixture.controller();
        let config_root = fixture.directory.path().join("state");
        std::fs::create_dir(&config_root).unwrap();
        let registry = Registry::default();
        let root = registry.root(&config_root).await.unwrap();
        let open_for = |chat: Uuid| {
            let mut request = fixture.counter();
            request.owner.chat = chat;
            request
        };
        let kept_request = open_for(fixture.owner.chat);
        let kept = controller.open(kept_request.clone()).await.unwrap();
        let mut kept_record = LaunchRecord::opening(
            kept_request.operation,
            fixture.owner.chat,
            kept_request.command.clone(),
            kept_request.cwd.clone(),
            None,
        )
        .unwrap();
        kept_record.state = LaunchState::Live {
            instance: kept.instance.clone(),
            host_epoch: kept.terminal.epoch,
        };
        root.record_launch(&kept_record).await.unwrap();
        // A Tick whose Chat the registry no longer lists, and one whose StopSubagent end was
        // decided but not carried out when the previous daemon stopped.
        let orphan = controller.open(open_for(Uuid::new_v4())).await.unwrap();
        let stopping_chat = Uuid::new_v4();
        let stopping = controller.open(open_for(stopping_chat)).await.unwrap();
        let stop = Uuid::new_v4();
        root.set_owner_state(stopping_chat, &OwnerState::Stopping { operation: stop })
            .await
            .unwrap();

        root.reconcile_with_host(&controller, &fixture.owner.root)
            .await
            .unwrap();

        assert!(
            gone(orphan.business_pid).await,
            "a Tick of no listed Chat is reaped"
        );
        assert!(
            gone(stopping.business_pid).await,
            "a decided end is carried out"
        );
        assert_eq!(
            root.owner_state(stopping_chat).await.unwrap(),
            OwnerState::Stopped { operation: stop }
        );
        assert!(alive(kept.business_pid), "a listed Chat keeps its Tick");

        let launcher = tickhost::process::sample(kept.business_pid).unwrap().parent;
        let host = tickhost::process::sample(launcher).unwrap().parent;
        unsafe { libc::kill(host as libc::pid_t, libc::SIGKILL) };
        assert!(gone(kept.business_pid).await);
        root.reconcile_with_host(&controller, &fixture.owner.root)
            .await
            .unwrap();
        let records = root.launches(fixture.owner.chat).await.unwrap();
        assert!(
            matches!(&records[0].state, LaunchState::Lost { instance, .. } if *instance == kept.instance),
            "the record of a Tick whose host died is Lost: {records:?}"
        );
        assert!(
            !fixture.paths.host_running().unwrap(),
            "reconciling starts no host"
        );
    }

    /// A new daemon hands the host the Watch intents its registry committed, replacing what the
    /// host held, and drops the records of instances the same host no longer runs instead of
    /// keeping them for recovery.
    #[tokio::test(flavor = "multi_thread")]
    async fn reconcile_hands_the_host_the_committed_watches_and_drops_idled_out_records() {
        use std::collections::BTreeSet;
        use tickloop_agent_runtime::hosted::registry::{LaunchRecord, LaunchState, Registry};
        let fixture = Fixture::with_idle_timeout(Duration::from_secs(2));
        let controller = fixture.controller();
        let config_root = fixture.directory.path().join("state");
        std::fs::create_dir(&config_root).unwrap();
        let registry = Registry::default();
        let root = registry.root(&config_root).await.unwrap();
        // The host still holds a Watch for both; the registry committed that the first one's
        // Watches all ended while the daemon was away.
        let mut ticks = Vec::new();
        for committed in [BTreeSet::new(), BTreeSet::from([Uuid::new_v4()])] {
            let request = fixture.counter();
            let opened = controller.open(request.clone()).await.unwrap();
            let intents = WatchIntents::new(
                &fixture.owner,
                &opened.instance,
                &BTreeSet::from([Uuid::new_v4()]),
            )
            .unwrap();
            controller.set_intents(&intents).await.unwrap();
            let mut record = LaunchRecord::opening(
                request.operation,
                fixture.owner.chat,
                request.command,
                request.cwd,
                None,
            )
            .unwrap();
            record.state = LaunchState::Live {
                instance: opened.instance.clone(),
                host_epoch: opened.terminal.epoch,
            };
            record.watches = committed;
            root.record_launch(&record).await.unwrap();
            ticks.push(opened);
        }
        let [unwatched, watched] = <[_; 2]>::try_from(ticks).unwrap();

        root.reconcile_with_host(&controller, &fixture.owner.root)
            .await
            .unwrap();
        assert!(
            gone(unwatched.business_pid).await,
            "the host dropped the Watch the registry no longer has"
        );
        assert!(alive(watched.business_pid), "the committed Watch keeps it");
        root.reconcile_with_host(&controller, &fixture.owner.root)
            .await
            .unwrap();
        let records = root.launches(fixture.owner.chat).await.unwrap();
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0].instance(), Some(&watched.instance));
        assert!(matches!(records[0].state, LaunchState::Live { .. }));

        root.set_watches(
            &controller,
            &fixture.owner,
            &watched.instance,
            BTreeSet::new(),
        )
        .await
        .unwrap();
        assert!(gone(watched.business_pid).await);
        assert!(fixture.host_exits().await);
        root.reconcile_with_host(&controller, &fixture.owner.root)
            .await
            .unwrap();
        assert!(
            root.launches(fixture.owner.chat).await.unwrap().is_empty(),
            "a host that exited after all its Ticks ended leaves nothing to recover"
        );
    }

    /// A listing settles only records older than itself: an open that completed while the
    /// host was being listed is neither dropped nor taken for lost.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_record_written_after_a_listing_began_waits_for_the_next_one() {
        use tickloop_agent_runtime::hosted::{
            controller::OwnerListing,
            registry::{LaunchRecord, LaunchState, Registry},
        };
        let fixture = Fixture::new();
        let controller = fixture.controller();
        let config_root = fixture.directory.path().join("state");
        std::fs::create_dir(&config_root).unwrap();
        let root = Registry::default().root(&config_root).await.unwrap();
        let request = fixture.counter();
        let mut record = LaunchRecord::opening(
            request.operation,
            fixture.owner.chat,
            request.command,
            request.cwd,
            None,
        )
        .unwrap();
        record.state = LaunchState::Live {
            instance: tick_protocol::InstanceRef::hosted(Uuid::new_v4()),
            host_epoch: Uuid::new_v4(),
        };
        let earlier = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        tokio::time::sleep(Duration::from_millis(5)).await;
        root.record_launch(&record).await.unwrap();
        let stale = OwnerListing {
            started_ms: earlier,
            running: None,
        };
        root.settle_launches(&controller, &fixture.owner, &stale)
            .await
            .unwrap();
        assert!(matches!(
            root.launches(fixture.owner.chat).await.unwrap()[0].state,
            LaunchState::Live { .. }
        ));
        tokio::time::sleep(Duration::from_millis(5)).await;
        let later = controller.owner_listing(&fixture.owner).await.unwrap();
        root.settle_launches(&controller, &fixture.owner, &later)
            .await
            .unwrap();
        assert!(matches!(
            root.launches(fixture.owner.chat).await.unwrap()[0].state,
            LaunchState::Lost { .. }
        ));
    }

    /// A Watch change during a listing rewrites the record but not its state: the listing still
    /// settles it, so a Tick lost with its host is never hidden by a Watch renewal.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_watch_change_during_a_listing_does_not_hide_a_lost_tick() {
        use tickloop_agent_runtime::hosted::registry::{LaunchRecord, LaunchState, Registry};
        let fixture = Fixture::new();
        let controller = fixture.controller();
        let config_root = fixture.directory.path().join("state");
        std::fs::create_dir(&config_root).unwrap();
        let root = Registry::default().root(&config_root).await.unwrap();
        let request = fixture.counter();
        let instance = tick_protocol::InstanceRef::hosted(Uuid::new_v4());
        let mut record = LaunchRecord::opening(
            request.operation,
            fixture.owner.chat,
            request.command,
            request.cwd,
            None,
        )
        .unwrap();
        record.state = LaunchState::Live {
            instance: instance.clone(),
            host_epoch: Uuid::new_v4(),
        };
        root.record_launch(&record).await.unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        let listing = controller.owner_listing(&fixture.owner).await.unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        root.set_watches(
            &controller,
            &fixture.owner,
            &instance,
            [Uuid::new_v4()].into_iter().collect(),
        )
        .await
        .unwrap();
        root.settle_launches(&controller, &fixture.owner, &listing)
            .await
            .unwrap();
        assert!(matches!(
            root.launches(fixture.owner.chat).await.unwrap()[0].state,
            LaunchState::Lost { .. }
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn killing_the_host_ends_its_ticks_and_the_next_open_starts_a_new_epoch() {
        let fixture = Fixture::new();
        let controller = fixture.controller();
        let request = fixture.counter();
        let operation = request.operation;
        let opened = controller.open(request).await.unwrap();
        let launcher = tickhost::process::sample(opened.business_pid)
            .unwrap()
            .parent;
        let host = tickhost::process::sample(launcher).unwrap().parent;
        unsafe { libc::kill(host as libc::pid_t, libc::SIGKILL) };
        assert!(
            gone(launcher).await,
            "the launcher gets the terminal hangup"
        );
        assert!(
            gone(opened.business_pid).await,
            "the business Tick ends with it"
        );
        let listed = controller.list(&fixture.owner, None).await.unwrap();
        assert_eq!(listed["total"], 0, "a killed host leaves no hosted Tick");
        assert_eq!(
            controller.receipt(operation, &fixture.owner).await.unwrap(),
            None,
            "the receipt died with its host: the outcome is unknown"
        );
        let closed = controller
            .close(Uuid::new_v4(), &fixture.owner, &opened.instance)
            .await
            .unwrap();
        assert_eq!(
            closed,
            tickhost::host::Receipt::Closed {
                instance: opened.instance.clone()
            }
        );
        assert!(
            !fixture.paths.host_running().unwrap(),
            "list, receipt and close start no host"
        );
        let reopened = controller.open(fixture.counter()).await.unwrap();
        assert_ne!(reopened.terminal.epoch, opened.terminal.epoch);
        assert_ne!(reopened.instance, opened.instance);
        controller
            .close(Uuid::new_v4(), &fixture.owner, &reopened.instance)
            .await
            .unwrap();
        assert!(fixture.host_exits().await);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn twenty_one_ticks_run_side_by_side_without_a_count_limit() {
        let fixture = Fixture::new();
        let controller = Arc::new(fixture.controller());
        let mut opens = tokio::task::JoinSet::new();
        for _ in 0..21 {
            let controller = controller.clone();
            let request = fixture.counter();
            opens.spawn(async move { controller.open(request).await });
        }
        let mut opened = Vec::new();
        while let Some(result) = opens.join_next().await {
            opened.push(result.unwrap().unwrap());
        }
        let hosts = opened
            .iter()
            .map(|tick| tick.terminal.epoch)
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(hosts.len(), 1, "concurrent first opens start one host");
        let listed = controller.list(&fixture.owner, None).await.unwrap();
        assert_eq!(listed["total"], 21);
        let other = TickOwner {
            root: "fixture-root".into(),
            chat: Uuid::new_v4(),
        };
        assert_eq!(controller.list(&other, None).await.unwrap()["total"], 0);
        let foreign = controller
            .close(Uuid::new_v4(), &other, &opened[0].instance)
            .await
            .unwrap_err();
        assert_eq!(foreign.code, "Forbidden", "another Chat cannot close it");
        for tick in &opened {
            controller
                .close(Uuid::new_v4(), &fixture.owner, &tick.instance)
                .await
                .unwrap();
        }
        for tick in &opened {
            assert!(gone(tick.business_pid).await);
        }
        assert!(fixture.host_exits().await);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stuck_open_never_delays_the_daemons_other_requests() {
        let fixture = Fixture::new();
        let controller = Arc::new(fixture.controller());
        // A launcher that never reports keeps its open pending until the control timeout, and
        // ignores SIGTERM, so the host must kill it after the terminate grace.
        let pid_file = fixture.directory.path().join("stubborn.pid");
        let mut never_ready = fixture.counter();
        never_ready.command = vec![
            "/bin/sh".into(),
            "-c".into(),
            r#"trap "" TERM; echo $$ > "$1"; while :; do sleep 1; done"#.into(),
            "stubborn".into(),
            pid_file.to_string_lossy().into_owned(),
        ];
        let stuck_since = std::time::Instant::now();
        let stuck = {
            let controller = controller.clone();
            tokio::spawn(async move { controller.open(never_ready).await })
        };
        for _ in 0..200 {
            if controller.list(&fixture.owner, None).await.unwrap()["total"] == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let started = std::time::Instant::now();
        let opened = controller.open(fixture.counter()).await.unwrap();
        let listed = controller.list(&fixture.owner, None).await.unwrap();
        assert!(
            started.elapsed() < CONTROL_TIMEOUT / 2,
            "other requests waited {:?} behind the stuck open",
            started.elapsed()
        );
        assert_eq!(listed["total"], 2);
        let refused = stuck.await.unwrap().unwrap_err();
        assert_eq!(refused.code, "LaunchFailed");
        assert!(
            stuck_since.elapsed() >= CONTROL_TIMEOUT + tickhost::host::TERMINATE_GRACE,
            "the launcher ignored SIGTERM and was killed after the grace"
        );
        let stubborn: u32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(!alive(stubborn), "SIGKILL ended the launcher");
        controller
            .close(Uuid::new_v4(), &fixture.owner, &opened.instance)
            .await
            .unwrap();
        assert!(fixture.host_exits().await);
    }

    /// Enough descriptors for an idle host and its controller, too few for a Tick terminal.
    const STARVED_DESCRIPTOR_LIMIT: libc::rlim_t = 16;

    #[tokio::test(flavor = "multi_thread")]
    async fn a_host_out_of_descriptors_refuses_opens_before_dispatch_and_keeps_serving() {
        use std::{io::Write, os::unix::process::CommandExt};
        let fixture = Fixture::new();
        let key = tickhost::host::random_token();
        let key_file = fixture.directory.path().join("tickhost.key");
        std::fs::write(&key_file, &key).unwrap();
        let mut command = std::process::Command::new(super::support::fixture_cli::tickhost());
        command
            .arg(tickhost::cli::RUN_COMMAND)
            .env(tickhost::paths::RUNTIME_DIR_ENV, &fixture.paths.runtime)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        unsafe {
            command.pre_exec(|| {
                let limit = libc::rlimit {
                    rlim_cur: STARVED_DESCRIPTOR_LIMIT,
                    rlim_max: STARVED_DESCRIPTOR_LIMIT,
                };
                if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut host = command.spawn().unwrap();
        let mut stdin = host.stdin.take().unwrap();
        let config = tickhost::host::HostConfig {
            controller_key: key.clone(),
            idle_timeout: super::support::hosted::IDLE_TIMEOUT,
        };
        writeln!(stdin, "{}", serde_json::to_string(&config).unwrap()).unwrap();
        drop(stdin);
        // The starved host must serve before the daemon asks, or the daemon would start its own.
        for _ in 0..1000 {
            if fixture.paths.socket().exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(fixture.paths.socket().exists(), "the starved host serves");
        let controller = fixture.controller();
        let refused = controller.open(fixture.counter()).await.unwrap_err();
        assert!(!refused.dispatched, "{refused:?}");
        assert!(
            ["TerminalUnavailable", "LaunchFailed"].contains(&refused.code.as_str()),
            "{refused:?}"
        );
        let listed = controller.list(&fixture.owner, None).await.unwrap();
        assert_eq!(listed["total"], 0, "the host keeps serving");
        drop(controller);
        drop(fixture);
        let exited = tokio::task::spawn_blocking(move || host.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(exited.success());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_flooding_terminal_is_drained_and_its_screen_stays_current() {
        let fixture = Fixture::new();
        let script = fixture.directory.path().join("flood.py");
        std::fs::write(&script, FLOOD).unwrap();
        let controller = fixture.controller();
        let opened = controller
            .open(fixture.request(vec![
                "/usr/bin/python3".into(),
                "-u".into(),
                script.to_string_lossy().into_owned(),
            ]))
            .await
            .unwrap();
        let mut reader = fixture.reader(&opened).await;
        let mut last = String::new();
        for _ in 0..200 {
            let snapshot = reader
                .request("snapshot", json!({}), CONTROL_TIMEOUT)
                .await
                .unwrap();
            last = snapshot["lines"]
                .as_array()
                .unwrap()
                .last()
                .and_then(|line| line.as_str())
                .unwrap_or_default()
                .to_owned();
            if last == "line 1499" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(last, "line 1499", "no tail line is lost");
        let read = reader
            .request("read", json!({"activity":"explicit"}), CONTROL_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(read["data"]["lines"], 1500);
        assert!(alive(opened.business_pid));
        controller
            .close(Uuid::new_v4(), &fixture.owner, &opened.instance)
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unread_update_stream_never_blocks_other_readers_or_calls() {
        let fixture = Fixture::new();
        let controller = fixture.controller();
        let opened = controller.open(fixture.counter()).await.unwrap();
        let mut stalled = fixture.reader(&opened).await;
        stalled
            .request("watch", json!({}), CONTROL_TIMEOUT)
            .await
            .unwrap();
        let mut reader = fixture.reader(&opened).await;
        for _ in 0..200 {
            reader
                .request(
                    "call",
                    json!({"tool":"increment","args":{"amount":1},"revision":"1"}),
                    CONTROL_TIMEOUT,
                )
                .await
                .unwrap();
        }
        let read = reader
            .request("read", json!({"activity":"explicit"}), CONTROL_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(read["data"]["value"], 200);
        controller
            .close(Uuid::new_v4(), &fixture.owner, &opened.instance)
            .await
            .unwrap();
        assert!(gone(opened.business_pid).await);
    }
}
