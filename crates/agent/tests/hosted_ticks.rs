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
            tools::HostedTickTools,
            worker_control::{
                HostedAdmission, OwnerControl, WorkerControl, WorkerControlGrant, WorkerRequest,
            },
        },
        runtime::Agent,
    };
    use tokio::sync::Mutex;
    use tokio_util::sync::CancellationToken;
    use uuid::Uuid;

    /// Workers in this process, each holding the grant the owner would hand a real worker.
    struct GrantedWorkers {
        control: Arc<WorkerControl>,
        grants: Mutex<Vec<WorkerControlGrant>>,
    }

    #[async_trait]
    impl WorkerFactory for GrantedWorkers {
        async fn start(
            &self,
            launch: &LaunchSpec,
            epoch: Uuid,
        ) -> Result<Arc<dyn AgentAccess>, AccessError> {
            let runtime = Agent::new_embedded(
                super::support::config(),
                Arc::new(super::support::ScriptedModel::new(Vec::new())),
            );
            let access: Arc<dyn AgentAccess> =
                Arc::new(EmbeddedAgentAccess::for_worker(runtime, epoch));
            let grant = self.control.grant();
            self.control
                .register(&grant, access.clone(), launch.config_root.as_deref())
                .await;
            self.grants.lock().await.push(grant);
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
        chats: Vec<Uuid>,
        grants: Vec<WorkerControlGrant>,
    }

    impl Owner {
        async fn start(chats: usize) -> Self {
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
            ));
            let control = Arc::new(WorkerControl::new(&root, epoch, controller));
            let listener = tokio::net::UnixListener::bind(control.endpoint()).unwrap();
            let factory = Arc::new(GrantedWorkers {
                control: control.clone(),
                grants: Mutex::new(Vec::new()),
            });
            let owner = OwnerServer::new(epoch, factory.clone());
            control.attach_owner(owner.clone());
            tokio::spawn(control.clone().serve(listener));
            let mut created_chats = Vec::new();
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
                let Some(CommandResult::Created { snapshot, .. }) = created.result else {
                    panic!("create failed: {created:?}");
                };
                created_chats.push(snapshot.conversation.id);
            }
            let grants = factory.grants.lock().await.clone();
            Self {
                _directory: directory,
                root,
                chats: created_chats,
                grants,
            }
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
    }
}

mod lifecycle {
    use std::{path::PathBuf, sync::Arc, time::Duration};

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
            }
        }

        /// One owner generation: same runtime directory and key file, its own epoch.
        fn controller(&self) -> HostController {
            HostController::new(
                self.directory.path(),
                self.paths.clone(),
                Uuid::new_v4(),
                super::support::fixture_cli::executable(),
            )
        }

        fn request(&self, program: Vec<String>) -> OpenRequest {
            let mut command = vec![
                super::support::fixture_cli::executable()
                    .to_string_lossy()
                    .into_owned(),
                "--tick-launch".into(),
                "--".into(),
            ];
            command.extend(program);
            OpenRequest {
                operation: Uuid::new_v4(),
                owner: self.owner.clone(),
                command,
                cwd: self.directory.path().to_path_buf(),
                environment: vec![("PATH".into(), "/usr/bin:/bin".into())],
                title: None,
            }
        }

        fn counter(&self) -> OpenRequest {
            let counter = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../examples/ticks/counter.py")
                .canonicalize()
                .unwrap();
            self.request(vec![
                "/usr/bin/python3".into(),
                "-u".into(),
                counter.to_string_lossy().into_owned(),
            ])
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
        writeln!(stdin, "{key}").unwrap();
        drop(stdin);
        for _ in 0..200 {
            if fixture.paths.socket().exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
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
            .request("read", json!({}), CONTROL_TIMEOUT)
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
            .request("read", json!({}), CONTROL_TIMEOUT)
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
