//! Hosted Ticks through the real CLI, owner and worker processes: the worker asks its owner over
//! the authenticated channel, the owner starts the Tick host on demand, and the Tick runs hidden
//! in a private terminal with no Herdr anywhere.

mod support;

use std::{
    path::Path,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

const FIXTURE_TIMEOUT: Duration = Duration::from_secs(30);

/// Stops the Tick host a test started, if one still runs, through its management channel, so
/// a failed assertion never leaves a host and its Ticks behind.
fn stop_test_host(runtime: &Path, key_file: &Path) {
    use std::io::{BufRead, BufReader, Write};
    let (Ok(key), Ok(stream)) = (
        std::fs::read_to_string(key_file),
        std::os::unix::net::UnixStream::connect(runtime.join("host.sock")),
    ) else {
        return;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let mut writer = &stream;
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    let hello = json!({"channel":"management","major":1,"minor":0,"controller_key":key.trim()});
    if writeln!(writer, "{hello}").is_err() || reader.read_line(&mut line).is_err() {
        return;
    }
    let Ok(hello) = serde_json::from_str::<Value>(&line) else {
        return;
    };
    let shutdown = json!({"id":1,"method":"shutdown_host","params":{"epoch":hello["result"]["epoch"],"operation":uuid::Uuid::new_v4()}});
    if writeln!(writer, "{shutdown}").is_ok() {
        line.clear();
        let _ = reader.read_line(&mut line);
    }
}

/// An open of `program` for `owner`, started through the CLI's Tick launcher.
fn tick_request(
    owner: tick_host::host::TickOwner,
    cwd: &Path,
    program: Vec<String>,
) -> tick_host::host::OpenRequest {
    let mut command = vec![
        support::fixture_cli::executable()
            .to_string_lossy()
            .into_owned(),
        "--tick-launch".into(),
        "--".into(),
    ];
    command.extend(program);
    tick_host::host::OpenRequest {
        operation: uuid::Uuid::new_v4(),
        owner,
        owner_revision: 0,
        command,
        cwd: cwd.to_path_buf(),
        environment: vec![("PATH".into(), "/usr/bin:/bin".into())],
        title: None,
    }
}

/// An open of the example counter Tick for `owner`.
fn counter_request(owner: tick_host::host::TickOwner, cwd: &Path) -> tick_host::host::OpenRequest {
    let counter = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/ticks/counter.py")
        .canonicalize()
        .unwrap();
    tick_request(
        owner,
        cwd,
        vec![
            "/usr/bin/python3".into(),
            "-u".into(),
            counter.to_string_lossy().into_owned(),
        ],
    )
}

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

/// Holds `chat`'s retention lock the way a running cleanup does.
fn hold_retention(config_root: &std::path::Path, chat: uuid::Uuid) -> std::fs::File {
    use std::os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, OpenOptionsExt},
    };
    let directory = config_root.join("chat-retention");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(&directory)
        .unwrap();
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(directory.join(format!("{chat}.lock")))
        .unwrap();
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
    lock
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
async fn a_worker_opens_reads_calls_and_closes_a_hidden_tick_through_its_owner() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let directory = tempfile::Builder::new()
        .prefix("tht")
        .tempdir_in("/private/tmp")
        .unwrap();
    let root = directory.path().canonicalize().unwrap();
    let counter = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/ticks/counter.py");
    let launcher = support::fixture_cli::executable();
    let observed = Arc::new(Observed::default());
    let server_observed = observed.clone();
    let host_runtime = root.join("h");
    let server_host_runtime = host_runtime.clone();
    let open_command = json!([
        launcher.to_string_lossy(),
        "--tick-launch",
        "--",
        "/usr/bin/python3",
        "-u",
        counter.canonicalize().unwrap().to_string_lossy(),
    ]);
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
    let mut child = tokio::process::Command::new(&launcher)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &home)
        .env("TICKLOOP_AGENT_RUNTIME_DIR", &runtime_directory)
        .env("TICKLOOP_TICK_HOST_RUNTIME_DIR", &host_runtime)
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
    support::cli_runtime::shutdown_owner(&runtime_directory)
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
        "the owner starts no host before the first open"
    );
    let opened: Value = serde_json::from_str(&results[0]).unwrap();
    assert_eq!(opened["status"], "opened", "{results:#?}");
    assert_eq!(opened["host"], "tick-host");
    assert!(opened["business_pid"].as_u64().is_some());
    let read: Value = serde_json::from_str(&results[1]).unwrap();
    assert_eq!(read["content"]["value"], 0, "{results:#?}");
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
    use std::{sync::Arc, time::Duration};

    use async_trait::async_trait;
    use serde_json::{Value, json};
    use tickloop_agent::{
        agent_access::{
            AccessError, AgentAccess, AgentCommand, CommandEnvelope, CommandResult,
            EmbeddedAgentAccess, LaunchSpec, OwnerServer, WorkerFactory,
        },
        host_tools::{HostCallResult, HostEndpointIdentity, HostToolAuthority, HostToolProvider},
        hosted::{
            controller::HostController,
            lifecycle::OwnerLifecycle,
            tools::HostedTickTools,
            worker_control::{
                HostedAdmission, OwnerControl, WorkerControl, WorkerControlGrant, WorkerRequest,
                root_identity,
            },
        },
        model::ModelClient,
        runtime::Agent,
        tick_adapter::TickAdapters,
    };
    use tokio::sync::Mutex;
    use tokio_util::sync::CancellationToken;
    use uuid::Uuid;

    /// Workers in this process, each holding the grant the owner would hand a real worker and,
    /// as a real worker with a config root does, telling the owner when StopAgent stops a child.
    struct GrantedWorkers {
        control: Arc<WorkerControl>,
        model: Arc<dyn ModelClient>,
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
            let runtime = Agent::new_embedded(super::support::config(), self.model.clone());
            let grant = self.control.grant();
            runtime.install_stopped_child_hook(Arc::new(OwnerLifecycle::new(grant.clone())));
            let tools = Arc::new(HostedTickTools::new(
                OwnerControl::new(grant.clone()),
                vec![("PATH".into(), "/usr/bin:/bin".into())],
            ));
            runtime.install_host_tools(tools.clone()).unwrap();
            runtime.install_hosted_ticks(tools.clone());
            runtime
                .install_tick_stream(Arc::new(TickAdapters::builtin(
                    Some(runtime.clone()),
                    None,
                    None,
                    Some(tools),
                )))
                .await;
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

    /// A real owner with its worker-control endpoint and in-process workers, one per Chat.
    struct Owner {
        _directory: tempfile::TempDir,
        root: std::path::PathBuf,
        config_root: std::path::PathBuf,
        server: OwnerServer,
        epoch: Uuid,
        chats: Vec<Uuid>,
        executions: Vec<tickloop_agent::agent_access::ExecutionRef>,
        grants: Vec<WorkerControlGrant>,
        runtimes: Vec<Agent>,
        controller: Arc<HostController>,
    }

    impl Owner {
        async fn start(chats: usize) -> Self {
            Self::start_with(
                Arc::new(super::support::ScriptedModel::new(Vec::new())),
                tick_host::host::DEFAULT_IDLE_TIMEOUT,
                chats,
            )
            .await
        }

        async fn start_with(
            model: Arc<dyn ModelClient>,
            idle_timeout: Duration,
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
                tick_host::paths::HostPaths {
                    runtime: root.join("h"),
                },
                epoch,
                super::support::fixture_cli::executable(),
                idle_timeout,
            ));
            let control = Arc::new(WorkerControl::new(&root, epoch, controller.clone()));
            let listener = tokio::net::UnixListener::bind(control.endpoint()).unwrap();
            let factory = Arc::new(GrantedWorkers {
                control: control.clone(),
                model,
                grants: Mutex::new(Vec::new()),
                runtimes: Mutex::new(Vec::new()),
            });
            let owner = OwnerServer::new(epoch, factory.clone());
            control.attach_owner(owner.clone());
            tokio::spawn(control.clone().serve(listener));
            let mut created_chats = Vec::new();
            let mut executions = Vec::new();
            for _ in 0..chats {
                let mut launch = LaunchSpec::embedded(root.clone());
                launch.config_root = Some(config_root.clone());
                let created = owner
                    .execute(CommandEnvelope {
                        owner_epoch: epoch,
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
                server: owner,
                epoch,
                chats: created_chats,
                executions,
                grants,
                runtimes,
                controller,
            }
        }

        /// Runs a command the way a View sends it through the owner to the Chat's worker.
        async fn command(&self, command: AgentCommand) -> CommandResult {
            self.server
                .execute(CommandEnvelope {
                    owner_epoch: self.epoch,
                    command_id: Uuid::new_v4(),
                    command,
                })
                .await
                .unwrap()
                .into_result()
                .unwrap()
        }

        /// The host owner of `chat` under this owner's config root.
        fn tick_owner(&self, chat: Uuid) -> tick_host::host::TickOwner {
            tick_host::host::TickOwner {
                root: root_identity(&self.config_root).unwrap(),
                chat,
            }
        }

        /// Opens a counter Tick for `chat` directly through the owner's host controller, after
        /// the owner's first-use reconcile of the root so the Tick is not taken for an orphan.
        async fn open_counter(&self, chat: Uuid) -> tick_host::host::OpenedTick {
            self.controller
                .open(super::counter_request(self.tick_owner(chat), &self.root))
                .await
                .unwrap()
        }

        /// The owner state file of `chat`, if the registry keeps one.
        fn owner_state(&self, chat: Uuid) -> Option<Value> {
            let path = self
                .config_root
                .join("tick-host/chats")
                .join(chat.to_string())
                .join("owner.json");
            std::fs::read(path)
                .ok()
                .map(|bytes| serde_json::from_slice(&bytes).unwrap())
        }
    }

    impl Drop for Owner {
        fn drop(&mut self) {
            super::stop_test_host(&self.root.join("h"), &self.root.join("tick-host.key"));
        }
    }

    #[tokio::test]
    async fn only_a_granted_worker_acts_and_only_for_its_own_chats() {
        // Refused requests never reach a host, and a list starts none: only an open does.
        let owner = Owner::start(2).await;
        let (root, chats) = (owner.root.clone(), owner.chats.clone());
        let (first, second) = (owner.grants[0].clone(), owner.grants[1].clone());

        let foreign = OwnerControl::new(second.clone())
            .request(list(chats[0]))
            .await;
        assert_eq!(
            foreign.unwrap_err().code,
            "Forbidden",
            "another worker's Chat"
        );
        let invented = OwnerControl::new(first.clone())
            .request(list(Uuid::new_v4()))
            .await;
        assert_eq!(invented.unwrap_err().code, "Forbidden", "an invented Chat");
        let mut forged = first.clone();
        forged.capability = "0".repeat(64);
        assert_eq!(
            OwnerControl::new(forged)
                .request(list(chats[0]))
                .await
                .unwrap_err()
                .code,
            "Forbidden"
        );
        let mut earlier = first.clone();
        earlier.owner_epoch = Uuid::new_v4();
        assert_eq!(
            OwnerControl::new(earlier)
                .request(list(chats[0]))
                .await
                .unwrap_err()
                .code,
            "EpochChanged"
        );

        let listed = OwnerControl::new(first.clone())
            .request(list(chats[0]))
            .await
            .unwrap();
        assert_eq!(listed["total"], 0);
        assert_eq!(
            listed["endpoint"],
            serde_json::json!(root.join("h/host.sock"))
        );
        let listed = OwnerControl::new(second.clone())
            .request(list(chats[1]))
            .await
            .unwrap();
        assert_eq!(listed["total"], 0, "each worker sees its own Chat");

        let elsewhere = OwnerControl::new(second.clone())
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
                        endpoint: HostEndpointIdentity::TickHost {
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
            let envelope = json!({"id":1,"envelope":{"owner_epoch":first.owner_epoch,"binding":first.binding,"capability":first.capability,"request":{"method":"reconcile","chat":chats[0]}}});
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
            "no request but an admitted open starts a Tick host"
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
            OwnerControl::new(WorkerControlGrant {
                endpoint: directory.path().join("w.sock"),
                owner_epoch: Uuid::new_v4(),
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
            owner_epoch: Uuid::new_v4(),
            binding: Uuid::new_v4(),
            capability: "0".repeat(64),
            host_endpoint: directory.path().join("host.sock"),
            intent_renewal: std::time::Duration::from_secs(100),
        };
        let chat = Uuid::new_v4();
        let instance = tick_connect::instance::InstanceRef::hosted(Uuid::new_v4());
        let opened = tick_host::host::OpenedTick {
            instance: instance.clone(),
            terminal: tick_connect::hosted::HostedTerminal {
                endpoint: directory.path().join("host.sock"),
                epoch: Uuid::new_v4(),
                terminal: Uuid::new_v4(),
            },
            business_pid: 4242,
            data_token: "1".repeat(64),
        };
        let owner = tokio::spawn(async move {
            let mut requests = Vec::new();
            let mut answers = [
                None,
                Some(json!(tick_host::host::Receipt::Opened(opened))),
                None,
                Some(json!(tick_host::host::Receipt::Dispatched)),
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
        let tools = HostedTickTools::new(OwnerControl::new(grant), Vec::new());
        let open = tools
            .prepare_call(
                chat,
                "ModifyTick",
                &json!({"action":"open","command":["/bin/sh"],"cwd":"/private/tmp","placement":{"type":"hidden"}}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let settled = tools
            .execute_prepared(chat, open, CancellationToken::new())
            .await;
        let HostCallResult::CompletedWithEffect(result, _) = settled else {
            panic!("the opened receipt settles the open");
        };
        assert!(
            result.content.contains(&instance.to_string()),
            "{}",
            result.content
        );
        let close = tools
            .prepare_call(
                chat,
                "ModifyTick",
                &json!({"action":"close","tick_instance_id":instance}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let unknown = tools
            .execute_prepared(chat, close, CancellationToken::new())
            .await;
        let HostCallResult::OutcomeUnknown(message) = unknown else {
            panic!("a receipt still dispatched leaves the close unknown");
        };
        assert!(message.contains("do not repeat"), "{message}");
        let requests = owner.await.unwrap();
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

    /// Its catalog grows when `evolve` is called.
    const EVOLVING: &str = r#"
import json, os, sys
instance = os.environ['TICKLOOP_INSTANCE_ID']
value = 0
tools = [{'name':'increment','description':'Increment','params':{'type':'object'},'mutates':True},{'name':'evolve','description':'Change the catalog','params':{'type':'object'},'mutates':False}]
for line in sys.stdin:
    request = json.loads(line)
    method = request['method']
    if method == 'tick/describe':
        result = {'protocol':2,'tick_type':'example.evolving','title':'Evolving','description':'Evolving','recover_with':None,'tools':tools}
    elif method == 'tick/read':
        result = {'version':3,'tick_instance_id':instance,'initialized':True,'sequence':value,'observed_at':'2026-10-07T00:00:00Z','data':{'value':value}}
    else:
        tool = request['params']['tool']
        if tool == 'increment':
            value += 1
        elif tool == 'evolve':
            tools = tools + [{'name':'decrement','description':'Decrement','params':{'type':'object'},'mutates':True}]
        result = {'value':value}
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}), flush=True)
"#;

    fn completed(result: HostCallResult) -> Value {
        match result {
            HostCallResult::Completed(result) | HostCallResult::CompletedWithEffect(result, _) => {
                serde_json::from_str(&result.content).unwrap()
            }
            HostCallResult::NotDispatched(error) | HostCallResult::OutcomeUnknown(error) => {
                panic!("hosted call failed: {error}")
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_call_approved_before_its_catalog_changed_is_never_dispatched() {
        let owner = Owner::start(1).await;
        let chat = owner.chats[0];
        let tools = HostedTickTools::new(
            OwnerControl::new(owner.grants[0].clone()),
            vec![("PATH".into(), "/usr/bin:/bin".into())],
        );
        let script = owner.root.join("evolving.py");
        std::fs::write(&script, EVOLVING).unwrap();
        let launcher = super::support::fixture_cli::executable();
        let run = |name: &'static str, input: Value| {
            let tools = &tools;
            async move {
                let prepared = tools
                    .prepare_call(chat, name, &input, CancellationToken::new())
                    .await
                    .unwrap();
                completed(
                    tools
                        .execute_prepared(chat, prepared, CancellationToken::new())
                        .await,
                )
            }
        };
        let opened = run(
            "ModifyTick",
            json!({"action":"open","command":[launcher, "--tick-launch", "--", "/usr/bin/python3", "-u", script],"cwd":owner.root,"placement":{"type":"hidden"}}),
        )
        .await;
        let instance = opened["tick_instance_id"].clone();
        let records = launch_records(&owner.root, chat);
        assert_eq!(records.len(), 1, "the owner keeps a durable launch record");
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
        run(
            "CallTickTool",
            json!({"tick_instance_id":instance,"tool":"evolve","args":{}}),
        )
        .await;
        let changed = tools
            .revalidate_call(chat, &approved, CancellationToken::new())
            .await
            .unwrap_err();
        assert!(changed.contains("catalog changed"), "{changed}");
        assert!(matches!(
            tools
                .execute_prepared(chat, approved, CancellationToken::new())
                .await,
            HostCallResult::NotDispatched(_)
        ));
        let read = run("ReadTick", json!({"tick_instance_id":instance})).await;
        assert_eq!(read["content"]["value"], 0, "the stale approval never ran");
        let other_chat = Uuid::new_v4();
        let foreign = tools
            .prepare_call(
                other_chat,
                "ReadTick",
                &json!({"tick_instance_id":instance}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let HostCallResult::NotDispatched(refused) = tools
            .execute_prepared(other_chat, foreign, CancellationToken::new())
            .await
        else {
            panic!("another Chat of this worker never reuses the owner's binding");
        };
        assert!(refused.contains("Forbidden"), "{refused}");
        assert!(read["tools"].to_string().contains("decrement"));
        let closed = run(
            "ModifyTick",
            json!({"action":"close","tick_instance_id":instance}),
        )
        .await;
        assert_eq!(closed["status"], "closed");
        assert!(
            launch_records(&owner.root, chat).is_empty(),
            "a closed Tick leaves no launch record"
        );
    }

    fn write_transcript(directory: &std::path::Path, chat: Uuid, age: Duration) {
        std::fs::create_dir_all(directory).unwrap();
        let path = directory.join(format!("{chat}.jsonl"));
        std::fs::write(&path, "{}\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - age)
            .unwrap();
    }

    /// The real retention cleanup in a worker removes an expired Transcript between its owner
    /// notices, so exactly that Chat's Ticks end, all of them. A Chat that still runs, a failed
    /// unlink and a cleanup of tool results only end nothing.
    #[tokio::test(flavor = "multi_thread")]
    async fn removing_an_expired_transcript_ends_only_that_chats_ticks_through_the_owner() {
        use tickloop_agent::cleanup::{CleanupOutcome, cleanup_old_message_files_in_background};
        const EXPIRED: Duration = Duration::from_secs(60 * 24 * 60 * 60);
        let owner = Owner::start(1).await;
        let running = owner.chats[0];
        OwnerControl::new(owner.grants[0].clone())
            .request(list(running))
            .await
            .unwrap();
        let (deleted, unlink_fails, tools_only) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let mut deleted_ticks = Vec::new();
        for _ in 0..21 {
            deleted_ticks.push(owner.open_counter(deleted).await);
        }
        let kept = [
            owner.open_counter(unlink_fails).await,
            owner.open_counter(tools_only).await,
            owner.open_counter(running).await,
        ];
        let project = owner.config_root.join("projects/-work");
        write_transcript(&project, deleted, EXPIRED);
        write_transcript(&project, running, EXPIRED);
        write_transcript(&project, tools_only, Duration::ZERO);
        let tool_result = project.join(format!("{tools_only}/tool-results/call.txt"));
        std::fs::create_dir_all(tool_result.parent().unwrap()).unwrap();
        std::fs::write(&tool_result, "result").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&tool_result)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - EXPIRED)
            .unwrap();
        let locked_project = owner.config_root.join("projects/-locked");
        write_transcript(&locked_project, unlink_fails, EXPIRED);
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&locked_project, std::fs::Permissions::from_mode(0o500)).unwrap();

        let notice = OwnerLifecycle::new(owner.grants[0].clone());
        let config_root = owner.config_root.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            cleanup_old_message_files_in_background(&config_root, &notice)
        })
        .await
        .unwrap();
        std::fs::set_permissions(&locked_project, std::fs::Permissions::from_mode(0o700)).unwrap();

        let CleanupOutcome::Completed(result) = outcome else {
            panic!("cleanup was skipped");
        };
        assert_eq!(
            result.errors, 2,
            "the failed unlink and the refused running Chat are reported"
        );
        assert!(!tool_result.exists());
        for tick in &deleted_ticks {
            assert!(
                super::gone(tick.business_pid).await,
                "every Tick of the deleted Chat ends"
            );
        }
        for tick in &kept {
            assert!(super::alive(tick.business_pid), "no other Chat's Tick ends");
        }
        assert_eq!(
            owner.owner_state(deleted),
            None,
            "a deleted Chat leaves no state"
        );
        assert_eq!(owner.owner_state(unlink_fails).unwrap()["state"], "active");
        assert_eq!(
            owner.owner_state(running),
            None,
            "a running Chat is never announced"
        );
    }

    /// A real root stops its direct child with StopAgent; the worker's hook tells the owner, which
    /// checks the family itself and ends only the child's Ticks.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_root_stopping_its_direct_child_ends_only_the_childs_ticks() {
        use super::support::routed::{
            RoutedModel, has_tool_result, history_text, state, tool_result,
        };
        use tickloop_agent::runtime::{Event, EventKind, Receipt};
        const STOP_PROMPT: &str = "Stop the waiter now";
        async fn finished(events: &mut tokio::sync::broadcast::Receiver<Event>, receipt: &Receipt) {
            loop {
                let event = super::support::next_event(events).await;
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
                    super::support::call(
                        "ready",
                        "SendMessage",
                        json!({"to":"parent","message":"Child is ready to wait"}),
                    )
                } else {
                    super::support::call(
                        "wait",
                        "ReceiveAgentMessage",
                        json!({"timeout_ms":600000}),
                    )
                }
            } else if !has_tool_result(request, "delegate") {
                super::support::call(
                    "delegate",
                    "DelegateAgent",
                    json!({"task":"Wait to be stopped","name":"waiter"}),
                )
            } else if history_text(request).contains(STOP_PROMPT)
                && !has_tool_result(request, "stop")
            {
                super::support::call("stop", "StopAgent", json!({"to":"waiter"}))
            } else {
                super::support::text("Done")
            }
        }));
        let owner = Owner::start_with(model, tick_host::host::DEFAULT_IDLE_TIMEOUT, 1).await;
        let (runtime, root) = (owner.runtimes[0].clone(), owner.chats[0]);
        OwnerControl::new(owner.grants[0].clone())
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
        let child_tick = owner.open_counter(child).await;
        let root_tick = owner.open_counter(root).await;
        for (claimed_root, claimed_child) in [(root, child), (root, root)] {
            let refused = OwnerControl::new(owner.grants[0].clone())
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
        assert_eq!(owner.owner_state(child).unwrap()["state"], "stopped");
        assert_eq!(owner.owner_state(root), None);
        let tools = HostedTickTools::new(
            OwnerControl::new(owner.grants[0].clone()),
            vec![("PATH".into(), "/usr/bin:/bin".into())],
        );
        let prepared = tools
            .prepare_call(
                child,
                "ModifyTick",
                &counter_command(&owner),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let HostCallResult::NotDispatched(refused) = tools
            .execute_prepared(child, prepared, CancellationToken::new())
            .await
        else {
            panic!("a stopped child opens nothing before it continues");
        };
        assert!(refused.contains("OwnerEnded"), "{refused}");
    }

    /// A Chat's Watch intents go through its owner, which records them with the launch and
    /// hands them to the host: any one Watch keeps an otherwise idle Tick, the last one ending
    /// lets it idle out, and the next complete listing drops its record. Only the Chat that owns
    /// the instance can protect it.
    #[tokio::test(flavor = "multi_thread")]
    async fn watch_intents_through_the_owner_keep_a_tick_until_the_last_watch_ends() {
        let owner = Owner::start_with(
            Arc::new(super::support::ScriptedModel::new(Vec::new())),
            Duration::from_secs(2),
            2,
        )
        .await;
        let (chat, other_chat) = (owner.chats[0], owner.chats[1]);
        let tools = HostedTickTools::new(
            OwnerControl::new(owner.grants[0].clone()),
            vec![("PATH".into(), "/usr/bin:/bin".into())],
        );
        let launcher = super::support::fixture_cli::executable();
        let counter = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/ticks/counter.py")
            .canonicalize()
            .unwrap();
        let prepared = tools
            .prepare_call(
                chat,
                "ModifyTick",
                &json!({"action":"open","command":[launcher, "--tick-launch", "--", "/usr/bin/python3", "-u", counter],"cwd":owner.root,"placement":{"type":"hidden"}}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let opened = completed(
            tools
                .execute_prepared(chat, prepared, CancellationToken::new())
                .await,
        );
        let instance: tick_connect::instance::InstanceRef =
            serde_json::from_value(opened["tick_instance_id"].clone()).unwrap();
        let business_pid = opened["business_pid"].as_u64().unwrap() as u32;
        let watches = |chat, watches: Vec<Uuid>| WorkerRequest::SetWatchIntents {
            chat,
            instance: instance.clone(),
            watches: watches.into_iter().collect(),
        };
        let (first_watch, second_watch) = (Uuid::new_v4(), Uuid::new_v4());
        OwnerControl::new(owner.grants[0].clone())
            .request(watches(chat, vec![first_watch, second_watch]))
            .await
            .unwrap();
        let recorded = launch_records(&owner.root, chat);
        assert_eq!(recorded[0]["watches"].as_array().unwrap().len(), 2);
        let foreign = OwnerControl::new(owner.grants[1].clone())
            .request(watches(chat, Vec::new()))
            .await
            .unwrap_err();
        assert_eq!(foreign.code, "Forbidden", "another worker's Chat");
        let not_its_tick = OwnerControl::new(owner.grants[1].clone())
            .request(watches(other_chat, Vec::new()))
            .await
            .unwrap_err();
        assert_eq!(not_its_tick.code, "TargetGone", "another Chat's Tick");

        // A worker renews its intents well within the idle timeout, as its grant says.
        let renewal = owner.grants[0].intent_renewal;
        assert!(renewal < Duration::from_secs(1));
        let renew = |current: Vec<Uuid>, rounds: u32| {
            let request = watches(chat, current);
            let grant = owner.grants[0].clone();
            async move {
                for _ in 0..rounds {
                    OwnerControl::new(grant.clone())
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
        OwnerControl::new(owner.grants[0].clone())
            .request(watches(chat, Vec::new()))
            .await
            .unwrap();
        assert!(
            super::gone(business_pid).await,
            "it idles out after the last Watch"
        );
        assert_eq!(launch_records(&owner.root, chat).len(), 1);
        OwnerControl::new(owner.grants[0].clone())
            .request(list(chat))
            .await
            .unwrap();
        assert!(
            launch_records(&owner.root, chat).is_empty(),
            "an idled-out Tick cannot be reopened and leaves no record"
        );
    }

    /// Runs one hosted Tick tool for `chat` the way an approved model call would.
    async fn run_tool(tools: &HostedTickTools, chat: Uuid, name: &str, input: Value) -> Value {
        let prepared = tools
            .prepare_call(chat, name, &input, CancellationToken::new())
            .await
            .unwrap();
        completed(
            tools
                .execute_prepared(chat, prepared, CancellationToken::new())
                .await,
        )
    }

    fn counter_command(owner: &Owner) -> Value {
        let counter = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/ticks/counter.py")
            .canonicalize()
            .unwrap();
        json!({"action":"open","command":[super::support::fixture_cli::executable(), "--tick-launch", "--", "/usr/bin/python3", "-u", counter],"cwd":owner.root,"placement":{"type":"hidden"}})
    }

    /// A Loop subscribed to a hosted Tick: the quiet Tick costs no model call, its Watch keeps it
    /// from idling, an update wakes the Loop with the new content, and stopping the Loop ends
    /// the Watch so the Tick idles out.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_hosted_tick_update_reaches_the_loop_subscribed_to_it() {
        use tickloop_agent::runtime::EventKind;
        let model = Arc::new(super::support::ScriptedModel::new(Vec::new()));
        let owner = Owner::start_with(model.clone(), Duration::from_secs(2), 1).await;
        let (runtime, chat) = (owner.runtimes[0].clone(), owner.chats[0]);
        let tools = HostedTickTools::new(
            OwnerControl::new(owner.grants[0].clone()),
            vec![("PATH".into(), "/usr/bin:/bin".into())],
        );
        let opened = run_tool(&tools, chat, "ModifyTick", counter_command(&owner)).await;
        let instance = opened["tick_instance_id"].clone();
        let business_pid = opened["business_pid"].as_u64().unwrap() as u32;
        model.enqueue(super::support::call(
            "subscribe",
            "SubscribeTick",
            json!({"tick_instance_id":instance,"every_seconds":5}),
        ));
        model.enqueue(super::support::text("watching"));
        let mut events = runtime.subscribe();
        runtime
            .submit(chat, None, "Watch the counter".into())
            .await
            .unwrap();
        loop {
            match super::support::next_event(&mut events).await.kind {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        let subscriptions = runtime.tick_subscriptions(chat).await.unwrap();
        assert_eq!(subscriptions.len(), 1, "{subscriptions:?}");
        let watched = async {
            for _ in 0..200 {
                let records = launch_records(&owner.root, chat);
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
        assert!(watched.await, "the Watch reached the owner as an intent");

        tokio::time::sleep(Duration::from_millis(5500)).await;
        assert!(super::alive(business_pid), "the Watch keeps the quiet Tick");
        assert_eq!(
            model.requests.lock().unwrap().len(),
            2,
            "a quiet Tick wakes nothing"
        );
        model.enqueue(super::support::text("update observed"));
        run_tool(
            &tools,
            chat,
            "CallTickTool",
            json!({"tick_instance_id":instance,"tool":"increment","args":{"amount":7}}),
        )
        .await;
        loop {
            let event = tokio::time::timeout(Duration::from_secs(15), events.recv())
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

    /// Two Loops watch one hosted Tick: cancelling one keeps the other's Watch and deliveries,
    /// and once the Tick's host dies the remaining Watch fails closed as gone, calling no model
    /// and never rebinding to a later host.
    #[tokio::test(flavor = "multi_thread")]
    async fn cancelling_one_hosted_watch_keeps_the_other_and_a_dead_host_fails_closed() {
        use tickloop_agent::runtime::{EventKind, TickLinkState, TickSubscriptionChange};
        let model = Arc::new(super::support::ScriptedModel::new(Vec::new()));
        let owner =
            Owner::start_with(model.clone(), tick_host::host::DEFAULT_IDLE_TIMEOUT, 1).await;
        let (runtime, chat) = (owner.runtimes[0].clone(), owner.chats[0]);
        let tools = HostedTickTools::new(
            OwnerControl::new(owner.grants[0].clone()),
            vec![("PATH".into(), "/usr/bin:/bin".into())],
        );
        let opened = run_tool(&tools, chat, "ModifyTick", counter_command(&owner)).await;
        let instance = opened["tick_instance_id"].clone();
        let mut events = runtime.subscribe();
        let run = |name: &'static str| {
            model.enqueue(super::support::call(
                name,
                "SubscribeTick",
                json!({"tick_instance_id":instance,"every_seconds":5}),
            ));
            model.enqueue(super::support::text("watching"));
        };
        for (name, prompt) in [("a", "Watch it as A"), ("b", "Watch it as B")] {
            run(name);
            let receipt = runtime.submit(chat, None, prompt.into()).await.unwrap();
            loop {
                let event = super::support::next_event(&mut events).await;
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
            let root = owner.root.clone();
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
        model.enqueue(super::support::text("update observed"));
        run_tool(
            &tools,
            chat,
            "CallTickTool",
            json!({"tick_instance_id":instance,"tool":"increment","args":{"amount":1}}),
        )
        .await;
        loop {
            let event = tokio::time::timeout(Duration::from_secs(15), events.recv())
                .await
                .expect("the remaining Watch is delivered")
                .unwrap();
            if matches!(event.kind, EventKind::Completed { .. }) {
                break;
            }
        }
        assert_eq!(model.requests.lock().unwrap().len(), before + 1);

        let business_pid = opened["business_pid"].as_u64().unwrap() as u32;
        let launcher = tick_host::process::sample(business_pid).unwrap().parent;
        let host = tick_host::process::sample(launcher).unwrap().parent;
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
        run_tool(&tools, chat, "ModifyTick", counter_command(&owner)).await;
        tokio::time::sleep(Duration::from_secs(6)).await;
        assert_eq!(
            model.requests.lock().unwrap().len(),
            before + 1,
            "nothing reached the model after the host died"
        );
    }

    /// A View's Monitor lists its Chat's hosted Ticks with a process sample of each and closes
    /// the one its user picks; the other keeps running.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_monitor_lists_hosted_ticks_with_samples_and_closes_the_one_picked() {
        let owner = Owner::start(1).await;
        let (chat, execution) = (owner.chats[0], owner.executions[0]);
        let tools = HostedTickTools::new(
            OwnerControl::new(owner.grants[0].clone()),
            vec![("PATH".into(), "/usr/bin:/bin".into())],
        );
        let closed = run_tool(&tools, chat, "ModifyTick", counter_command(&owner)).await;
        let kept = run_tool(&tools, chat, "ModifyTick", counter_command(&owner)).await;
        let CommandResult::HostedTicks(members) = owner
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
        }
        let instance = serde_json::from_value(closed["tick_instance_id"].clone()).unwrap();
        assert!(matches!(
            owner
                .command(AgentCommand::CloseHostedTick {
                    execution,
                    instance,
                })
                .await,
            CommandResult::Done
        ));
        assert!(super::gone(closed["business_pid"].as_u64().unwrap() as u32).await);
        assert!(super::alive(kept["business_pid"].as_u64().unwrap() as u32));
        let records = launch_records(&owner.root, chat);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["instance"], kept["tick_instance_id"]);
    }

    /// A Tick whose host died is lost: nothing reopens it on its own. Its user reopens it as a new
    /// instance with a new process; the lost instance's subscription stays gone rather than
    /// following it, and asking again never starts a second one.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_tick_lost_with_its_host_reopens_only_explicitly_as_a_new_instance() {
        use tickloop_agent::{
            hosted::tools::HostedMemberState,
            runtime::{EventKind, TickLinkState},
        };
        let model = Arc::new(super::support::ScriptedModel::new(Vec::new()));
        let owner =
            Owner::start_with(model.clone(), tick_host::host::DEFAULT_IDLE_TIMEOUT, 1).await;
        let (runtime, chat, execution) = (
            owner.runtimes[0].clone(),
            owner.chats[0],
            owner.executions[0],
        );
        let tools = HostedTickTools::new(
            OwnerControl::new(owner.grants[0].clone()),
            vec![("PATH".into(), "/usr/bin:/bin".into())],
        );
        let opened = run_tool(&tools, chat, "ModifyTick", counter_command(&owner)).await;
        let lost: tick_connect::instance::InstanceRef =
            serde_json::from_value(opened["tick_instance_id"].clone()).unwrap();
        model.enqueue(super::support::call(
            "subscribe",
            "SubscribeTick",
            json!({"tick_instance_id":lost,"every_seconds":5}),
        ));
        model.enqueue(super::support::text("watching"));
        let mut events = runtime.subscribe();
        runtime.submit(chat, None, "Watch it".into()).await.unwrap();
        loop {
            if matches!(
                super::support::next_event(&mut events).await.kind,
                EventKind::Completed { .. }
            ) {
                break;
            }
        }

        let business_pid = opened["business_pid"].as_u64().unwrap() as u32;
        let launcher = tick_host::process::sample(business_pid).unwrap().parent;
        let host = tick_host::process::sample(launcher).unwrap().parent;
        unsafe { libc::kill(host as libc::pid_t, libc::SIGKILL) };
        assert!(super::gone(business_pid).await);
        let CommandResult::HostedTicks(members) = owner
            .command(AgentCommand::ListHostedTicks { execution })
            .await
        else {
            panic!("hosted Ticks expected");
        };
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].instance, lost);
        assert_eq!(members[0].state, HostedMemberState::Lost);
        let host_paths = tick_host::paths::HostPaths {
            runtime: owner.root.join("h"),
        };
        assert!(
            !host_paths.host_running().unwrap(),
            "nothing reopened by itself"
        );

        let CommandResult::HostedTickReopened {
            instance: reopened,
            membership,
        } = owner
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
        let CommandResult::HostedTicks(members) = owner
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
        let records = launch_records(&owner.root, chat);
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
        } = owner
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
        let CommandResult::HostedTicks(members) = owner
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

    /// A cleanup whose finish answer never arrives still ends the deleted Chat's Ticks: the
    /// owner settles the deletion itself once the cleanup lets go of the Chat.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_deletion_whose_finish_is_lost_is_settled_by_the_owner_itself() {
        let owner = Owner::start(1).await;
        OwnerControl::new(owner.grants[0].clone())
            .request(list(owner.chats[0]))
            .await
            .unwrap();
        let deleted = Uuid::new_v4();
        let tick = owner.open_counter(deleted).await;
        let transcript = owner
            .config_root
            .join(format!("projects/-work/{deleted}.jsonl"));
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(&transcript, "{}\n").unwrap();
        let cleanup = super::hold_retention(&owner.config_root, deleted);
        OwnerControl::new(owner.grants[0].clone())
            .request(WorkerRequest::BeginChatDeletion {
                chat: deleted,
                operation: Uuid::new_v4(),
            })
            .await
            .unwrap();
        std::fs::remove_file(&transcript).unwrap();
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(
            super::alive(tick.business_pid),
            "nothing ends while the cleanup holds the Chat"
        );
        drop(cleanup);
        assert!(super::gone(tick.business_pid).await);
        assert_eq!(owner.owner_state(deleted), None);
    }

    /// A lost Tick whose working directory was replaced after it opened is not reopened: the
    /// user opens it again, under a fresh approval.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_lost_tick_whose_launch_changed_is_not_reopened() {
        let owner = Owner::start(1).await;
        let (chat, execution) = (owner.chats[0], owner.executions[0]);
        let tools = HostedTickTools::new(
            OwnerControl::new(owner.grants[0].clone()),
            vec![("PATH".into(), "/usr/bin:/bin".into())],
        );
        let work = owner.root.join("work");
        std::fs::create_dir(&work).unwrap();
        let mut command = counter_command(&owner);
        command["cwd"] = json!(work);
        let opened = run_tool(&tools, chat, "ModifyTick", command).await;
        let business_pid = opened["business_pid"].as_u64().unwrap() as u32;
        let launcher = tick_host::process::sample(business_pid).unwrap().parent;
        let host = tick_host::process::sample(launcher).unwrap().parent;
        unsafe { libc::kill(host as libc::pid_t, libc::SIGKILL) };
        assert!(super::gone(business_pid).await);
        std::fs::remove_dir(&work).unwrap();
        std::fs::create_dir(&work).unwrap();
        let refused = owner
            .server
            .execute(CommandEnvelope {
                owner_epoch: owner.epoch,
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

    /// The launch records the owner keeps for one Chat under the config root.
    fn launch_records(root: &std::path::Path, chat: Uuid) -> Vec<Value> {
        let directory = root.join("state/tick-host/chats").join(chat.to_string());
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

    use super::{alive, gone, hold_retention};

    use serde_json::json;
    use tick_connect::hosted::{CAPABILITY_DATA, CONTROL_TIMEOUT, HostConnection, Role};
    use tick_host::{
        host::{OpenRequest, OpenedTick, Receipt, TickOwner},
        paths::HostPaths,
    };
    use tickloop_agent::hosted::controller::HostController;
    use uuid::Uuid;

    const FLOOD: &str = r#"
import json, os, sys
instance = os.environ['TICKLOOP_INSTANCE_ID']
for line in range(1500):
    print(f'line {line}', file=sys.stderr)
sys.stderr.flush()
for line in sys.stdin:
    request = json.loads(line)
    if request['method'] == 'tick/describe':
        result = {'protocol':2,'tick_type':'example.flood','title':'Flood','description':'Flood','recover_with':None,'tools':[]}
    else:
        result = {'version':3,'tick_instance_id':instance,'initialized':True,'sequence':1,'observed_at':'2026-10-07T00:00:00Z','data':{'lines':1500}}
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
            super::stop_test_host(
                &self.paths.runtime,
                &self.directory.path().join("tick-host.key"),
            );
        }
    }

    impl Fixture {
        fn new() -> Self {
            Self::with_idle_timeout(tick_host::host::DEFAULT_IDLE_TIMEOUT)
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

        /// One owner generation: same runtime directory and key file, its own epoch.
        fn controller(&self) -> HostController {
            HostController::new(
                self.directory.path(),
                self.paths.clone(),
                Uuid::new_v4(),
                super::support::fixture_cli::executable(),
                self.idle_timeout,
            )
        }

        fn request(&self, program: Vec<String>) -> OpenRequest {
            super::tick_request(self.owner.clone(), self.directory.path(), program)
        }

        fn counter(&self) -> OpenRequest {
            super::counter_request(self.owner.clone(), self.directory.path())
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
    async fn an_owner_replacement_keeps_the_host_and_tick_and_fences_the_old_owner() {
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
            "the same process keeps running under the new owner"
        );
        let fenced = first.list(&fixture.owner, None).await.unwrap_err();
        assert_eq!(fenced.code, "Fenced");
        let mut reader = fixture.reader(&opened).await;
        reader
            .request(
                "call",
                json!({"tool":"increment","args":{"amount":1}}),
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
    async fn an_owner_reconciles_its_registry_with_the_running_host() {
        use tickloop_agent::hosted::registry::{LaunchRecord, LaunchState, OwnerState, Registry};
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
        // A Tick whose Chat the registry no longer lists; one whose StopAgent end was decided
        // but not carried out when the previous owner stopped; and deletions whose finish was
        // lost: Transcript gone, Transcript still there, cleanup still holding the lock.
        let orphan = controller.open(open_for(Uuid::new_v4())).await.unwrap();
        let (stopping_chat, deleted_chat, present_chat, held_chat) = (
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        let stopping = controller.open(open_for(stopping_chat)).await.unwrap();
        let deleted = controller.open(open_for(deleted_chat)).await.unwrap();
        let present = controller.open(open_for(present_chat)).await.unwrap();
        let held = controller.open(open_for(held_chat)).await.unwrap();
        let stop = Uuid::new_v4();
        root.set_owner_state(stopping_chat, &OwnerState::Stopping { operation: stop })
            .await
            .unwrap();
        for chat in [deleted_chat, present_chat, held_chat] {
            root.begin_deletion(chat, Uuid::new_v4()).await.unwrap();
        }
        let project = config_root.join("projects/-work");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join(format!("{present_chat}.jsonl")), "{}\n").unwrap();
        let cleanup = hold_retention(&config_root, held_chat);

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
        assert!(
            gone(deleted.business_pid).await,
            "a Chat whose Transcript is gone ends"
        );
        assert!(
            !root.chats().await.unwrap().contains(&deleted_chat),
            "a deleted Chat leaves no state"
        );
        assert!(alive(kept.business_pid), "a listed Chat keeps its Tick");
        assert!(
            alive(present.business_pid),
            "a Transcript that is still there keeps its Chat"
        );
        assert_eq!(
            root.owner_state(present_chat).await.unwrap(),
            OwnerState::Active
        );
        assert!(
            alive(held.business_pid),
            "a cleanup that still holds the lock is not second-guessed"
        );
        assert!(matches!(
            root.owner_state(held_chat).await.unwrap(),
            OwnerState::Deleting { .. }
        ));
        drop(cleanup);

        let launcher = tick_host::process::sample(kept.business_pid)
            .unwrap()
            .parent;
        let host = tick_host::process::sample(launcher).unwrap().parent;
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
            !root.chats().await.unwrap().contains(&held_chat),
            "once the cleanup released the lock, its removed Transcript ends the Chat"
        );
        assert!(
            !fixture.paths.host_running().unwrap(),
            "reconciling starts no host"
        );
    }

    /// A new owner hands the host the Watch intents its registry committed, replacing what the
    /// host held, and drops the records of instances the same host no longer runs instead of
    /// keeping them for recovery.
    #[tokio::test(flavor = "multi_thread")]
    async fn reconcile_hands_the_host_the_committed_watches_and_drops_idled_out_records() {
        use std::collections::BTreeSet;
        use tickloop_agent::hosted::registry::{LaunchRecord, LaunchState, Registry};
        let fixture = Fixture::with_idle_timeout(Duration::from_secs(2));
        let controller = fixture.controller();
        let config_root = fixture.directory.path().join("state");
        std::fs::create_dir(&config_root).unwrap();
        let registry = Registry::default();
        let root = registry.root(&config_root).await.unwrap();
        // The host still holds a Watch for both; the registry committed that the first one's
        // Watches all ended while the owner was away.
        let mut ticks = Vec::new();
        for committed in [BTreeSet::new(), BTreeSet::from([Uuid::new_v4()])] {
            let request = fixture.counter();
            let opened = controller.open(request.clone()).await.unwrap();
            controller
                .set_intents(
                    &fixture.owner,
                    &opened.instance,
                    &BTreeSet::from([Uuid::new_v4()]),
                )
                .await
                .unwrap();
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
        use tickloop_agent::hosted::{
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
            instance: tick_connect::instance::InstanceRef::hosted(Uuid::new_v4()),
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

    #[tokio::test(flavor = "multi_thread")]
    async fn killing_the_host_ends_its_ticks_and_the_next_open_starts_a_new_epoch() {
        let fixture = Fixture::new();
        let controller = fixture.controller();
        let request = fixture.counter();
        let operation = request.operation;
        let opened = controller.open(request).await.unwrap();
        let launcher = tick_host::process::sample(opened.business_pid)
            .unwrap()
            .parent;
        let host = tick_host::process::sample(launcher).unwrap().parent;
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
            tick_host::host::Receipt::Closed {
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
    async fn a_stuck_open_never_delays_the_owners_other_requests() {
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
            stuck_since.elapsed() >= CONTROL_TIMEOUT + tick_host::host::TERMINATE_GRACE,
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
        let key = tick_host::host::random_token();
        let key_file = fixture.directory.path().join("tick-host.key");
        std::fs::write(&key_file, &key).unwrap();
        let mut command = std::process::Command::new(super::support::fixture_cli::executable());
        command
            .arg(tickloop_agent::hosted::controller::HOST_MODE)
            .env(tick_host::paths::RUNTIME_DIR_ENV, &fixture.paths.runtime)
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
        let config = tick_host::host::HostConfig {
            controller_key: key.clone(),
            idle_timeout: tick_host::host::DEFAULT_IDLE_TIMEOUT,
        };
        writeln!(stdin, "{}", serde_json::to_string(&config).unwrap()).unwrap();
        drop(stdin);
        // The starved host must serve before the owner asks, or the owner would start its own.
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
                    json!({"tool":"increment","args":{"amount":1}}),
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
