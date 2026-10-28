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

/// The `hosted:` instance an open result names, as the model read it.
fn hosted_instance(results: &[String]) -> Option<String> {
    results.iter().find_map(|result| {
        let instance = serde_json::from_str::<Value>(result).ok()?["tick_instance_id"]
            .as_str()?
            .to_owned();
        instance.starts_with("hosted:").then_some(instance)
    })
}

#[derive(Default)]
struct Observed {
    calls: AtomicUsize,
    host_before_open: std::sync::atomic::AtomicBool,
    results: Mutex<Vec<String>>,
    tick_manage_schema: Mutex<Value>,
}

/// The model first sends TickManage with no arguments, as GLM did for a schema whose root was
/// only a `oneOf`, and is told which fields to fill. It then opens the Tick package program
/// `tick-counter` by name and without `protocol`: the worker finds the package installed in its
/// View's config root, and the program runs as the protocol Tick its manifest declares.
#[tokio::test(flavor = "multi_thread")]
async fn a_worker_opens_reads_calls_and_closes_a_hidden_tick_through_its_daemon() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let directory = tempfile::Builder::new()
        .prefix("tht")
        .tempdir_in(tickloop_harness::test_support::SHORT_TEMP_ROOT)
        .unwrap();
    let root = directory.path().canonicalize().unwrap();
    let observed = Arc::new(Observed::default());
    let server_observed = observed.clone();
    let config_root = root.join("state");
    let host_runtime = tickhost::paths::HostPaths::of_config_root(Some(&config_root)).runtime;
    let server_host_runtime = host_runtime.clone();
    support::hosted::install_counter_package(&config_root, &root);
    let server = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let observed = server_observed.clone();
            let host_socket = server_host_runtime.join("host.sock");
            tokio::spawn(async move {
                let (path, request) = read_request(&mut stream).await;
                assert!(path.starts_with("POST /v1/messages "), "{path}");
                let results = tool_results(&request);
                *observed.results.lock().unwrap() = results.clone();
                if let Some(modify) = request["tools"]
                    .as_array()
                    .and_then(|tools| tools.iter().find(|tool| tool["name"] == "TickManage"))
                {
                    *observed.tick_manage_schema.lock().unwrap() = modify["input_schema"].clone();
                }
                observed.calls.fetch_add(1, Ordering::SeqCst);
                let instance = hosted_instance(&results);
                let block = match (results.len(), instance) {
                    (0, _) => {
                        json!({"type":"tool_use","id":"empty","name":"TickManage","input":{}})
                    }
                    (1, _) => {
                        observed
                            .host_before_open
                            .store(host_socket.exists(), Ordering::SeqCst);
                        json!({"type":"tool_use","id":"open","name":"TickManage","input":{"action":"open","command":["tick-counter"]}})
                    }
                    (2, Some(instance)) => {
                        json!({"type":"tool_use","id":"read","name":"TickRead","input":{"tick_instance_id":instance}})
                    }
                    (3, Some(instance)) => {
                        json!({"type":"tool_use","id":"call","name":"TickCall","input":{"tick_instance_id":instance,"tool":"increment","args":{"amount":2}}})
                    }
                    (4, Some(instance)) => {
                        json!({"type":"tool_use","id":"close","name":"TickManage","input":{"action":"close","tick_instance_id":instance}})
                    }
                    _ => json!({"type":"text","text":"Hosted counter finished."}),
                };
                let body = model_stream(block);
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            });
        }
    });
    let config_path = root.join("model.json");
    std::fs::write(&config_path, serde_json::to_vec(&json!({"name":"fixture","protocol":"messages","base_url":origin,"model":"fixture","api_key_env":"","max_output_tokens":256})).unwrap()).unwrap();
    let runtime_directory = support::cli_runtime::adopt(&config_root);
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
        while observed.calls.load(Ordering::SeqCst) < 6 {
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
    assert_eq!(results.len(), 5, "{results:#?}");
    let tick_manage_schema = observed.tick_manage_schema.lock().unwrap().clone();
    assert_eq!(tick_manage_schema["type"], "object", "{tick_manage_schema}");
    assert_eq!(tick_manage_schema["required"], json!(["action"]));
    assert!(
        tick_manage_schema.get("oneOf").is_none(),
        "{tick_manage_schema}"
    );
    assert_eq!(
        results[0],
        "invalid TickManage input: action is required: one of open (requires command), send_input (requires tick_instance_id), close (requires tick_instance_id), reopen (requires tick_instance_id)"
    );
    assert!(
        !observed.host_before_open.load(Ordering::SeqCst),
        "the daemon starts no host before the first open"
    );
    let opened: Value = serde_json::from_str(&results[1]).unwrap();
    assert_eq!(opened["status"], "opened", "{results:#?}");
    assert_eq!(opened["host"], "tickhost");
    assert!(opened["business_pid"].as_u64().is_some());
    let read: Value = serde_json::from_str(&results[2]).unwrap();
    assert_eq!(read["data"]["value"], 0, "{results:#?}");
    assert!(read.get("descriptor").is_none() && read.get("tools").is_none());
    let called: Value = serde_json::from_str(&results[3]).unwrap();
    assert_eq!(called["result"]["value"], 2, "{results:#?}");
    let closed: Value = serde_json::from_str(&results[4]).unwrap();
    assert_eq!(closed["status"], "closed", "{results:#?}");
    assert!(
        host_gone.is_ok(),
        "the host exits once its last Tick is closed"
    );
}

mod channel {
    mod confirmed_open;

    use std::{collections::BTreeMap, path::Path, sync::Arc, time::Duration};

    use async_trait::async_trait;
    use serde_json::{Value, json};
    use tickhost_connect::hosted::{CAPABILITY_DATA, CONTROL_TIMEOUT, HostConnection, Role};
    use tickloop_code_access::{
        AccessError, AgentAccess, AgentCommand, CommandEnvelope, CommandResult, EnvironmentValue,
        HostWorkContext, LaunchSpec,
    };
    use tickloop_code_daemon::{
        DaemonServer, EmbeddedAgentAccess, WorkerFactory, build_runtime_with_models,
        worker_control::WorkerControl,
    };
    use tickloop_harness::model::ModelClient;
    use tickloop_harness::protocol::ToolResult;
    use tickloop_harness::protocol::{
        EventKind, HostEndpointIdentity, HostToolAuthority, TickLinkState, TickSubscriptionChange,
    };

    use tickloop_code_local_environment::environment::LocalEnvironment;
    use tickloop_code_local_environment::hosted::{
        controller::HostController,
        tools::HostedTickTools,
        worker_control::{
            DaemonControl, HostedAdmission, WorkerControlGrant, WorkerRequest, root_identity,
        },
    };
    use tickloop_harness::runtime::{
        Agent, HarnessOptions, HarnessSeams,
        host_tools::{HostCallResult, HostToolProvider},
        tick_adapter::TickAdapters,
        tick_stream::{
            CONNECTS_PER_WATCH, HostedTickStream, RECONNECT_PAUSE, RECONNECTS_PER_WATCH,
            TickConnectionState, TickStreamEvent, TickStreamPort,
        },
    };
    use tickloop_harness::test_support::harness::ChosenModels;
    use tokio::sync::{Mutex, mpsc};
    use tokio_util::sync::CancellationToken;
    use uuid::Uuid;

    use super::support::hosted::{
        counter_program, counter_request, python_program, tick_environment,
    };

    const WITHHELD_CALL_REPLY: &str = r#"
import json, sys
value = 0
for line in sys.stdin:
    request = json.loads(line)
    method = request['method']
    if method == 'tick/describe':
        result = {'protocol':3,'revision':'1','kind':'example.counter','title':'Counter','description':'Counter','tools':[{'name':'increment','description':'Increment','input_schema':{'type':'object'},'mutates':True}]}
    elif method == 'tick/read':
        result = {'seq':value,'data':{'value':value}}
    else:
        value += 1
        print(json.dumps({'jsonrpc':'2.0','method':'tick/update','params':{'seq':value,'data':{'value':value}}}), flush=True)
        continue
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}), flush=True)
"#;

    #[tokio::test]
    async fn execution_port_dispatched_cancellation_is_shared_by_both_adapters() {
        use tickloop_harness::runtime::environment::{ExecutionScope, OpenSpec};
        use tickloop_harness::test_support::execution_contracts::dispatched_cancellation_contract;
        use tickloop_harness::test_support::execution_environment::MemoryEnvironment;
        let memory = MemoryEnvironment::new();
        memory.install_tick_program("/counter", serde_json::from_value(json!({
            "protocol":3,"revision":"1","kind":"example.counter","title":"Counter","description":"Counter",
            "tools":[{"name":"increment","description":"Increment","input_schema":{"type":"object"},"mutates":true}],
        })).unwrap(), json!({"value":0}), |data, _, _| { data["value"] = json!(data["value"].as_u64().unwrap() + 1); Ok(data.clone()) });
        let _held = memory.hold_call_replies();
        dispatched_cancellation_contract(
            &memory,
            ExecutionScope {
                chat: tickloop_ids::generate().into(),
                folder: Some("/".into()),
            },
            OpenSpec {
                command: vec!["/counter".into()],
                cwd: Some("/".into()),
                protocol: true,
            },
        )
        .await;
        let daemon = Daemon::start(1).await;
        let script = daemon.root.join("withheld-reply.py");
        std::fs::write(&script, WITHHELD_CALL_REPLY).unwrap();
        let environment = LocalEnvironment::with(
            Arc::new(DaemonControl::new(daemon.grants[0].clone())),
            Arc::new(daemon.tools()),
        );
        dispatched_cancellation_contract(
            &environment,
            ExecutionScope {
                chat: daemon.chats[0].into(),
                folder: Some(daemon.root.to_string_lossy().into_owned()),
            },
            OpenSpec {
                command: python_program(&script, &[]),
                cwd: Some(daemon.root.to_string_lossy().into_owned()),
                protocol: true,
            },
        )
        .await;
    }

    fn port_scope(
        chat: Uuid,
        folder: &Path,
    ) -> tickloop_harness::runtime::environment::ExecutionScope {
        tickloop_harness::runtime::environment::ExecutionScope {
            chat: chat.into(),
            folder: Some(folder.to_string_lossy().into_owned()),
        }
    }

    fn port_open(
        command: Vec<String>,
        folder: &Path,
    ) -> tickloop_harness::runtime::environment::OpenSpec {
        tickloop_harness::runtime::environment::OpenSpec {
            command,
            cwd: Some(folder.to_string_lossy().into_owned()),
            protocol: true,
        }
    }

    #[tokio::test]
    async fn execution_port_scope_and_revocation_contracts_match_both_adapters() {
        use tickloop_harness::test_support::{
            execution_contracts::{authority_contract, ownership_contract},
            execution_environment::counter_environment,
        };
        let memory = counter_environment();
        let first = port_scope(tickloop_ids::generate(), Path::new("/"));
        let second = port_scope(tickloop_ids::generate(), Path::new("/"));
        authority_contract(
            &memory,
            first.clone(),
            port_open(vec!["/counter".into()], Path::new("/")),
        )
        .await;
        ownership_contract(
            &memory,
            first,
            &memory,
            second,
            port_open(vec!["/counter".into()], Path::new("/")),
        )
        .await;
        let daemon = Daemon::start(2).await;
        let first = LocalEnvironment::with_ticks(Arc::new(daemon.tools()));
        let second = LocalEnvironment::with_ticks(Arc::new(HostedTickTools::new(
            DaemonControl::new(daemon.grants[1].clone()),
            tick_environment(),
        )));
        authority_contract(
            &first,
            port_scope(daemon.chats[0], &daemon.root),
            port_open(counter_program(), &daemon.root),
        )
        .await;
        ownership_contract(
            &first,
            port_scope(daemon.chats[0], &daemon.root),
            &second,
            port_scope(daemon.chats[1], &daemon.root),
            port_open(counter_program(), &daemon.root),
        )
        .await;
    }

    #[tokio::test]
    async fn execution_port_revision_contract_matches_both_adapters() {
        use tickloop_harness::test_support::{
            execution_contracts::revision_contract, execution_environment::counter_environment,
        };
        let memory = counter_environment();
        let control = memory.clone();
        revision_contract(
            &memory,
            port_scope(tickloop_ids::generate(), Path::new("/")),
            port_open(vec!["/counter".into()], Path::new("/")),
            |instance| async move {
                control.change_revision(&instance, "2");
            },
        )
        .await;
        let daemon = Daemon::start(1).await;
        let tools = Arc::new(daemon.tools());
        let environment = LocalEnvironment::with_ticks(tools.clone());
        let script = daemon.root.join("contract-evolving.py");
        std::fs::write(&script, EVOLVING).unwrap();
        let chat = daemon.chats[0];
        revision_contract(
            &environment,
            port_scope(chat, &daemon.root),
            port_open(python_program(&script, &[]), &daemon.root),
            |instance| async move {
                run_tool(
                    &tools,
                    chat,
                    "TickCall",
                    json!({"tick_instance_id":instance,"tool":"evolve","args":{}}),
                )
                .await;
            },
        )
        .await;
    }

    #[tokio::test]
    async fn execution_port_observation_contract_matches_both_adapters() {
        use tickloop_harness::test_support::{
            execution_contracts::observation_contract, execution_environment::counter_environment,
        };
        let memory = counter_environment();
        observation_contract(
            &memory,
            port_scope(tickloop_ids::generate(), Path::new("/")),
            port_open(vec!["/counter".into()], Path::new("/")),
        )
        .await;
        let daemon = Daemon::start(1).await;
        let environment = LocalEnvironment::with_ticks(Arc::new(daemon.tools()));
        observation_contract(
            &environment,
            port_scope(daemon.chats[0], &daemon.root),
            port_open(counter_program(), &daemon.root),
        )
        .await;
    }

    #[tokio::test]
    async fn execution_port_restart_contract_matches_both_adapters() {
        use tickloop_harness::test_support::{
            execution_contracts::restart_contract, execution_environment::counter_environment,
        };
        let memory = counter_environment();
        restart_contract(
            &memory,
            port_scope(tickloop_ids::generate(), Path::new("/")),
            port_open(vec!["/counter".into()], Path::new("/")),
            |_| async {
                memory.restart();
            },
        )
        .await;
        let daemon = Daemon::start(1).await;
        let environment = LocalEnvironment::with_ticks(Arc::new(daemon.tools()));
        restart_contract(
            &environment,
            port_scope(daemon.chats[0], &daemon.root),
            port_open(counter_program(), &daemon.root),
            |opened| async move {
                let business = opened["business_pid"].as_u64().unwrap() as u32;
                let launcher = tickhost::process::sample(business).unwrap().parent;
                let host = tickhost::process::sample(launcher).unwrap().parent;
                unsafe { libc::kill(host as libc::pid_t, libc::SIGKILL) };
                assert!(super::gone(business).await);
            },
        )
        .await;
    }

    const RESTORING_TERMINAL: &str = r#"
import sys, termios
mode = termios.tcgetattr(0)
mode[3] &= ~termios.ECHO
termios.tcsetattr(0, termios.TCSANOW, mode)
def draw(value):
    sys.stdout.write('\x1b[2J\x1b[H' + value)
    sys.stdout.flush()
draw('A')
for line in sys.stdin:
    draw(line.strip())
"#;

    #[tokio::test]
    async fn execution_port_terminal_baselines_match_both_adapters() {
        use tickloop_harness::test_support::{
            execution_contracts::terminal_baseline_contract,
            execution_environment::MemoryEnvironment,
        };
        let memory = Arc::new(MemoryEnvironment::new());
        memory.install_terminal_program("/terminal", "A", |screen, input| {
            *screen = String::from_utf8_lossy(input).trim().into()
        });
        let authority = HostToolAuthority::Native {
            endpoint: HostEndpointIdentity::TrustedAdapter {
                scope: tickloop_ids::generate(),
            },
        };
        let mut open = port_open(vec!["/terminal".into()], Path::new("/"));
        open.protocol = false;
        terminal_baseline_contract(
            memory,
            port_scope(tickloop_ids::generate(), Path::new("/")),
            authority,
            open,
        )
        .await;
        let daemon = Daemon::start(1).await;
        let script = daemon.root.join("restoring-terminal.py");
        std::fs::write(&script, RESTORING_TERMINAL).unwrap();
        let tools = Arc::new(daemon.tools());
        let authority = HostToolAuthority::Native {
            endpoint: tools.host_endpoint().unwrap().unwrap(),
        };
        let environment = Arc::new(LocalEnvironment::with_ticks(tools));
        let mut open = port_open(python_program(&script, &[]), &daemon.root);
        open.protocol = false;
        terminal_baseline_contract(
            environment,
            port_scope(daemon.chats[0], &daemon.root),
            authority,
            open,
        )
        .await;
    }

    #[tokio::test]
    async fn execution_port_latest_delivery_contract_on_memory() {
        use tickloop_harness::runtime::environment::ExecutionTools;
        use tickloop_harness::test_support::{
            ScriptedModel, execution_contracts::latest_delivery_contract,
            execution_environment::counter_environment,
        };
        let directory = tempfile::tempdir().unwrap();
        let model = Arc::new(ScriptedModel::new(Vec::new()));
        let environment = Arc::new(counter_environment());
        let authority = HostToolAuthority::Native {
            endpoint: HostEndpointIdentity::TrustedAdapter {
                scope: tickloop_ids::generate(),
            },
        };
        let agent = Agent::with_seams(
            HarnessSeams {
                search: None,
                transcripts: None,
                environment: Some(environment.clone()),
                models: ChosenModels::new(model.clone()),
            },
            HarnessOptions::local(tickloop_harness::test_support::config()),
        )
        .unwrap();
        agent
            .install_host_tools(Arc::new(ExecutionTools::new(
                environment.clone(),
                authority.clone(),
            )))
            .unwrap();
        agent
            .install_tick_stream(Arc::new(HostedTickStream::with_environment(
                environment.clone(),
                authority,
            )))
            .await;
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        latest_delivery_contract(
            &agent,
            &model,
            environment.as_ref(),
            port_scope(chat.into(), directory.path()),
            port_open(vec!["/counter".into()], Path::new("/")),
        )
        .await;
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn execution_port_latest_delivery_contract_on_native() {
        use tickloop_harness::test_support::{
            ScriptedModel, execution_contracts::latest_delivery_contract,
        };
        let model = Arc::new(ScriptedModel::new(Vec::new()));
        let daemon =
            Daemon::start_with(model.clone(), super::support::hosted::IDLE_TIMEOUT, 1).await;
        let environment = LocalEnvironment::with_ticks(Arc::new(daemon.tools()));
        latest_delivery_contract(
            &daemon.runtimes[0],
            &model,
            &environment,
            port_scope(daemon.chats[0], &daemon.root),
            port_open(counter_program(), &daemon.root),
        )
        .await;
    }

    /// The Herdr Workspace a Herdr worker's Chats work in.
    const HERDR_WORKSPACE: &str = "workspace-a";
    /// How long a hosted test waits for its next event. It outlasts TickManage's 10-second
    /// wait for a Watch to become ready, during which a run emits nothing.
    const HOSTED_EVENT_WAIT: Duration = Duration::from_secs(30);

    /// How long a hosted test waits for a `high` subscription to deliver: the first delivery
    /// comes one period after subscribing.
    fn high_delivery_wait() -> Duration {
        tickloop_harness::protocol::TickSubscriptionFrequency::High
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
        /// production worker is, with its hosted Tick tools and their Herdr views.
        herdr: Option<std::path::PathBuf>,
        grants: Mutex<Vec<WorkerControlGrant>>,
        runtimes: Mutex<Vec<Agent>>,
        environments: Mutex<Vec<LocalEnvironment>>,
    }

    #[async_trait]
    impl WorkerFactory for GrantedWorkers {
        async fn start(
            &self,
            launch: &LaunchSpec,
            epoch: Uuid,
        ) -> Result<Arc<dyn AgentAccess>, AccessError> {
            let grant = self.control.grant();
            let (runtime, environment) = match &self.herdr {
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
                    build_runtime_with_models(
                        &launch,
                        ChosenModels::new(self.model.clone()),
                        Some(grant.clone()),
                    )
                    .await
                    .map_err(AccessError::unavailable)?
                }
                None => {
                    let mut tools =
                        HostedTickTools::new(DaemonControl::new(grant.clone()), tick_environment());
                    // As the production worker does, an open finds the programs of the Tick
                    // packages installed in the config root.
                    if let Some(config_root) = &launch.config_root {
                        tools = tools
                            .with_plugins(tick_plugins::PluginStore::in_config_root(config_root));
                    }
                    let tools = Arc::new(tools);
                    let environment = LocalEnvironment::with(
                        Arc::new(DaemonControl::new(grant.clone())),
                        tools.clone(),
                    );
                    let runtime = Agent::with_seams(
                        HarnessSeams {
                            search: None,
                            transcripts: None,
                            environment: Some(Arc::new(environment.clone())),
                            models: ChosenModels::new(self.model.clone()),
                        },
                        HarnessOptions {
                            ..HarnessOptions::local(tickloop_harness::test_support::config())
                        },
                    )
                    .map_err(|error| AccessError::unavailable(error.to_string()))?;
                    environment.install(&runtime);
                    runtime.install_host_tools(tools.clone()).unwrap();
                    runtime
                        .install_tick_stream(Arc::new(TickAdapters::builtin(
                            runtime.clone(),
                            Some(Arc::new(HostedTickStream::with_environment(
                                Arc::new(environment.clone()),
                                tools.authority(),
                            ))),
                        )))
                        .await;
                    (runtime, environment)
                }
            };
            let access: Arc<dyn AgentAccess> =
                Arc::new(EmbeddedAgentAccess::for_worker_with_environment(
                    runtime.clone(),
                    epoch,
                    environment.clone(),
                ));
            self.control
                .register(&grant, access.clone(), launch.config_root.as_deref())
                .await;
            self.grants.lock().await.push(grant);
            self.runtimes.lock().await.push(runtime);
            self.environments.lock().await.push(environment);
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
        executions: Vec<tickloop_code_access::ExecutionRef>,
        grants: Vec<WorkerControlGrant>,
        runtimes: Vec<Agent>,
        environments: Vec<LocalEnvironment>,
        controller: Arc<HostController>,
    }

    impl Daemon {
        async fn start(chats: usize) -> Self {
            Self::start_with(
                Arc::new(tickloop_harness::test_support::ScriptedModel::new(
                    Vec::new(),
                )),
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
                .tempdir_in(tickloop_harness::test_support::SHORT_TEMP_ROOT)
                .unwrap();
            let root = directory.path().canonicalize().unwrap();
            let config_root = root.join("state");
            std::fs::create_dir(&config_root).unwrap();
            // A Herdr worker is built as in production, on the bundled default model; its key is
            // saved here so no test reads the developer's own.
            tickloop_harness::model::credentials::save_api_key(
                &config_root,
                "DEEPSEEK_API_KEY",
                "fixture-deepseek-key",
            )
            .unwrap();
            let epoch = Uuid::new_v4();
            let controller = Arc::new(HostController::new(
                &root,
                tickhost::paths::HostPaths::of_config_root(Some(&config_root)),
                epoch,
                super::support::hosted::executable_in(&config_root),
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
                environments: Mutex::new(Vec::new()),
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
                            launch_rules: Vec::new(),
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
                created_chats.push(snapshot.conversation.id.into());
                executions.push(execution);
            }
            let grants = factory.grants.lock().await.clone();
            let runtimes = factory.runtimes.lock().await.clone();
            let environments = factory.environments.lock().await.clone();
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
                environments,
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
        ) -> Result<CommandResult, tickloop_code_access::AccessError> {
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
            super::support::stop_tickhost(
                &tickhost::paths::HostPaths::of_config_root(Some(&self.root.join("state"))).runtime,
                &self.root.join("tickhost.key"),
            );
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
            .prepare_call(chat.into(), name, &input, CancellationToken::new())
            .await
            .unwrap();
        tools
            .execute_prepared(chat.into(), prepared, CancellationToken::new())
            .await
    }

    /// Runs one hosted Tick tool for `chat` that must complete, and returns its answer.
    async fn run_tool(tools: &HostedTickTools, chat: Uuid, name: &str, input: Value) -> Value {
        match call_tool(tools, chat, name, input).await {
            HostCallResult::Completed(result)
            | HostCallResult::CompletedWithImages(result, _)
            | HostCallResult::CompletedWithEffect(result, _) => {
                serde_json::from_str(&result.content).unwrap()
            }
            HostCallResult::NotDispatched(error) | HostCallResult::OutcomeUnknown(error) => {
                panic!("hosted {name} failed: {error}")
            }
        }
    }

    /// The TickManage input that opens the protocol `program` hidden in the daemon's Tickhost.
    fn open_command(daemon: &Daemon, program: Vec<String>) -> Value {
        json!({"action":"open","command":program,"protocol":true,"cwd":daemon.root})
    }

    fn counter_command(daemon: &Daemon) -> Value {
        open_command(daemon, counter_program())
    }

    /// Submits `text` to `chat` and, once its run completes, returns by call id the tool results
    /// of that run and of any child it delegated to in the foreground.
    async fn run_turn(runtime: &Agent, chat: Uuid, text: &str) -> BTreeMap<String, ToolResult> {
        let mut events = runtime.subscribe();
        let receipt = runtime.submit(chat.into(), text.into()).await.unwrap();
        let mut results = BTreeMap::new();
        loop {
            let event =
                tickloop_harness::test_support::next_event_within(&mut events, HOSTED_EVENT_WAIT)
                    .await;
            match event.kind {
                EventKind::ToolCompleted {
                    call_id, result, ..
                } => {
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
            serde_json::json!(
                tickhost::paths::HostPaths::of_config_root(Some(&root.join("state"))).socket()
            )
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
                adapter: tickhost::host::TickAdapter::Terminal,
                command: vec!["/usr/bin/true".into()],
                cwd: root.clone(),
                environment: Vec::new(),
                title: None,
                admission: HostedAdmission {
                    tool: "TickManage".into(),
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
            !tickhost::paths::HostPaths::of_config_root(Some(&root.join("state")))
                .lock()
                .exists(),
            "no request but an admitted open starts a Tickhost"
        );
    }

    #[tokio::test]
    async fn an_open_approved_for_a_replaced_working_directory_is_refused() {
        let directory = tempfile::Builder::new()
            .prefix("thw")
            .tempdir_in(tickloop_harness::test_support::SHORT_TEMP_ROOT)
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
            }),
            Vec::new(),
        );
        let chat = Uuid::new_v4();
        let approved = tools
            .prepare_call(
                chat.into(),
                "TickManage",
                &json!({"action":"open","command":["/bin/sh"],"cwd":work}),
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
            .revalidate_call(chat.into(), &approved, CancellationToken::new())
            .await
            .expect("new entries do not replace the directory");
        std::fs::rename(&work, directory.path().join("work-aside")).unwrap();
        std::fs::create_dir(&work).unwrap();
        let replaced = tools
            .revalidate_call(chat.into(), &approved, CancellationToken::new())
            .await
            .unwrap_err();
        assert!(replaced.contains("working directory changed"), "{replaced}");
        assert!(matches!(
            tools
                .execute_prepared(chat.into(), approved, CancellationToken::new())
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
            .tempdir_in(tickloop_harness::test_support::SHORT_TEMP_ROOT)
            .unwrap();
        let endpoint = directory.path().join("w.sock");
        let listener = tokio::net::UnixListener::bind(&endpoint).unwrap();
        let grant = WorkerControlGrant {
            endpoint,
            daemon_epoch: Uuid::new_v4(),
            binding: Uuid::new_v4(),
            capability: "0".repeat(64),
            host_endpoint: directory.path().join("host.sock"),
        };
        let chat = Uuid::new_v4();
        let instance = tick_protocol::InstanceRef::hosted(Uuid::new_v4());
        let opened = tickhost::host::OpenedTick {
            instance: instance.clone(),
            adapter: tickhost::host::TickAdapter::Protocol,
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
            "TickManage",
            json!({"action":"open","command":["/bin/sh"],"cwd":tickloop_harness::test_support::SHORT_TEMP_ROOT}),
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
            "TickManage",
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
            requests[0]["admission"]["tool"], "TickManage",
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
            "TickManage",
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
                chat.into(),
                "TickCall",
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
            "TickCall",
            json!({"tick_instance_id":instance,"tool":"evolve","args":{}}),
        )
        .await;
        let HostCallResult::NotDispatched(changed) = tools
            .execute_prepared(chat.into(), approved, CancellationToken::new())
            .await
        else {
            panic!("a call prepared against the earlier catalog revision is refused unrun");
        };
        assert!(changed.contains("catalog changed"), "{changed}");
        let read = run_tool(
            &tools,
            chat,
            "TickRead",
            json!({"tick_instance_id":instance}),
        )
        .await;
        assert_eq!(read["data"]["value"], 0, "the stale approval never ran");
        let other_chat = Uuid::new_v4();
        let HostCallResult::NotDispatched(refused) = call_tool(
            &tools,
            other_chat,
            "TickRead",
            json!({"tick_instance_id":instance}),
        )
        .await
        else {
            panic!("another Chat of this worker never reuses the daemon's binding");
        };
        assert!(refused.contains("Forbidden"), "{refused}");
        let described = run_tool(
            &tools,
            chat,
            "TickDescribe",
            json!({"tick_instance_id":instance}),
        )
        .await;
        assert!(
            described["descriptor"]["tools"]
                .to_string()
                .contains("decrement")
        );
        assert!(read.get("descriptor").is_none() && read.get("tools").is_none());
        let closed = run_tool(
            &tools,
            chat,
            "TickManage",
            json!({"action":"close","tick_instance_id":instance}),
        )
        .await;
        assert_eq!(closed["status"], "closed");
        assert!(
            launch_records(&daemon.root, chat).is_empty(),
            "a closed Tick leaves no launch record"
        );
    }

    /// Declares two tools whose schema takes anything: the Tick itself refuses `strict` with
    /// -32602 and fails `jam` with -32000.
    const REFUSING: &str = r#"
import json, sys
tools = [{'name':'strict','description':'Refuse the arguments','input_schema':{'type':'object'},'mutates':True},{'name':'jam','description':'Fail to run','input_schema':{'type':'object'},'mutates':True}]
for line in sys.stdin:
    request = json.loads(line)
    method = request['method']
    if method == 'tick/describe':
        reply = {'result':{'protocol':3,'revision':'1','kind':'example.refusing','title':'Refusing','description':'Refusing','tools':tools}}
    elif method == 'tick/read':
        reply = {'result':{'seq':0,'data':{'value':0}}}
    elif request['params']['tool'] == 'strict':
        reply = {'error':{'code':-32602,'message':'amount is required'}}
    else:
        reply = {'error':{'code':-32000,'message':'the counter is jammed'}}
    print(json.dumps({'jsonrpc':'2.0','id':request['id'], **reply}), flush=True)
"#;

    #[tokio::test(flavor = "multi_thread")]
    async fn a_call_the_tick_refused_or_failed_has_a_known_outcome() {
        let daemon = Daemon::start(1).await;
        let chat = daemon.chats[0];
        let tools = daemon.tools();
        let script = daemon.root.join("refusing.py");
        std::fs::write(&script, REFUSING).unwrap();
        let opened = run_tool(
            &tools,
            chat,
            "TickManage",
            open_command(&daemon, python_program(&script, &[])),
        )
        .await;
        let instance = opened["tick_instance_id"].clone();
        let call = |tool: &str| json!({"tick_instance_id":instance,"tool":tool,"args":{}});
        let HostCallResult::NotDispatched(refused) =
            call_tool(&tools, chat, "TickCall", call("strict")).await
        else {
            panic!("arguments the Tick refuses leave the call unrun");
        };
        assert!(refused.contains("amount is required"), "{refused}");
        let HostCallResult::Completed(failed) =
            call_tool(&tools, chat, "TickCall", call("jam")).await
        else {
            panic!("a tool that ran and failed has a known outcome");
        };
        assert!(failed.is_error);
        let failed: Value = serde_json::from_str(&failed.content).unwrap();
        assert_eq!(failed["status"], "failed", "{failed}");
        assert_eq!(failed["tick_instance_id"], instance);
        assert!(failed["error"].to_string().contains("jammed"), "{failed}");
        run_tool(
            &tools,
            chat,
            "TickManage",
            json!({"action":"close","tick_instance_id":instance}),
        )
        .await;
    }

    /// A Tick package installed in the worker's config root: its program opens by its bare name
    /// and runs as a protocol Tick because its manifest says so, without `protocol: true`; once
    /// the package is uninstalled, the open names the install command.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_installed_package_program_opens_by_name_as_a_protocol_tick() {
        let daemon = Daemon::start(1).await;
        let chat = daemon.chats[0];
        let packages =
            super::support::hosted::install_counter_package(&daemon.config_root, &daemon.root);
        let tools = daemon.tools().with_plugins(packages.clone());
        let open = json!({"action":"open","command":["tick-counter"],"cwd":daemon.root});

        let opened = run_tool(&tools, chat, "TickManage", open.clone()).await;
        let instance = opened["tick_instance_id"].clone();
        let read = run_tool(
            &tools,
            chat,
            "TickRead",
            json!({"tick_instance_id":instance}),
        )
        .await;
        assert_eq!(read["data"]["value"], 0, "{read}");
        let described = run_tool(
            &tools,
            chat,
            "TickDescribe",
            json!({"tick_instance_id":instance}),
        )
        .await;
        assert!(
            described["descriptor"]["tools"]
                .to_string()
                .contains("increment"),
            "{described}"
        );
        assert!(read.get("tools").is_none());
        let called = run_tool(
            &tools,
            chat,
            "TickCall",
            json!({"tick_instance_id":instance,"tool":"increment","args":{"amount":2}}),
        )
        .await;
        assert_eq!(called["status"], "completed", "{called}");
        run_tool(
            &tools,
            chat,
            "TickManage",
            json!({"action":"close","tick_instance_id":instance}),
        )
        .await;

        packages.uninstall("counter", false).unwrap();
        let refused = tools
            .prepare_call(chat.into(), "TickManage", &open, CancellationToken::new())
            .await
            .unwrap_err();
        assert!(
            refused.contains("`tickloop tick install counter`"),
            "{refused}"
        );
    }

    /// A package Tick lost with its host: once its package is uninstalled, a reopen names the
    /// install command; once reinstalled, its program changed and it must be opened anew.
    #[tokio::test(flavor = "multi_thread")]
    async fn reopening_a_lost_package_tick_after_its_package_went_names_the_install_command() {
        let daemon = Daemon::start(1).await;
        let chat = daemon.chats[0];
        let packages =
            super::support::hosted::install_counter_package(&daemon.config_root, &daemon.root);
        let tools = daemon.tools().with_plugins(packages.clone());
        let opened = run_tool(
            &tools,
            chat,
            "TickManage",
            json!({"action":"open","command":["tick-counter"],"cwd":daemon.root}),
        )
        .await;
        let business_pid = opened["business_pid"].as_u64().unwrap() as u32;
        let launcher = tickhost::process::sample(business_pid).unwrap().parent;
        let host = tickhost::process::sample(launcher).unwrap().parent;
        unsafe { libc::kill(host as libc::pid_t, libc::SIGKILL) };
        assert!(super::gone(business_pid).await);
        packages.uninstall("counter", false).unwrap();
        let reopen = json!({"action":"reopen","tick_instance_id":opened["tick_instance_id"]});

        let HostCallResult::NotDispatched(uninstalled) =
            call_tool(&tools, chat, "TickManage", reopen.clone()).await
        else {
            panic!("a reopen of an uninstalled package's program runs nothing");
        };
        assert!(
            uninstalled.contains(
                "executable not found: tick-counter comes from plugin counter, which is not installed; the user installs it from /ticks in the Agent (or `tickloop tick install counter` in a terminal)"
            ),
            "{uninstalled}"
        );
        super::support::hosted::install_counter_package(&daemon.config_root, &daemon.root);
        let HostCallResult::NotDispatched(changed) =
            call_tool(&tools, chat, "TickManage", reopen).await
        else {
            panic!("a reinstalled program is another program");
        };
        assert!(changed.contains("LaunchChanged"), "{changed}");
    }

    /// A package's program starts with its credentials in the environment: the token file of an
    /// `oauth2` package and the key of an `api_key` one. The key reaches the program alone: no
    /// approval evidence, tool result, model request, Chat record or Monitor row holds it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_package_program_starts_with_its_credentials_which_nothing_else_shows() {
        use tick_packages::{ApiKey, OAUTH_TOKEN_FILE_VARIABLE, PackageCredentials};
        use tickloop_harness::protocol::ExecutionMode;
        use tickloop_harness::test_support::{ScriptedModel, auto_classification, call, text};
        const KEY: &str = "sample-key-7f3a9c";
        let model = Arc::new(ScriptedModel::new(Vec::new()));
        let daemon =
            Daemon::start_with(model.clone(), super::support::hosted::IDLE_TIMEOUT, 1).await;
        let (runtime, chat) = (daemon.runtimes[0].clone(), daemon.chats[0]);
        let sources = daemon.root.join("sources");
        let install = |name, auth| {
            super::support::hosted::install_environment_recording_package(
                &daemon.config_root,
                &sources,
                name,
                auth,
            )
        };
        install(
            "keyed",
            json!({"type":"api_key","env":"SAMPLE_API_KEY","label":"Sample key"}),
        );
        let packages = install(
            "signed",
            json!({"type":"oauth2","issuers":[{"label":"Sample","issuer":"https://auth.example.com"}]}),
        );
        let keyed = packages.credentials("keyed").unwrap();
        keyed.save_api_key(&ApiKey::new(KEY).unwrap()).unwrap();
        runtime
            .set_execution_mode(chat.into(), ExecutionMode::Auto)
            .await
            .unwrap();
        let reviewer = Arc::new(ScriptedModel::new(vec![
            text(&auto_classification(false).to_string()),
            text(&auto_classification(false).to_string()),
        ]));
        runtime.set_auto_reviewer(reviewer.clone()).await;
        let open = |program: &str| json!({"action":"open","command":[program],"cwd":daemon.root});
        model.enqueue(call("keyed", "TickManage", open("tick-keyed")));
        model.enqueue(call("signed", "TickManage", open("tick-signed")));
        model.enqueue(text("opened"));

        let results = run_turn(&runtime, chat, "Open the keyed and the signed counters").await;

        for name in ["keyed", "signed"] {
            assert!(!results[name].is_error, "{}", results[name].content);
        }
        let keyed_environment = super::support::hosted::recorded_environment(&sources, "keyed");
        assert_eq!(keyed_environment["SAMPLE_API_KEY"], KEY);
        assert!(!keyed_environment.contains_key(OAUTH_TOKEN_FILE_VARIABLE));
        let signed_environment = super::support::hosted::recorded_environment(&sources, "signed");
        assert_eq!(
            signed_environment[OAUTH_TOKEN_FILE_VARIABLE],
            PackageCredentials::in_config_root(&daemon.config_root, "signed")
                .unwrap()
                .oauth_token_path()
                .to_str()
                .unwrap()
        );
        assert!(!signed_environment.contains_key("SAMPLE_API_KEY"));
        for (name, environment) in [
            ("keyed", &keyed_environment),
            ("signed", &signed_environment),
        ] {
            let plugin = packages.get(name).unwrap();
            for variable in ["TICKLOOP_PLUGIN_ROOT", "CLAUDE_PLUGIN_ROOT"] {
                assert_eq!(environment[variable], plugin.root.to_str().unwrap());
            }
            let data = packages.data_directory(name);
            for variable in ["TICKLOOP_PLUGIN_DATA", "CLAUDE_PLUGIN_DATA"] {
                assert_eq!(environment[variable], data.to_str().unwrap());
            }
            assert!(data.is_dir());
        }

        let shown = [
            serde_json::to_string(&results).unwrap(),
            format!("{:?}", reviewer.requests.lock().unwrap()),
            format!("{:?}", model.requests.lock().unwrap()),
            serde_json::to_string(&runtime.snapshot(chat.into()).await.unwrap()).unwrap(),
            serde_json::to_string(&daemon.tools().members(chat.into()).await.unwrap()).unwrap(),
        ];
        assert!(
            reviewer.requests.lock().unwrap().len() == 2,
            "both opens were reviewed"
        );
        for shown in shown {
            assert!(!shown.contains(KEY), "{shown}");
        }

        // Credentials come from Tickloop alone: what the worker inherited gives way, and a
        // package whose user never entered a key starts without one.
        keyed.remove_api_key().unwrap();
        let mut inherited = tick_environment();
        inherited.push((
            OAUTH_TOKEN_FILE_VARIABLE.into(),
            "/elsewhere/oauth2.json".into(),
        ));
        inherited.push(("SAMPLE_API_KEY".into(), "inherited".into()));
        let tools = HostedTickTools::new(DaemonControl::new(daemon.grants[0].clone()), inherited)
            .with_plugins(packages);
        let opened = run_tool(&tools, chat, "TickManage", open("tick-keyed")).await;
        assert_eq!(opened["status"], "opened", "{opened}");
        let keyed_environment = super::support::hosted::recorded_environment(&sources, "keyed");
        assert!(!keyed_environment.contains_key("SAMPLE_API_KEY"));
        assert!(!keyed_environment.contains_key(OAUTH_TOKEN_FILE_VARIABLE));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_marketplace_alias_keeps_program_ownership_credentials_and_directory_variables() {
        let daemon = Daemon::start(1).await;
        let sources = daemon.root.join("sources");
        let plugins = super::support::hosted::install_environment_recording_package(
            &daemon.config_root,
            &sources,
            "original",
            json!({"type":"api_key","env":"SAMPLE_API_KEY","label":"Alias key"}),
        );
        plugins.uninstall("original", false).unwrap();
        let (archive, sha256) =
            crate::tick_package_commands::archive_of(&sources.join("original-package"));
        let path = sources.join("alias.tar.gz");
        std::fs::write(&path, archive).unwrap();
        let alias = plugins
            .install_archive(
                &path,
                tick_plugins::ExpectedArchive {
                    marketplace: "aliases",
                    name: "renamed",
                    version: "1.0.0",
                    sha256: &sha256,
                    url: "https://example.test/alias.tar.gz",
                },
            )
            .unwrap()
            .plugin;
        plugins
            .credentials("renamed")
            .unwrap()
            .save_api_key(&tick_packages::ApiKey::new("fixture-alias-key").unwrap())
            .unwrap();
        let tools = daemon.tools().with_plugins(plugins.clone());
        let opened = run_tool(
            &tools,
            daemon.chats[0],
            "TickManage",
            json!({"action":"open","command":["tick-renamed"],"cwd":daemon.root}),
        )
        .await;
        let environment = super::support::hosted::recorded_environment(&sources, "original");
        assert_eq!(
            environment.get("SAMPLE_API_KEY").map(String::as_str),
            Some("fixture-alias-key")
        );
        assert_eq!(
            environment["TICKLOOP_PLUGIN_ROOT"],
            alias.root.to_str().unwrap()
        );
        assert_eq!(
            environment["CLAUDE_PLUGIN_DATA"],
            plugins.data_directory("renamed").to_str().unwrap()
        );
        let executable = alias.executable().unwrap();
        let resolved = tickhost_connect::program::resolve_executable(
            executable.to_str().unwrap(),
            &daemon.root,
            Some(&plugins),
        )
        .unwrap();
        assert_eq!(resolved.plugin.unwrap().id(), "renamed@aliases");
        run_tool(
            &tools,
            daemon.chats[0],
            "TickManage",
            json!({"action":"close","tick_instance_id":opened["tick_instance_id"]}),
        )
        .await;
        plugins.uninstall("renamed", false).unwrap();
        assert_eq!(
            tickhost_connect::program::uninstalled_plugin_of(&executable, &plugins).as_deref(),
            Some("renamed")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_plugin_credential_cannot_replace_a_plugin_directory_variable() {
        let daemon = Daemon::start(1).await;
        let sources = daemon.root.join("sources");
        for (name, variable) in [
            ("root-conflict", "CLAUDE_PLUGIN_ROOT"),
            ("data-conflict", "CLAUDE_PLUGIN_DATA"),
        ] {
            let plugins = super::support::hosted::install_environment_recording_package(
                &daemon.config_root,
                &sources,
                name,
                json!({"type":"api_key","env":variable,"label":"Conflicting key"}),
            );
            plugins
                .credentials(name)
                .unwrap()
                .save_api_key(&tick_packages::ApiKey::new("fixture-conflicting-key").unwrap())
                .unwrap();
            let result = call_tool(
                &daemon.tools().with_plugins(plugins),
                daemon.chats[0],
                "TickManage",
                json!({"action":"open","command":[format!("tick-{name}")],"cwd":daemon.root}),
            )
            .await;
            let HostCallResult::NotDispatched(error) = result else {
                panic!("a conflicting credential variable must prevent dispatch");
            };
            assert!(
                error.contains(variable) && error.contains("plugin directory"),
                "{error}"
            );
            assert!(!sources.join(format!("{name}.env")).exists());
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn auto_cleanup_supplies_the_creation_exception_for_the_chats_own_tick() {
        use tickloop_harness::protocol::ExecutionMode;
        use tickloop_harness::test_support::{
            ScriptedModel, auto_classification, call, next_event, text,
        };
        let model = Arc::new(ScriptedModel::new(Vec::new()));
        let daemon =
            Daemon::start_with(model.clone(), super::support::hosted::IDLE_TIMEOUT, 1).await;
        let (runtime, chat) = (daemon.runtimes[0].clone(), daemon.chats[0]);
        runtime
            .set_execution_mode(chat.into(), ExecutionMode::Auto)
            .await
            .unwrap();
        let cleanup = auto_classification(false);
        let reviewer = Arc::new(ScriptedModel::new(vec![
            text(&auto_classification(false).to_string()),
            text(&cleanup.to_string()),
            text(&cleanup.to_string()),
        ]));
        runtime.set_auto_reviewer(reviewer.clone()).await;
        model.enqueue(call("open", "TickManage", counter_command(&daemon)));
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
        model.enqueue(call(
            "close",
            "TickManage",
            json!({"action":"close","tick_instance_id":instance}),
        ));
        model.enqueue(text("closed"));
        let mut events = runtime.subscribe();
        let receipt = runtime
            .submit(chat.into(), "Close the task counter".into())
            .await
            .unwrap();
        let mut prompted = false;
        loop {
            let event = next_event(&mut events).await;
            match event.kind {
                EventKind::PermissionRequested { request } => {
                    prompted = true;
                    runtime
                        .answer_permission(chat.into(), request.id, false)
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
            let tickloop_harness::protocol::Content::Text { text } =
                &requests[1].messages[1].content[0]
            else {
                panic!()
            };
            let snapshot: Value = serde_json::from_str(text).unwrap();
            assert!(snapshot["creation_receipt"].is_object());
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
        assert!(super::gone(opened["business_pid"].as_u64().unwrap() as u32).await);
    }

    /// A Tick never ends for idling: an extension Tick nobody watches, views or uses, and a terminal
    /// Tick whose program exited, both outlast the host's idle timeout, the exited one with its
    /// last screen still readable, until each is closed.
    #[tokio::test(flavor = "multi_thread")]
    async fn ticks_nobody_watches_or_views_outlast_the_idle_timeout_until_closed() {
        let idle_timeout = Duration::from_secs(2);
        let daemon = Daemon::start_with(
            Arc::new(tickloop_harness::test_support::ScriptedModel::new(
                Vec::new(),
            )),
            idle_timeout,
            1,
        )
        .await;
        let chat = daemon.chats[0];
        let tools = daemon.tools();
        let counter = run_tool(&tools, chat, "TickManage", counter_command(&daemon)).await;
        let business_pid = counter["business_pid"].as_u64().unwrap() as u32;
        let exited = run_tool(
            &tools,
            chat,
            "TickManage",
            json!({"action":"open","command":["/bin/sh","-c","printf finished"],"cwd":daemon.root}),
        )
        .await;

        tokio::time::sleep(idle_timeout * 2 + Duration::from_millis(500)).await;
        assert!(
            super::alive(business_pid),
            "an unwatched, unviewed Tick does not idle out"
        );
        let read = run_tool(
            &tools,
            chat,
            "TickRead",
            json!({"tick_instance_id":exited["tick_instance_id"]}),
        )
        .await;
        assert_ne!(read["data"]["exit"], Value::Null, "{read}");
        assert!(
            read["data"]["text"]
                .as_str()
                .is_some_and(|text| text.contains("finished")),
            "an exited terminal Tick keeps its last screen: {read}"
        );

        for opened in [&counter, &exited] {
            run_tool(
                &tools,
                chat,
                "TickManage",
                json!({"action":"close","tick_instance_id":opened["tick_instance_id"]}),
            )
            .await;
        }
        assert!(super::gone(business_pid).await, "closing ends it");
    }

    /// A Loop subscribed to a hosted Tick: the quiet Tick costs no model call, an update wakes
    /// the Loop with the new content, and stopping pauses its subscription while both the
    /// extension and Terminal Tick keep running.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_hosted_tick_update_reaches_the_loop_subscribed_to_it() {
        let model = Arc::new(tickloop_harness::test_support::ScriptedModel::new(
            Vec::new(),
        ));
        let daemon = Daemon::start_with(model.clone(), Duration::from_secs(2), 1).await;
        let (runtime, chat) = (daemon.runtimes[0].clone(), daemon.chats[0]);
        let tools = daemon.tools();
        let opened = run_tool(&tools, chat, "TickManage", counter_command(&daemon)).await;
        let instance = opened["tick_instance_id"].clone();
        let business_pid = opened["business_pid"].as_u64().unwrap() as u32;
        model.enqueue(tickloop_harness::test_support::call(
            "subscribe",
            "TickManage",
            json!({"action":"subscribe","tick_instance_id":instance}),
        ));
        model.enqueue(tickloop_harness::test_support::text("watching"));
        let mut events = runtime.subscribe();
        runtime
            .submit(chat.into(), "Watch the counter".into())
            .await
            .unwrap();
        loop {
            match tickloop_harness::test_support::next_event(&mut events)
                .await
                .kind
            {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        let subscriptions = runtime.tick_subscriptions(chat.into()).await.unwrap();
        assert_eq!(subscriptions.len(), 1, "{subscriptions:?}");

        tokio::time::sleep(Duration::from_millis(5500)).await;
        assert!(super::alive(business_pid), "the quiet Tick keeps running");
        assert_eq!(
            model.requests.lock().unwrap().len(),
            2,
            "a quiet Tick wakes nothing"
        );
        model.enqueue(tickloop_harness::test_support::text("update observed"));
        run_tool(
            &tools,
            chat,
            "TickCall",
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

        let terminal = run_tool(
            &tools,
            chat,
            "TickManage",
            json!({"action":"open","command":["/bin/sh","-c","sleep 300"],"cwd":daemon.root}),
        )
        .await;
        let terminal_pid = terminal["business_pid"].as_u64().unwrap() as u32;
        runtime.stop(chat.into()).await.unwrap();
        assert_eq!(
            runtime.tick_subscriptions(chat.into()).await.unwrap().len(),
            1,
            "stopping pauses the subscription and keeps it"
        );
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert!(
            super::alive(business_pid),
            "stopping the Loop leaves the Tick running"
        );
        assert!(
            super::alive(terminal_pid),
            "stopping the Loop leaves its Terminal Tick running"
        );
        for instance in [instance, terminal["tick_instance_id"].clone()] {
            run_tool(
                &tools,
                chat,
                "TickManage",
                json!({"action":"close","tick_instance_id":instance}),
            )
            .await;
        }
    }

    /// A Tick whose state holds a thousand candles, about 140 KiB, newest last, and whose
    /// `advance` adds one and publishes the whole state again.
    const CANDLES: &str = r#"
import json, sys
candles = [{'i': i, 'time': '2026-10-14T01:%02d:%02dZ' % (i // 60 % 60, i % 60), 'open': '512.50', 'high': '513.00', 'low': '512.00', 'close': '512.%02d' % (i % 100), 'volume': 1000 + i, 'turnover': '512500.00'} for i in range(1000)]
seq = 0
def state():
    return {'seq': seq, 'data': {'symbol': '700.HK', 'candles': candles}}
tools = [{'name':'advance','description':'Add a candle','input_schema':{'type':'object'},'mutates':True},{'name':'history','description':'All candles','input_schema':{'type':'object'},'mutates':False}]
for line in sys.stdin:
    request = json.loads(line)
    method = request['method']
    if method == 'tick/describe':
        result = {'protocol':3,'revision':'1','kind':'example.candles','title':'Candles','description':'Candles','recover_with':'history','tools':tools,'state':{'series':[{'path':'/candles','newest':'last'}]}}
    elif method == 'tick/read':
        result = state()
    elif request['params']['tool'] == 'advance':
        candles = candles[1:] + [dict(candles[-1], i=candles[-1]['i'] + 1)]
        seq += 1
        print(json.dumps({'jsonrpc':'2.0','method':'tick/update','params':state()}), flush=True)
        result = {'newest': candles[-1]['i']}
    else:
        result = {'candles': candles}
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}), flush=True)
"#;

    /// The Tick delivery payload in the newest user message of `messages`.
    fn delivery_payload(messages: &[tickloop_harness::protocol::Message]) -> Value {
        let text = messages
            .iter()
            .rev()
            .filter(|message| message.role == tickloop_harness::protocol::Role::User)
            .flat_map(|message| &message.content)
            .find_map(|content| match content {
                tickloop_harness::protocol::Content::Text { text }
                    if text.contains("<tick-delivery>") =>
                {
                    Some(text.clone())
                }
                _ => None,
            })
            .expect("the Loop was woken by a Tick delivery");
        let start = text.find("<tick-delivery>").unwrap() + "<tick-delivery>".len();
        let end = text.find("</tick-delivery>").unwrap();
        serde_json::from_str(
            &text[start..end]
                .replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&amp;", "&"),
        )
        .unwrap()
    }

    /// A subscribed extension Tick whose state is far larger than a delivery wakes its Loop with
    /// that state trimmed by segments, never with empty data: the delivery says how many updates
    /// it merged, their seq span, and what it trimmed.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_large_tick_state_is_delivered_trimmed_never_empty() {
        let model = Arc::new(tickloop_harness::test_support::ScriptedModel::new(
            Vec::new(),
        ));
        let daemon = Daemon::start_with(model.clone(), Duration::from_secs(2), 1).await;
        let (runtime, chat) = (daemon.runtimes[0].clone(), daemon.chats[0]);
        let tools = daemon.tools();
        let script = daemon.root.join("candles.py");
        std::fs::write(&script, CANDLES).unwrap();
        let opened = run_tool(
            &tools,
            chat,
            "TickManage",
            open_command(&daemon, python_program(&script, &[])),
        )
        .await;
        let instance = opened["tick_instance_id"].clone();
        model.enqueue(tickloop_harness::test_support::call(
            "subscribe",
            "TickManage",
            json!({"action":"subscribe","tick_instance_id":instance}),
        ));
        model.enqueue(tickloop_harness::test_support::text("watching"));
        let mut events = runtime.subscribe();
        runtime
            .submit(chat.into(), "Watch the candles".into())
            .await
            .unwrap();
        loop {
            match tickloop_harness::test_support::next_event(&mut events)
                .await
                .kind
            {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }

        model.enqueue(tickloop_harness::test_support::text("candles noted"));
        for _ in 0..3 {
            run_tool(
                &tools,
                chat,
                "TickCall",
                json!({"tick_instance_id":instance,"tool":"advance","args":{}}),
            )
            .await;
        }
        loop {
            let event = tokio::time::timeout(high_delivery_wait(), events.recv())
                .await
                .expect("the updates wake the Loop")
                .unwrap();
            match event.kind {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }

        let payload = delivery_payload(&model.requests.lock().unwrap()[2].messages);
        assert!(
            payload.to_string().len() <= tickloop_harness::protocol::MAX_DELIVERY_PAYLOAD_BYTES,
            "{payload}"
        );
        let item = &payload["items"][0];
        assert_eq!(item["updates"], 3, "{item}");
        assert_eq!(item["seq"], "1→3", "{item}");
        assert_eq!(item["data"]["symbol"], "700.HK", "{item}");
        let candles = item["data"]["candles"]
            .as_array()
            .expect("the candles stay");
        assert_eq!(candles[0]["i"], 3, "the oldest candle stays");
        assert_eq!(
            candles.last().unwrap()["i"],
            1002,
            "the newest candle stays"
        );
        let trimmed = item["trimmed"]
            .as_array()
            .expect("the delivery says what it trimmed");
        assert_eq!(trimmed.len(), 1, "{item}");
        assert_eq!(trimmed[0]["path"], "/candles");
        assert_eq!(trimmed[0]["total"], 1000);
        assert_eq!(trimmed[0]["rule"], "newest_last");
        assert_eq!(item["recover_with"], "history");
    }

    /// The TickManage input that opens `/bin/cat` as a terminal Tick in the daemon's Tickhost.
    fn cat_command(daemon: &Daemon) -> Value {
        json!({"action":"open","command":["/bin/cat"],"cwd":daemon.root})
    }

    /// Samples `watch` of `instance` until it reports a screen that `wanted` accepts.
    async fn sample_until(
        stream: &HostedTickStream,
        watch: Uuid,
        instance: &tick_protocol::InstanceRef,
        wanted: impl Fn(&Value) -> bool,
    ) -> tickloop_harness::runtime::tick_stream::TickObservation {
        for _ in 0..400 {
            if let Some(sampled) = stream.sample(tickloop_ids::Id::from(watch), instance).await
                && wanted(&sampled.data)
            {
                return sampled;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("no sample of {watch} showed what was expected");
    }

    /// A program opened without `protocol` runs as a terminal Tick: TickRead returns its screen
    /// text, send_input types into it, and it declares no tools. Its Watches get no screen
    /// pushed: each starts from the screen as it is when it joins, is told once that the screen
    /// changed since it last got it, and a sample reports the screen only when its text, or
    /// whether the program exited, differs from what that Watch last got. The program's exit
    /// tells every Watch to sample at once.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_terminal_tick_is_sampled_against_what_each_watch_last_got() {
        let daemon = Daemon::start(1).await;
        let chat = daemon.chats[0];
        let tools = Arc::new(daemon.tools());
        let opened = run_tool(&tools, chat, "TickManage", cat_command(&daemon)).await;
        let instance = opened["tick_instance_id"].clone();
        assert!(
            instance.as_str().unwrap().starts_with("hosted:"),
            "{opened}"
        );
        let read = run_tool(
            &tools,
            chat,
            "TickRead",
            json!({"tick_instance_id":instance}),
        )
        .await;
        assert_eq!(read["source"], "terminal", "{read}");
        let described = run_tool(
            &tools,
            chat,
            "TickDescribe",
            json!({"tick_instance_id":instance}),
        )
        .await;
        assert_eq!(
            described["descriptor"]["command"],
            json!([std::fs::canonicalize("/bin/cat").unwrap()])
        );
        assert!(read.get("command").is_none());
        assert_eq!(read["data"]["exit"], Value::Null);
        let refused = tools
            .prepare_call(
                chat.into(),
                "TickCall",
                &json!({"tick_instance_id":instance,"tool":"increment","args":{}}),
                CancellationToken::new(),
            )
            .await
            .err()
            .unwrap();
        assert!(refused.contains("terminal Tick"), "{refused}");

        let (first, second) = (
            tickloop_harness::runtime::tick_stream::WatchId::new(),
            tickloop_harness::runtime::tick_stream::WatchId::new(),
        );
        let stream = HostedTickStream::with_environment(
            Arc::new(LocalEnvironment::with_ticks(tools.clone())),
            tools.authority(),
        );
        let authority = tools
            .tool_authorities("TickRead")
            .unwrap()
            .pop_first()
            .unwrap();
        stream.set_watch_chats(BTreeMap::from([(first, chat), (second, chat)]));
        stream.set_watch_authorities(BTreeMap::from([
            (first, authority.clone()),
            (second, authority),
        ]));
        let mut handle = stream.open(&Default::default());
        let target: tick_protocol::InstanceRef = serde_json::from_value(instance.clone()).unwrap();
        handle
            .desired
            .send_replace(BTreeMap::from([(first, target.clone())]));
        let mut seen = Vec::new();
        wait_for(
            &mut handle.events,
            &mut seen,
            0,
            "the first baseline",
            |event| is_ready(event, first.into()),
        )
        .await;
        assert!(
            stream.sample(first, &target).await.is_none(),
            "an unchanged screen is no update"
        );

        let typed = run_tool(
            &tools,
            chat,
            "TickManage",
            json!({"action":"send_input","tick_instance_id":instance,"text":"hello terminal","keys":["Enter"]}),
        )
        .await;
        assert_eq!(typed["written"], 15, "{typed}");
        let typed_at = seen.len();
        wait_for(
            &mut handle.events,
            &mut seen,
            typed_at,
            "the first Watch's pending change",
            |event| is_screen_change(event, first.into(), true),
        )
        .await;
        let changed = sample_until(&stream, first.into(), &target, |data| {
            data["text"]
                .as_str()
                .unwrap()
                .matches("hello terminal")
                .count()
                == 2
        })
        .await;
        assert_eq!(
            changed.kind,
            tickloop_harness::protocol::ObservationKind::Terminal { truncated: false }
        );
        assert_eq!(changed.data["exit"], Value::Null);
        assert!(
            stream.sample(first, &target).await.is_none(),
            "the screen it last got is no update again"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        while let Ok(event) = handle.events.try_recv() {
            seen.push(event);
        }
        let last_change = seen.iter().rev().find_map(|event| match event {
            TickStreamEvent::ScreenChange {
                audience,
                is_pending,
                ..
            } if audience.contains(&first) => Some(*is_pending),
            _ => None,
        });
        assert_eq!(
            last_change,
            Some(false),
            "the samples settled the change and the screen stayed as sampled: {seen:#?}"
        );

        handle.desired.send_replace(BTreeMap::from([
            (first, target.clone()),
            (second, target.clone()),
        ]));
        let joined = seen.len();
        wait_for(
            &mut handle.events,
            &mut seen,
            joined,
            "the second baseline",
            |event| is_ready(event, second.into()),
        )
        .await;
        assert!(
            stream.sample(second, &target).await.is_none(),
            "a later Watch starts from the screen as it is when it joins"
        );

        let exiting = seen.len();
        run_tool(
            &tools,
            chat,
            "TickManage",
            json!({"action":"send_input","tick_instance_id":instance,"keys":["Ctrl-D"]}),
        )
        .await;
        for watch in [first, second] {
            wait_for(
                &mut handle.events,
                &mut seen,
                exiting,
                "the program's exit",
                |event| matches!(event, TickStreamEvent::ProgramExited { audience, .. } if audience.contains(&watch)),
            )
            .await;
        }
        for watch in [first, second] {
            let exited = sample_until(&stream, watch.into(), &target, |data| {
                data["exit"] != Value::Null
            })
            .await;
            assert_eq!(exited.data["exit"], json!({"code":0,"signal":null}));
            assert!(
                exited.data["text"]
                    .as_str()
                    .unwrap()
                    .contains("hello terminal")
            );
        }
        let member = tools
            .members(chat.into())
            .await
            .unwrap()
            .into_iter()
            .find(|member| member.instance == target)
            .unwrap();
        assert_eq!(
            member.state,
            tickloop_harness::protocol::HostedMemberState::Exited
        );
        assert_eq!(
            member.exit,
            Some(tickloop_harness::protocol::HostedProgramExit {
                code: Some(0),
                signal: None
            })
        );
        assert_eq!(member.resources, None, "an exited program is not sampled");
        let late = call_tool(
            &tools,
            chat,
            "TickManage",
            json!({"action":"send_input","tick_instance_id":instance,"text":"late"}),
        )
        .await;
        let HostCallResult::NotDispatched(reason) = late else {
            panic!("input for a program that exited is never dispatched");
        };
        assert!(reason.contains("ProgramExited"), "{reason}");

        let closed = seen.len();
        run_tool(
            &tools,
            chat,
            "TickManage",
            json!({"action":"close","tick_instance_id":instance}),
        )
        .await;
        for watch in [first, second] {
            wait_for(
                &mut handle.events,
                &mut seen,
                closed,
                "the Tick's end",
                |event| is_state(event, watch.into(), TickConnectionState::Gone),
            )
            .await;
        }
        assert!(stream.sample(first, &target).await.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn changing_a_hosted_terminal_frequency_keeps_its_pending_screen() {
        let model = Arc::new(tickloop_harness::test_support::ScriptedModel::new(
            Vec::new(),
        ));
        let daemon =
            Daemon::start_with(model.clone(), super::support::hosted::IDLE_TIMEOUT, 1).await;
        let (runtime, chat) = (daemon.runtimes[0].clone(), daemon.chats[0]);
        let tools = daemon.tools();
        let opened = run_tool(&tools, chat, "TickManage", cat_command(&daemon)).await;
        let instance = opened["tick_instance_id"].clone();
        model.enqueue(tickloop_harness::test_support::call(
            "subscribe",
            "TickManage",
            json!({"action":"subscribe","tick_instance_id":instance}),
        ));
        model.enqueue(tickloop_harness::test_support::text("watching"));
        let mut events = runtime.subscribe();
        runtime
            .submit(chat.into(), "Watch this terminal".into())
            .await
            .unwrap();
        loop {
            match tickloop_harness::test_support::next_event(&mut events)
                .await
                .kind
            {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        run_tool(&tools, chat, "TickManage", json!({"action":"send_input","tick_instance_id":instance,"text":"pending update","keys":["Enter"]})).await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let before = loop {
            let status = runtime
                .tick_subscriptions(chat.into())
                .await
                .unwrap()
                .remove(0);
            if status.has_pending_change {
                break status;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "pending screen never arrived: {status:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        for frequency in [
            tickloop_harness::protocol::TickSubscriptionFrequency::Medium,
            tickloop_harness::protocol::TickSubscriptionFrequency::Low,
            tickloop_harness::protocol::TickSubscriptionFrequency::High,
        ] {
            runtime
                .update_tick_subscription(
                    chat.into(),
                    before.id,
                    TickSubscriptionChange::Frequency(frequency),
                )
                .await
                .unwrap();
            for _ in 0..10 {
                tokio::time::sleep(Duration::from_millis(25)).await;
                let status = runtime
                    .tick_subscriptions(chat.into())
                    .await
                    .unwrap()
                    .remove(0);
                assert!(
                    status.has_pending_change,
                    "changing to {frequency:?} cleared the unread screen: {status:?}"
                );
                assert!(status.pending_since_ms.is_some());
                assert_eq!(status.last_delivery_ms, before.last_delivery_ms);
            }
        }
    }

    /// A Loop subscribed to a terminal Tick at the default high frequency: the screen is read
    /// once its period elapsed, and the input and exit that changed it wake the Loop once with
    /// the new text and the exit code.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_high_subscription_to_a_terminal_tick_delivers_its_changed_screen_and_exit() {
        let model = Arc::new(tickloop_harness::test_support::ScriptedModel::new(
            Vec::new(),
        ));
        let daemon =
            Daemon::start_with(model.clone(), super::support::hosted::IDLE_TIMEOUT, 1).await;
        let (runtime, chat) = (daemon.runtimes[0].clone(), daemon.chats[0]);
        let tools = daemon.tools();
        let opened = run_tool(&tools, chat, "TickManage", cat_command(&daemon)).await;
        let instance = opened["tick_instance_id"].clone();
        model.enqueue(tickloop_harness::test_support::call(
            "subscribe",
            "TickManage",
            json!({"action":"subscribe","tick_instance_id":instance}),
        ));
        model.enqueue(tickloop_harness::test_support::text("watching"));
        let mut events = runtime.subscribe();
        runtime
            .submit(chat.into(), "Watch the terminal".into())
            .await
            .unwrap();
        loop {
            match tickloop_harness::test_support::next_event(&mut events)
                .await
                .kind
            {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        let subscriptions = runtime.tick_subscriptions(chat.into()).await.unwrap();
        assert_eq!(
            subscriptions[0].frequency,
            tickloop_harness::protocol::TickSubscriptionFrequency::High
        );
        assert_eq!(
            subscriptions[0].link,
            tickloop_harness::protocol::TickLinkState::Live
        );

        model.enqueue(tickloop_harness::test_support::text("screen observed"));
        run_tool(
            &tools,
            chat,
            "TickManage",
            json!({"action":"send_input","tick_instance_id":instance,"text":"quote ready","keys":["Enter","Ctrl-D"]}),
        )
        .await;
        loop {
            let event = tokio::time::timeout(high_delivery_wait(), events.recv())
                .await
                .expect("the changed screen wakes the Loop")
                .unwrap();
            match event.kind {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 3, "one delivery");
        let woken = serde_json::to_string(&requests[2].messages).unwrap();
        assert!(woken.contains("quote ready"), "{woken}");
        assert!(
            woken.contains("\\\"code\\\":0") || woken.contains("\"code\":0"),
            "{woken}"
        );
    }

    /// A subscribed terminal Tick whose program fails wakes the Loop at once with its last screen
    /// and exit code, long before the subscription's period would have read it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_subscribed_terminal_tick_that_fails_is_delivered_at_once() {
        let model = Arc::new(tickloop_harness::test_support::ScriptedModel::new(
            Vec::new(),
        ));
        let daemon =
            Daemon::start_with(model.clone(), super::support::hosted::IDLE_TIMEOUT, 1).await;
        let (runtime, chat) = (daemon.runtimes[0].clone(), daemon.chats[0]);
        let tools = daemon.tools();
        let opened = run_tool(
            &tools,
            chat,
            "TickManage",
            json!({"action":"open","command":["/bin/sh","-c","read symbol; echo \"no quote for $symbol\"; exit 3"],"cwd":daemon.root}),
        )
        .await;
        let instance = opened["tick_instance_id"].clone();
        model.enqueue(tickloop_harness::test_support::call(
            "subscribe",
            "TickManage",
            json!({"action":"subscribe","tick_instance_id":instance}),
        ));
        model.enqueue(tickloop_harness::test_support::text("watching"));
        let mut events = runtime.subscribe();
        runtime
            .submit(chat.into(), "Watch the quote script".into())
            .await
            .unwrap();
        loop {
            match tickloop_harness::test_support::next_event(&mut events)
                .await
                .kind
            {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }

        model.enqueue(tickloop_harness::test_support::text("the script failed"));
        run_tool(
            &tools,
            chat,
            "TickManage",
            json!({"action":"send_input","tick_instance_id":instance,"text":"700.HK","keys":["Enter"]}),
        )
        .await;
        let period = tickloop_harness::protocol::TickSubscriptionFrequency::High
            .period()
            .unwrap();
        let immediately = Duration::from_secs(10);
        assert!(immediately < period / 4);
        loop {
            let event = tokio::time::timeout(immediately, events.recv())
                .await
                .expect("the failed program wakes the Loop at once")
                .unwrap();
            match event.kind {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 3, "one delivery");
        let woken = serde_json::to_string(&requests[2].messages).unwrap();
        assert!(woken.contains("no quote for 700.HK"), "{woken}");
        assert!(
            woken.contains("\\\"code\\\":3") || woken.contains("\"code\":3"),
            "{woken}"
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

    /// A worker in a Herdr View, wired as production wires it, opens a Tick in the Tickhost and
    /// asks Herdr nothing: the Tick joins the Chat's Monitor without a subscription, streams to
    /// the Loop that subscribes to it, and closes through its host. A placement an earlier build
    /// offered, repeated from an old conversation, is refused before anything runs.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_herdr_worker_opens_ticks_in_the_tickhost_and_asks_herdr_nothing() {
        use tickloop_code_access::ConfigurationChange;
        use tickloop_harness::protocol::EventKind;
        use tickloop_harness::protocol::ExecutionMode;

        let directory = tempfile::Builder::new()
            .prefix("thh")
            .tempdir_in(tickloop_harness::test_support::SHORT_TEMP_ROOT)
            .unwrap();
        let herdr = RecordingHerdr::start(directory.path().join("herdr.sock"));
        let model = Arc::new(tickloop_harness::test_support::ScriptedModel::new(
            Vec::new(),
        ));
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
        model.enqueue(tickloop_harness::test_support::call(
            "open",
            "TickManage",
            counter_command(&daemon),
        ));
        model.enqueue(tickloop_harness::test_support::text("opened"));
        let results = run_turn(&runtime, chat, "Open the counter").await;
        assert!(!results["open"].is_error, "{}", results["open"].content);
        let opened: Value = serde_json::from_str(&results["open"].content).unwrap();
        assert_eq!(opened["status"], "opened", "{opened}");
        assert_eq!(opened["host"], "tickhost", "{opened}");
        for retired in ["visibility", "view"] {
            assert!(opened.get(retired).is_none(), "{retired}: {opened}");
        }
        let instance = opened["tick_instance_id"].as_str().unwrap().to_owned();
        assert!(instance.starts_with("hosted:"), "{instance}");
        assert!(
            herdr.methods().is_empty(),
            "a hidden open asks Herdr nothing: {:?}",
            herdr.methods()
        );
        let members = runtime.snapshot(chat.into()).await.unwrap().monitor.members;
        assert_eq!(
            members
                .iter()
                .map(|member| (member.instance.as_str(), member.title.as_deref()))
                .collect::<Vec<_>>(),
            vec![(
                instance
                    .parse::<tick_protocol::InstanceRef>()
                    .unwrap()
                    .as_str(),
                Some("Counter")
            )],
            "the hidden Tick joins this Chat's Monitor under its declared title"
        );
        assert!(
            runtime
                .tick_subscriptions(chat.into())
                .await
                .unwrap()
                .is_empty(),
            "opening never subscribes"
        );

        model.enqueue(tickloop_harness::test_support::call(
            "subscribe",
            "TickManage",
            json!({"action":"subscribe","tick_instance_id":instance}),
        ));
        model.enqueue(tickloop_harness::test_support::text("watching"));
        let results = run_turn(&runtime, chat, "Watch the counter").await;
        assert!(
            !results["subscribe"].is_error,
            "{}",
            results["subscribe"].content
        );
        assert_eq!(
            runtime.tick_subscriptions(chat.into()).await.unwrap().len(),
            1
        );
        let mut events = runtime.subscribe();
        model.enqueue(tickloop_harness::test_support::text("update observed"));
        run_tool(
            &daemon.tools(),
            chat,
            "TickCall",
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

        model.enqueue(tickloop_harness::test_support::call(
            "visible",
            "TickManage",
            json!({"action":"open","command":["/bin/cat"],"cwd":daemon.root,"placement":{"type":"new_tab"}}),
        ));
        model.enqueue(tickloop_harness::test_support::call(
            "close",
            "TickManage",
            json!({"action":"close","tick_instance_id":instance}),
        ));
        model.enqueue(tickloop_harness::test_support::text("done"));
        let results = run_turn(&runtime, chat, "Show cat, then close the counter").await;
        assert!(
            herdr.methods().is_empty(),
            "a placement is refused before Herdr is asked: {:?}",
            herdr.methods()
        );
        let visible = &results["visible"];
        assert!(visible.is_error, "{}", visible.content);
        assert!(
            visible
                .content
                .contains("placement is not allowed for open; open takes command, and optionally cwd, protocol"),
            "{}",
            visible.content
        );

        let closed: Value = serde_json::from_str(&results["close"].content).unwrap();
        assert_eq!(closed["status"], "closed", "{closed}");
        assert_eq!(closed["host"], "tickhost", "{closed}");
        assert_eq!(closed["tick_instance_id"], instance);
    }

    /// A child delegated in a Herdr worker inherits only the Tickhost's authority for its Tick
    /// tools, and hosted Ticks stay with the Chat that opened them: the child cannot open one, and
    /// reading its parent's is refused by ownership.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_delegated_child_cannot_open_or_use_its_parents_hosted_tick() {
        use tickloop_code_access::ConfigurationChange;
        use tickloop_harness::protocol::ExecutionMode;
        let directory = tempfile::Builder::new()
            .prefix("thd")
            .tempdir_in(tickloop_harness::test_support::SHORT_TEMP_ROOT)
            .unwrap();
        let herdr = RecordingHerdr::start(directory.path().join("herdr.sock"));
        let model = Arc::new(tickloop_harness::test_support::ScriptedModel::new(
            Vec::new(),
        ));
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
        let opened = run_tool(&parents, chat, "TickManage", counter_command(&daemon)).await;
        let instance = opened["tick_instance_id"].clone();
        let child_open = counter_command(&daemon);
        for reply in [
            tickloop_harness::test_support::call(
                "delegate",
                "Subagent",
                json!({"task":"Inspect the counter."}),
            ),
            tickloop_harness::test_support::call(
                "child-read",
                "TickRead",
                json!({"tick_instance_id":instance}),
            ),
            tickloop_harness::test_support::call("child-open", "TickManage", child_open),
            tickloop_harness::test_support::text("The child is done."),
            tickloop_harness::test_support::text("Delegated."),
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
        for tool in ["TickRead", "TickCall", "TickManage"] {
            assert_eq!(
                ceiling[tool].as_array().map(Vec::len),
                Some(1),
                "{tool}: {ceiling}"
            );
        }
        assert!(ceiling.get("ReadWorkspace").is_none(), "{ceiling}");
        let read = run_tool(
            &parents,
            chat,
            "TickRead",
            json!({"tick_instance_id":instance}),
        )
        .await;
        assert_eq!(read["data"]["value"], 0, "the parent's Tick is untouched");
    }

    /// A model reopens a Tick lost with its host through TickManage: the reopen waits for
    /// approval like any change, starts a new hidden instance in place of the lost one, and joins
    /// the Chat's Monitor under the lost Tick's title without any subscription.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_model_reopens_a_lost_tick_only_after_approval_as_a_new_instance() {
        let model = Arc::new(tickloop_harness::test_support::ScriptedModel::new(
            Vec::new(),
        ));
        let daemon =
            Daemon::start_with(model.clone(), super::support::hosted::IDLE_TIMEOUT, 1).await;
        let (runtime, chat) = (daemon.runtimes[0].clone(), daemon.chats[0]);
        let tools = daemon.tools();
        let opened = run_tool(&tools, chat, "TickManage", counter_command(&daemon)).await;
        let lost = opened["tick_instance_id"].as_str().unwrap().to_owned();
        let lost_title = launch_records(&daemon.root, chat)[0]["title"].clone();
        let business_pid = opened["business_pid"].as_u64().unwrap() as u32;
        let launcher = tickhost::process::sample(business_pid).unwrap().parent;
        let host = tickhost::process::sample(launcher).unwrap().parent;
        unsafe { libc::kill(host as libc::pid_t, libc::SIGKILL) };
        assert!(super::gone(business_pid).await);
        let host_paths =
            tickhost::paths::HostPaths::of_config_root(Some(&daemon.root.join("state")));

        model.enqueue(tickloop_harness::test_support::call(
            "reopen",
            "TickManage",
            json!({"action":"reopen","tick_instance_id":lost}),
        ));
        model.enqueue(tickloop_harness::test_support::text("reopened"));
        let mut events = runtime.subscribe();
        let receipt = runtime
            .submit(chat.into(), "Reopen the counter".into())
            .await
            .unwrap();
        let mut result = None;
        loop {
            let event = tickloop_harness::test_support::next_event(&mut events).await;
            match event.kind {
                EventKind::PermissionRequested { request } => {
                    assert_eq!(request.input["action"], "reopen");
                    assert!(
                        !host_paths.host_running().unwrap(),
                        "nothing starts before approval"
                    );
                    runtime
                        .answer_permission(chat.into(), request.id, true)
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
        assert!(reopened.get("visibility").is_none(), "{reopened}");
        let stored = |text: &str| text.parse::<tick_protocol::InstanceRef>().unwrap();
        assert_eq!(
            stored(reopened["replaces"].as_str().unwrap()),
            stored(&lost)
        );
        let instance = stored(reopened["tick_instance_id"].as_str().unwrap());
        assert_ne!(instance, stored(&lost), "a new instance");
        assert_ne!(reopened["business_pid"], opened["business_pid"]);
        let member = runtime
            .snapshot(chat.into())
            .await
            .unwrap()
            .monitor
            .members
            .into_iter()
            .find(|member| member.instance == instance)
            .expect("the reopened Tick joins Monitor");
        assert!(lost_title.is_string(), "the lost Tick keeps a title");
        assert_eq!(
            member.title.as_deref(),
            Some("Counter"),
            "the reopened Tick is named by its declaration: {member:?}"
        );
        assert!(
            runtime
                .tick_subscriptions(chat.into())
                .await
                .unwrap()
                .is_empty(),
            "no subscription follows a reopen"
        );
        run_tool(
            &tools,
            chat,
            "TickManage",
            json!({"action":"close","tick_instance_id":instance}),
        )
        .await;
    }

    /// A Watch of a hosted Tick delivers its updates, and once the Tick's host dies it fails
    /// closed as gone, calling no model and never rebinding to a later host.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_hosted_watch_delivers_and_fails_closed_once_its_host_dies() {
        let model = Arc::new(tickloop_harness::test_support::ScriptedModel::new(
            Vec::new(),
        ));
        let daemon =
            Daemon::start_with(model.clone(), super::support::hosted::IDLE_TIMEOUT, 1).await;
        let (runtime, chat) = (daemon.runtimes[0].clone(), daemon.chats[0]);
        let tools = daemon.tools();
        let opened = run_tool(&tools, chat, "TickManage", counter_command(&daemon)).await;
        let instance = opened["tick_instance_id"].clone();
        let mut events = runtime.subscribe();
        model.enqueue(tickloop_harness::test_support::call(
            "watch",
            "TickManage",
            json!({"action":"subscribe","tick_instance_id":instance}),
        ));
        model.enqueue(tickloop_harness::test_support::text("watching"));
        let receipt = runtime
            .submit(chat.into(), "Watch it".into())
            .await
            .unwrap();
        loop {
            let event = tickloop_harness::test_support::next_event(&mut events).await;
            if event.run_id == receipt.run_id && matches!(event.kind, EventKind::Completed { .. }) {
                break;
            }
        }
        let subscriptions = runtime.tick_subscriptions(chat.into()).await.unwrap();
        assert_eq!(subscriptions.len(), 1, "{subscriptions:?}");

        let before = model.requests.lock().unwrap().len();
        model.enqueue(tickloop_harness::test_support::text("update observed"));
        run_tool(
            &tools,
            chat,
            "TickCall",
            json!({"tick_instance_id":instance,"tool":"increment","args":{"amount":1}}),
        )
        .await;
        loop {
            let event = tokio::time::timeout(high_delivery_wait(), events.recv())
                .await
                .expect("the Watch is delivered")
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
        let remaining = subscriptions[0].id;
        let mut is_gone = false;
        for _ in 0..120 {
            let link = runtime
                .tick_subscriptions(chat.into())
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
        run_tool(&tools, chat, "TickManage", counter_command(&daemon)).await;
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
        let model = Arc::new(tickloop_harness::test_support::ScriptedModel::new(
            Vec::new(),
        ));
        let daemon =
            Daemon::start_with(model.clone(), super::support::hosted::IDLE_TIMEOUT, 1).await;
        let (runtime, chat) = (daemon.runtimes[0].clone(), daemon.chats[0]);
        let script = daemon.root.join("refusing.py");
        std::fs::write(&script, REFUSING_TICK).unwrap();
        let opened = run_tool(
            &daemon.tools(),
            chat,
            "TickManage",
            open_command(&daemon, python_program(&script, &[])),
        )
        .await;
        let instance = opened["tick_instance_id"].clone();
        let business_pid = opened["business_pid"].as_u64().unwrap() as u32;
        let describes = daemon.root.join("describes");
        let served_before = logged(&describes, "served");
        std::fs::write(daemon.root.join(REFUSE_DESCRIBE), "").unwrap();

        model.enqueue(tickloop_harness::test_support::call(
            "subscribe",
            "TickManage",
            json!({"action":"subscribe","tick_instance_id":instance}),
        ));
        model.enqueue(tickloop_harness::test_support::text("watching"));
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
        let rows = runtime.tick_subscriptions(chat.into()).await.unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].link, TickLinkState::Disconnected);
        assert_eq!(model.requests.lock().unwrap().len(), 2, "nothing woke");
        assert!(super::alive(business_pid), "the Tick keeps running");

        runtime
            .update_tick_subscription(chat.into(), rows[0].id, TickSubscriptionChange::Cancel)
            .await
            .unwrap();
        model.enqueue(tickloop_harness::test_support::call(
            "again",
            "TickManage",
            json!({"action":"subscribe","tick_instance_id":instance}),
        ));
        model.enqueue(tickloop_harness::test_support::text("watching again"));
        let results = run_turn(&runtime, chat, "Watch it again").await;
        let again: Value = serde_json::from_str(&results["again"].content).unwrap();
        assert_eq!(again["ready"], true, "{again}");
        assert_eq!(
            logged(&describes, "served"),
            served_before + 1,
            "the new Watch registered at once"
        );
        let rows = runtime.tick_subscriptions(chat.into()).await.unwrap();
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
        matches!(event, TickStreamEvent::State { state, audience, .. } if *state == wanted && audience.contains(&tickloop_ids::Id::from(watch)))
    }

    fn is_screen_change(event: &TickStreamEvent, watch: Uuid, pending: bool) -> bool {
        matches!(event, TickStreamEvent::ScreenChange { audience, is_pending, .. } if *is_pending == pending && audience.contains(&tickloop_ids::Id::from(watch)))
    }

    /// An update over the 512 KiB limit once encoded, though its line is within it, is a
    /// protocol violation of its own Tick: the host never forwards it, the call that published
    /// it has an unknown outcome, and that Tick's Watch reaches a terminal state without
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
            "TickManage",
            open_command(
                &daemon,
                python_program(&script, &[GROWN_VALUES.to_string()]),
            ),
        )
        .await;
        let counter = run_tool(&tools, chat, "TickManage", counter_command(&daemon)).await;
        let (large, small) = (
            tickloop_harness::runtime::tick_stream::WatchId::new(),
            tickloop_harness::runtime::tick_stream::WatchId::new(),
        );
        let stream = HostedTickStream::with_environment(
            Arc::new(LocalEnvironment::with_ticks(tools.clone())),
            tools.authority(),
        );
        let authority = tools
            .tool_authorities("TickRead")
            .unwrap()
            .pop_first()
            .unwrap();
        stream.set_watch_chats(BTreeMap::from([(large, chat), (small, chat)]));
        stream.set_watch_authorities(BTreeMap::from([
            (large, authority.clone()),
            (small, authority),
        ]));
        let mut handle = stream.open(&Default::default());
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
                |event| is_ready(event, watch.into()),
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
            "TickCall",
            json!({"tick_instance_id":growing["tick_instance_id"],"tool":"grow","args":{}}),
        )
        .await
        {
            HostCallResult::OutcomeUnknown(_) => None,
            HostCallResult::Completed(result)
            | HostCallResult::CompletedWithImages(result, _)
            | HostCallResult::CompletedWithEffect(result, _) => {
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
        // Latest-only watches may coalesce the protocol error with the Tick's subsequent exit.
        let refused = wait_for(
            &mut handle.events,
            &mut seen,
            grown,
            "the violating Tick's link ending",
            |event| {
                is_state(event, large.into(), TickConnectionState::IdentityUncertain)
                    || is_state(event, large.into(), TickConnectionState::Gone)
            },
        )
        .await;

        run_tool(
            &tools,
            chat,
            "TickCall",
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
        let closed = run_tool(&tools, chat, "TickManage", counter_command(&daemon)).await;
        let kept = run_tool(&tools, chat, "TickManage", counter_command(&daemon)).await;
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
            "TickCall",
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
        use tickloop_code_local_environment::hosted::worker_control::HostedTickPage;
        use tickloop_harness::protocol::HostedMemberState;
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
        let members = tools.members(chat.into()).await.unwrap();
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

        super::support::stop_tickhost(
            &tickhost::paths::HostPaths::of_config_root(Some(&daemon.root.join("state"))).runtime,
            &daemon.root.join("tickhost.key"),
        );
        for tick in &opened {
            assert!(super::gone(tick.business_pid).await, "every Tick ends");
        }
    }

    /// A View attaches to its Chat's hosted Tick through a one-time view grant from the daemon,
    /// which hands it no data token; another Chat's Tick is refused.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_view_gets_a_one_time_attach_grant_for_its_chats_tick_only() {
        use tickhost_connect::hosted::{CAPABILITY_DATA, CONTROL_TIMEOUT, HostConnection, Role};
        let daemon = Daemon::start(2).await;
        let (chat, execution, other) =
            (daemon.chats[0], daemon.executions[0], daemon.executions[1]);
        let tools = daemon.tools();
        let opened = run_tool(&tools, chat, "TickManage", counter_command(&daemon)).await;
        let instance: tick_protocol::InstanceRef =
            serde_json::from_value(opened["tick_instance_id"].clone()).unwrap();
        assert!(
            daemon
                .try_command(AgentCommand::AttachHostedTick {
                    execution: other,
                    instance: instance.clone(),
                })
                .await
                .is_err(),
            "another Chat's Tick is refused"
        );
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
    /// instance with a new process in the lost row's place; its subscription follows because the
    /// user asked for it, and asking again never starts a second one.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_tick_lost_with_its_host_reopens_only_explicitly_as_a_new_instance() {
        use tickloop_harness::protocol::{EventKind, HostedMemberState, TickSubscriptionFrequency};
        let model = Arc::new(tickloop_harness::test_support::ScriptedModel::new(
            Vec::new(),
        ));
        let daemon =
            Daemon::start_with(model.clone(), super::support::hosted::IDLE_TIMEOUT, 1).await;
        let (runtime, chat, execution) = (
            daemon.runtimes[0].clone(),
            daemon.chats[0],
            daemon.executions[0],
        );
        let tools = daemon.tools();
        let opened = run_tool(&tools, chat, "TickManage", counter_command(&daemon)).await;
        let lost: tick_protocol::InstanceRef =
            serde_json::from_value(opened["tick_instance_id"].clone()).unwrap();
        model.enqueue(tickloop_harness::test_support::call(
            "subscribe",
            "TickManage",
            json!({"action":"subscribe","tick_instance_id":lost}),
        ));
        model.enqueue(tickloop_harness::test_support::text("watching"));
        let mut events = runtime.subscribe();
        runtime
            .submit(chat.into(), "Watch it".into())
            .await
            .unwrap();
        loop {
            if matches!(
                tickloop_harness::test_support::next_event(&mut events)
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
        let host_paths =
            tickhost::paths::HostPaths::of_config_root(Some(&daemon.root.join("state")));
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
            restore_error,
        } = daemon
            .command(AgentCommand::ReopenHostedTick {
                execution,
                instance: lost.clone(),
                restore_subscriptions: true,
            })
            .await
        else {
            panic!("a reopened Tick expected");
        };
        assert_ne!(reopened, lost, "a new instance");
        assert_eq!(restore_error, None);
        assert!(
            membership
                .members
                .iter()
                .any(|member| member.instance == reopened)
        );
        assert!(
            membership.is_removed(&lost),
            "the new instance takes the lost row's place"
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

        let subscriptions = runtime.tick_subscriptions(chat.into()).await.unwrap();
        assert_eq!(
            subscriptions
                .iter()
                .map(|row| (row.instance.clone(), row.frequency))
                .collect::<Vec<_>>(),
            [(reopened.clone(), TickSubscriptionFrequency::High)],
            "the subscription follows the new instance at its frequency, as the user asked"
        );

        let CommandResult::HostedTickReopened {
            instance: again, ..
        } = daemon
            .command(AgentCommand::ReopenHostedTick {
                execution,
                instance: lost.clone(),
                restore_subscriptions: true,
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
        let opened = run_tool(&tools, chat, "TickManage", command).await;
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
                    restore_subscriptions: false,
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
    use tickloop_code_local_environment::hosted::controller::HostController;
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
                .tempdir_in(tickloop_harness::test_support::SHORT_TEMP_ROOT)
                .unwrap();
            let paths = HostPaths::of_config_root(Some(directory.path()));
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
                super::support::hosted::executable_in(self.directory.path()),
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
        use tickloop_code_local_environment::hosted::registry::{
            LaunchRecord, LaunchState, Registry,
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
            kept_request.adapter,
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
        // A Tick whose Chat the registry no longer lists.
        let orphan = controller.open(open_for(Uuid::new_v4())).await.unwrap();

        root.reconcile_with_host(&controller, &fixture.owner.root)
            .await
            .unwrap();

        assert!(
            gone(orphan.business_pid).await,
            "a Tick of no listed Chat is reaped"
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

    /// Reconciling keeps the record of a Tick nobody watches past the host's idle timeout, which
    /// keeps running, still ends an orphan whose Chat owns no record, and marks the record of a
    /// Tick the same host no longer runs as ended, kept until its Monitor row is removed. A kept
    /// ended record is never taken for a running Tick: later reconciles still reap orphans,
    /// and a host that dies leaves it ended while the Tick that did run on it is lost.
    #[tokio::test(flavor = "multi_thread")]
    async fn reconcile_keeps_an_unwatched_tick_ends_orphans_and_marks_ended_records() {
        use tickloop_code_local_environment::hosted::registry::{
            LaunchRecord, LaunchState, Registry,
        };
        let idle_timeout = Duration::from_secs(2);
        let fixture = Fixture::with_idle_timeout(idle_timeout);
        let controller = fixture.controller();
        let config_root = fixture.directory.path().join("state");
        std::fs::create_dir(&config_root).unwrap();
        let registry = Registry::default();
        let root = registry.root(&config_root).await.unwrap();
        let mut ticks = Vec::new();
        for _ in 0..2 {
            let request = fixture.counter();
            let opened = controller.open(request.clone()).await.unwrap();
            let mut record = LaunchRecord::opening(
                request.operation,
                fixture.owner.chat,
                request.adapter,
                request.command,
                request.cwd,
                None,
            )
            .unwrap();
            record.state = LaunchState::Live {
                instance: opened.instance.clone(),
                host_epoch: opened.terminal.epoch,
            };
            root.record_launch(&record).await.unwrap();
            ticks.push(opened);
        }
        let [closed, kept] = <[_; 2]>::try_from(ticks).unwrap();
        let orphan_owner = TickOwner {
            root: fixture.owner.root.clone(),
            chat: Uuid::new_v4(),
        };
        let orphan = controller
            .open(counter_request(orphan_owner, fixture.directory.path()))
            .await
            .unwrap();

        tokio::time::sleep(idle_timeout * 2 + Duration::from_millis(500)).await;
        root.reconcile_with_host(&controller, &fixture.owner.root)
            .await
            .unwrap();
        assert!(alive(closed.business_pid) && alive(kept.business_pid));
        assert!(
            gone(orphan.business_pid).await,
            "a Tick whose Chat owns no record is still reaped as an orphan"
        );
        assert_eq!(root.launches(fixture.owner.chat).await.unwrap().len(), 2);

        controller
            .close(Uuid::new_v4(), &fixture.owner, &closed.instance)
            .await
            .unwrap();
        assert!(gone(closed.business_pid).await);
        root.reconcile_with_host(&controller, &fixture.owner.root)
            .await
            .unwrap();
        let states = || async {
            let records = root.launches(fixture.owner.chat).await.unwrap();
            assert_eq!(records.len(), 2, "{records:?}");
            let state_of = |instance| {
                records
                    .iter()
                    .find(|record| record.instance() == Some(instance))
                    .unwrap()
                    .state
                    .clone()
            };
            (state_of(&closed.instance), state_of(&kept.instance))
        };
        let (closed_state, kept_state) = states().await;
        assert!(matches!(closed_state, LaunchState::Ended { .. }));
        assert!(matches!(kept_state, LaunchState::Live { .. }));

        let second_orphan = controller
            .open(counter_request(
                TickOwner {
                    root: fixture.owner.root.clone(),
                    chat: Uuid::new_v4(),
                },
                fixture.directory.path(),
            ))
            .await
            .unwrap();
        root.reconcile_with_host(&controller, &fixture.owner.root)
            .await
            .unwrap();
        assert!(
            gone(second_orphan.business_pid).await,
            "an ended record keeps no orphan running"
        );
        assert!(alive(kept.business_pid));
        assert_eq!(states().await, (closed_state.clone(), kept_state));

        let launcher = tickhost::process::sample(kept.business_pid).unwrap().parent;
        let host = tickhost::process::sample(launcher).unwrap().parent;
        unsafe { libc::kill(host as libc::pid_t, libc::SIGKILL) };
        assert!(gone(kept.business_pid).await);
        root.reconcile_with_host(&controller, &fixture.owner.root)
            .await
            .unwrap();
        let (after_closed, after_kept) = states().await;
        assert_eq!(
            after_closed, closed_state,
            "an ended Tick is never taken for lost"
        );
        assert!(matches!(after_kept, LaunchState::Lost { .. }));
    }

    /// A listing settles only records older than itself: an open that completed while the
    /// host was being listed is neither dropped nor taken for lost.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_record_written_after_a_listing_began_waits_for_the_next_one() {
        use tickloop_code_local_environment::hosted::{
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
            request.adapter,
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
        // A protocol program that never answers keeps its open pending until it times out.
        let pid_file = fixture.directory.path().join("silent.pid");
        let mut never_ready = fixture.counter();
        never_ready.command = vec![
            "/bin/sh".into(),
            "-c".into(),
            r#"echo $$ > "$1"; while :; do sleep 1; done"#.into(),
            "silent".into(),
            pid_file.to_string_lossy().into_owned(),
        ];
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
        let silent: u32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(gone(silent).await, "a failed open leaves nothing running");
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
            .env("TICKLOOP_CONFIG_DIR", fixture.directory.path())
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
        // A view of the Tick's own size paints its hidden screen as it is.
        let (rows, columns) = (tickhost::pty::DEFAULT_ROWS, tickhost::pty::DEFAULT_COLUMNS);
        let mut view = fixture.reader(&opened).await;
        let attached = view
            .request(
                "attach",
                json!({"size":{"rows":rows,"columns":columns}}),
                CONTROL_TIMEOUT,
            )
            .await
            .unwrap();
        assert_eq!(attached["attached"], true);
        let mut shown = vt100::Parser::new(rows, columns, 0);
        let last_line = |shown: &vt100::Parser| {
            shown
                .screen()
                .contents()
                .lines()
                .rfind(|line| !line.trim().is_empty())
                .map(str::to_owned)
        };
        let painted = tokio::time::timeout(Duration::from_secs(10), async {
            while last_line(&shown).as_deref() != Some("line 1499") {
                let frame = view
                    .next_event()
                    .await
                    .unwrap()
                    .expect("the view stays attached");
                assert_eq!(frame["event"], "frame", "{frame}");
                shown.process(frame["data"].as_str().unwrap().as_bytes());
            }
        })
        .await;
        assert!(
            painted.is_ok(),
            "no tail line is lost: {:?}",
            last_line(&shown)
        );
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
                    json!({"tool":"increment","args":{"amount":1},"revision":"1"}),
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
