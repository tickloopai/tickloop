//! Process-boundary regressions using private fixture sockets and real daemon/Worker/Tickhost
//! processes. The queued-delivery case seeds one authenticated checkpoint at its exact boundary.

use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use serde_json::{Value, json};
use tickloop_code_access::{
    AgentAccess, AgentCommand, AgentDetailQuery, CommandResult, EnvironmentValue, ExecutionRef,
    LaunchSpec, LoopAction, ObservationRequest, ObservationValue, UserInput,
};
use tickloop_code_client::{LocalAgentClient, connect_or_start_at};
use tickloop_harness::protocol::{
    ExecutionMode, PendingInput, PromptPolicy, Receipt, TriggerSource,
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::oneshot,
};
use uuid::Uuid;

use super::graceful_upgrade::{PrivateRuntime, command, copy_bundle, snapshot, version};
use crate::support;

const WAIT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(20);
const QUEUED_MARKER: &str = "queued-tick-during-release";

fn response(text: Option<&str>, tool: Option<(&str, &str, Value)>) -> String {
    let (delta, finish) = match tool {
        Some((id, name, input)) => (
            json!({"role":"assistant","tool_calls":[{
                "index":0,"id":id,"type":"function","function":{"name":name,"arguments":input.to_string()}
            }]}),
            "tool_calls",
        ),
        None => (json!({"role":"assistant","content":text.unwrap()}), "stop"),
    };
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"id":"upgrade-boundary","choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
        json!({"id":"upgrade-boundary","choices":[{"index":0,"delta":{},"finish_reason":finish}]})
    )
}

struct Scenario {
    runtime: PrivateRuntime,
    _bundles: tempfile::TempDir,
    workdir: tempfile::TempDir,
    new_executable: PathBuf,
    client: LocalAgentClient,
    execution: ExecutionRef,
}

impl Scenario {
    async fn new(model_url: &str, workdir: tempfile::TempDir) -> Self {
        let runtime = PrivateRuntime::new();
        tickhost::paths::HostPaths::of_config_root(Some(runtime.config_root()))
            .check_session_directories()
            .expect("the private test runtime must leave room for every Tick Session socket");
        let bundles = tempfile::tempdir().unwrap();
        let source = support::fixture_cli::executable();
        let old_executable = copy_bundle(&source, &bundles.path().join("old"));
        let new_executable = copy_bundle(&source, &bundles.path().join("new"));
        let config_root = runtime.config_root().to_path_buf();
        tickloop_code_settings::UserSettings::in_root(&config_root)
            .trust_working_directory(workdir.path())
            .unwrap();
        let transport_path = workdir.path().join("transport.json");
        let mut transport = tickloop_harness::test_support::config();
        transport.protocol = tickloop_harness::protocol::Protocol::ChatCompletions;
        transport.base_url = model_url.trim_end_matches("/anthropic").into();
        transport.api_key_env = "TICKLOOP_FIXTURE_API_KEY".into();
        std::fs::write(&transport_path, serde_json::to_vec(&transport).unwrap()).unwrap();
        let mut launch = LaunchSpec::embedded(workdir.path().to_path_buf());
        launch.config_root = Some(config_root);
        launch.session_persistence = false;
        launch.host_options.execution_mode_override = Some(ExecutionMode::FullAccess);
        launch.host_options.prompt_policy = PromptPolicy::Unrestricted;
        launch.model = Some(tickloop_harness::test_support::config());
        launch.environment = [
            ("PATH", "/usr/bin:/bin"),
            ("TICKLOOP_FIXTURE_API_KEY", "fixture-key"),
            ("DEEPSEEK_API_KEY", "fixture-key"),
        ]
        .into_iter()
        .map(|(name, value)| EnvironmentValue {
            name: name.as_bytes().to_vec(),
            value: value.as_bytes().to_vec(),
        })
        .collect();
        launch.environment.push(EnvironmentValue {
            name: b"TICKLOOP_TRANSPORT_FIXTURE".to_vec(),
            value: transport_path.as_os_str().as_encoded_bytes().to_vec(),
        });
        let client = connect_or_start_at(
            runtime.path().to_path_buf(),
            runtime.executable(&old_executable),
        )
        .await
        .unwrap();
        let created = client
            .execute(command(
                client.daemon_epoch(),
                AgentCommand::Create {
                    launch,
                    initial: None,
                    origin: None,
                },
            ))
            .await
            .unwrap()
            .into_result()
            .unwrap();
        let CommandResult::Created { execution, .. } = created else {
            panic!("expected Created");
        };
        Self {
            runtime,
            _bundles: bundles,
            workdir,
            new_executable,
            client,
            execution,
        }
    }

    async fn submit(&self, text: &str) {
        self.client
            .execute(command(
                self.client.daemon_epoch(),
                AgentCommand::Submit {
                    execution: self.execution,
                    origin: None,
                    input: UserInput::Text {
                        text: text.into(),
                        contexts: Vec::new(),
                        paths: Vec::new(),
                    },
                },
            ))
            .await
            .unwrap()
            .into_result()
            .unwrap();
    }

    fn worker_endpoint(&self) -> PathBuf {
        std::fs::read_dir(self.runtime.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("w-")
                    && path
                        .extension()
                        .is_some_and(|extension| extension == "sock")
            })
            .expect("one real Worker endpoint")
    }

    async fn activate(&self, executable: PathBuf) {
        self.client
            .execute(command(
                self.client.daemon_epoch(),
                AgentCommand::UpgradeRuntime { executable },
            ))
            .await
            .unwrap()
            .into_result()
            .unwrap();
    }

    async fn replacement(&self) -> LocalAgentClient {
        tokio::time::timeout(WAIT, async {
            loop {
                if let Ok(client) =
                    LocalAgentClient::connect(self.runtime.path().join("daemon.sock")).await
                {
                    let state = version(&client).await;
                    if state.executable == self.new_executable
                        && state
                            .workers
                            .iter()
                            .all(|worker| worker.executable == self.new_executable)
                    {
                        return client;
                    }
                }
                tokio::time::sleep(POLL).await;
            }
        })
        .await
        .expect("selected daemon/Worker did not activate")
    }
}

async fn wait_for_text(client: &LocalAgentClient, execution: ExecutionRef, text: &str) {
    tokio::time::timeout(WAIT, async {
        loop {
            let current = snapshot(client, execution).await;
            if current.conversation.messages.iter().any(|message| message.content.iter()
                .any(|block| matches!(block, tickloop_harness::protocol::Content::Text { text: value } if value.contains(text)))) {
                return;
            }
            tokio::time::sleep(POLL).await;
        }
    }).await.expect("model response did not arrive");
}

struct SocketProxy {
    task: tokio::task::JoinHandle<()>,
    published: PathBuf,
    original: PathBuf,
}

impl Drop for SocketProxy {
    fn drop(&mut self) {
        self.task.abort();
        if self.original.exists() {
            let _ = std::fs::remove_file(&self.published);
            let _ = std::fs::rename(&self.original, &self.published);
        }
    }
}

fn replace_socket(path: &Path) -> (UnixListener, PathBuf) {
    let original = path.with_extension("original");
    std::fs::rename(path, &original).unwrap();
    let listener = UnixListener::bind(path).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    (listener, original)
}

async fn read_checkpoint_frame(stream: &mut UnixStream) -> Value {
    let length = stream.read_u32().await.unwrap() as usize;
    assert!(length < 128 * 1024 * 1024);
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn write_checkpoint_frame(stream: &mut UnixStream, frame: &Value) {
    let bytes = serde_json::to_vec(frame).unwrap();
    stream.write_u32(bytes.len() as u32).await.unwrap();
    stream.write_all(&bytes).await.unwrap();
}

fn seed_queued_tick(checkpoint: &mut Value) -> Uuid {
    let runtime = &mut checkpoint["Prepared"]["runtime"];
    let (subscription, watch) = runtime["tick_deliveries"]["subscriptions"]
        .as_object()
        .unwrap()
        .iter()
        .next()
        .map(|(id, watch)| (id.clone(), watch.clone()))
        .unwrap();
    let chat = &mut runtime["chats"][0];
    let loop_id: Uuid = serde_json::from_value(watch["loop_id"].clone()).unwrap();
    let snapshot = &mut chat["snapshot"];
    let chat_id: Uuid = serde_json::from_value(snapshot["history"]["id"].clone()).unwrap();
    let run_id = Uuid::new_v4();
    let queued_sequence = snapshot["last_queue_sequence"].as_u64().unwrap() + 1;
    let pending = PendingInput {
        receipt: Receipt {
            chat_id: chat_id.into(),
            loop_id: loop_id.into(),
            run_id: run_id.into(),
        },
        input: QUEUED_MARKER.into(),
        contexts: Vec::new(),
        agent_message: None,
        directed_human: None,
        denial_reference: None,
        trigger_source: Some(
            serde_json::from_value::<TriggerSource>(json!({
                "kind": "tick_update",
                "items": [{
                    "subscription": subscription, "watch": watch["watch"],
                    "instance": watch["instance"], "sequence": 0, "updates": 1,
                }],
            }))
            .unwrap(),
        ),
        armed: true,
        queued_sequence,
        task_continuation: false,
        is_skill_invocation: false,
        attached_file_reads: Vec::new(),
    };
    snapshot["pending_inputs"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::to_value(pending).unwrap());
    snapshot["input_authorizations"]
        .as_array_mut()
        .unwrap()
        .push(json!([run_id, false, []]));
    snapshot["last_queue_sequence"] = json!(queued_sequence);
    chat["bookkeeping"]["run_loops"][run_id.to_string()] = json!(loop_id);
    run_id
}

#[tokio::test]
async fn queued_tick_revalidates_against_the_real_successor_worker_during_release() {
    let workdir = tempfile::tempdir().unwrap();
    let (url, mut requests) = tickloop_harness::test_support::model_http_support::serve_responses(vec![
        (200, response(None, Some(("open", "TickManage", json!({"action":"open","command":support::hosted::counter_program(),"cwd":workdir.path().canonicalize().unwrap(),"protocol":true,"placement":{"type":"hidden"}}))))),
        (200, response(Some("counter opened"), None)),
        (200, response(Some("queued tick consumed"), None)),
    ]).await;
    let scenario = Scenario::new(&url, workdir).await;
    scenario.submit("Open the counter").await;
    wait_for_text(&scenario.client, scenario.execution, "counter opened").await;
    requests.recv().await.unwrap();
    let (_, after_open) = requests.recv().await.unwrap();
    let tool_results = after_open["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "tool")
        .map(|message| message["content"].clone())
        .collect::<Vec<_>>();
    let listed = scenario
        .client
        .execute(command(
            scenario.client.daemon_epoch(),
            AgentCommand::ListHostedTicks {
                execution: scenario.execution,
            },
        ))
        .await
        .unwrap()
        .into_result()
        .unwrap();
    let CommandResult::HostedTicks(ticks) = listed else {
        panic!("expected hosted counter");
    };
    let instance = ticks
        .first()
        .unwrap_or_else(|| panic!("counter did not open; tool results: {tool_results:?}"))
        .instance
        .clone();
    scenario
        .client
        .execute(command(
            scenario.client.daemon_epoch(),
            AgentCommand::ManageLoop {
                execution: scenario.execution,
                action: LoopAction::TickManage {
                    instance: instance.clone(),
                },
            },
        ))
        .await
        .unwrap()
        .into_result()
        .unwrap();
    tokio::time::timeout(WAIT, async {
        loop {
            let status = scenario.client.observe(ObservationRequest::Agent { execution: scenario.execution, detail: AgentDetailQuery::Subscriptions, after: None }).await.unwrap();
            if matches!(status.initial, ObservationValue::Subscriptions(ref subscriptions) if subscriptions.iter().any(|subscription| subscription.link == tickloop_harness::protocol::TickLinkState::Live)) { break; }
            tokio::time::sleep(POLL).await;
        }
    }).await.unwrap();
    let published = scenario.worker_endpoint().with_extension("upgrade");
    let (listener, original) = replace_socket(&published);
    let forwarding = original.clone();
    let (seeded, received) = oneshot::channel();
    let task = tokio::spawn(async move {
        let mut seeded = Some(seeded);
        while let Ok((mut source, _)) = listener.accept().await {
            let mut target = UnixStream::connect(&forwarding).await.unwrap();
            let frame = read_checkpoint_frame(&mut source).await;
            write_checkpoint_frame(&mut target, &frame).await;
            let mut reply = read_checkpoint_frame(&mut target).await;
            if reply["Prepared"].is_object() && seeded.is_some() {
                let run = seed_queued_tick(&mut reply);
                seeded.take().unwrap().send(run).unwrap();
            }
            write_checkpoint_frame(&mut source, &reply).await;
        }
    });
    let proxy = SocketProxy {
        task,
        published,
        original,
    };
    scenario.activate(scenario.new_executable.clone()).await;
    let queued_run = tokio::time::timeout(WAIT, received).await.unwrap().unwrap();
    let replacement = scenario.replacement().await;
    let (_, request) = tokio::time::timeout(WAIT, requests.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(request.to_string().contains(QUEUED_MARKER));
    wait_for_text(&replacement, scenario.execution, "queued tick consumed").await;
    let current = snapshot(&replacement, scenario.execution).await;
    assert!(
        current
            .pending_inputs
            .iter()
            .all(|input| input.receipt.run_id != queued_run)
    );
    assert_eq!(
        current
            .events
            .iter()
            .filter(|event| event.run_id == queued_run
                && matches!(event.kind, tickloop_harness::protocol::EventKind::Started))
            .count(),
        1
    );
    let listed = replacement
        .execute(command(
            replacement.daemon_epoch(),
            AgentCommand::ListHostedTicks {
                execution: scenario.execution,
            },
        ))
        .await
        .unwrap()
        .into_result()
        .unwrap();
    assert!(
        matches!(listed, CommandResult::HostedTicks(ref ticks) if ticks.iter().any(|tick| tick.instance == instance))
    );
    replacement.shutdown_daemon().await.unwrap();
    drop(proxy);
}

#[tokio::test]
async fn live_worker_retries_an_unstarted_tool_request_while_its_daemon_changes() {
    use tickhost_connect::{
        host_error::HostError,
        hosted::{BUSINESS_FRAME_BYTES, error, read_frame, write_frame},
    };
    let (url, mut requests, first_reply) = tickloop_harness::test_support::model_http_support::serve_responses_with_gated_response(vec![
        (200, response(None, Some(("once", "Bash", json!({"command":"printf exactly-once >> tool-effects","description":"Record one effect"}))))),
        (200, response(Some("tool-control completed"), None)),
    ], 0).await;
    let scenario = Scenario::new(&url, tempfile::tempdir().unwrap()).await;
    scenario.submit("Run the command once").await;
    tokio::time::timeout(WAIT, requests.recv())
        .await
        .unwrap()
        .unwrap();
    let ready = scenario.workdir.path().join("candidate-ready");
    let release = scenario.workdir.path().join("release-daemon");
    let wrapper = scenario.new_executable.with_file_name("gated-tickloop");
    std::fs::write(&wrapper, format!("#!/bin/sh\nif [ \"$1\" = daemon ] && [ \"$2\" = run ]; then\n  : > '{}'\n  while [ ! -f '{}' ]; do /bin/sleep 0.01; done\nfi\nexec '{}' \"$@\"\n", ready.display(), release.display(), scenario.new_executable.display())).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let published = scenario.runtime.path().join("worker-control.sock");
    let (listener, original) = replace_socket(&published);
    let forwarding = original.clone();
    let restored_control = published.clone();
    let daemon_endpoint = scenario.runtime.path().join("daemon.sock");
    let old_daemon = scenario.client.server_process_id();
    let (refused, refusal) = oneshot::channel();
    let (retry_captured, retry_capture) = oneshot::channel();
    let (forwarded, forwarding_done) = oneshot::channel();
    let injected = Arc::new(AtomicBool::new(false));
    let refused = Arc::new(Mutex::new(Some(refused)));
    let retry_captured = Arc::new(Mutex::new(Some(retry_captured)));
    let forwarded = Arc::new(Mutex::new(Some(forwarded)));
    let original_request = Arc::new(Mutex::new(None));
    let task = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                completed = connections.join_next(), if !connections.is_empty() => {
                    completed.unwrap().unwrap();
                }
                accepted = listener.accept() => {
                    let (source, _) = accepted.unwrap();
                    let injected = injected.clone();
                    let refused = refused.clone();
                    let retry_captured = retry_captured.clone();
                    let forwarded = forwarded.clone();
                    let original_request = original_request.clone();
                    let forwarding = forwarding.clone();
                    let restored_control = restored_control.clone();
                    let daemon_endpoint = daemon_endpoint.clone();
                    connections.spawn(async move {
                        let (reader, mut writer) = source.into_split();
                        let frame = read_frame(&mut BufReader::new(reader), BUSINESS_FRAME_BYTES).await.unwrap().unwrap();
                        let is_spawn = frame["envelope"]["request"]["method"] == "allow_shell_spawn";
                        if is_spawn && !injected.swap(true, Ordering::SeqCst) {
                            *original_request.lock().unwrap() = Some(frame.clone());
                            write_frame(&mut writer, &error(&frame["id"], &HostError::before("DaemonTransferred", "test gate before dispatch")), BUSINESS_FRAME_BYTES).await.unwrap();
                            refused.lock().unwrap().take().unwrap().send(()).unwrap();
                            return;
                        }
                        if is_spawn {
                            assert!(original_request.lock().unwrap().as_ref() == Some(&frame), "retry changed its authorized envelope");
                            retry_captured.lock().unwrap().take().unwrap().send(()).unwrap();
                            tokio::time::timeout(WAIT, async {
                                loop {
                                    if let Ok(client) = LocalAgentClient::connect(&daemon_endpoint).await
                                        && client.server_process_id() != old_daemon { break; }
                                    tokio::time::sleep(POLL).await;
                                }
                            }).await.unwrap();
                        }
                        let target = UnixStream::connect(if is_spawn { &restored_control } else { &forwarding }).await.unwrap();
                        let (target_reader, mut target_writer) = target.into_split();
                        write_frame(&mut target_writer, &frame, BUSINESS_FRAME_BYTES).await.unwrap();
                        let reply = read_frame(&mut BufReader::new(target_reader), BUSINESS_FRAME_BYTES).await.unwrap().unwrap();
                        write_frame(&mut writer, &reply, BUSINESS_FRAME_BYTES).await.unwrap();
                        if is_spawn { forwarded.lock().unwrap().take().unwrap().send(()).unwrap(); }
                    });
                }
            }
        }
    });
    let proxy = SocketProxy {
        task,
        published,
        original,
    };
    scenario.activate(wrapper).await;
    tokio::time::timeout(WAIT, async {
        while !ready.is_file() {
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .unwrap();
    first_reply.send(()).unwrap();
    tokio::time::timeout(WAIT, refusal).await.unwrap().unwrap();
    assert!(!scenario.workdir.path().join("tool-effects").exists());
    tokio::time::timeout(WAIT, retry_capture)
        .await
        .unwrap()
        .unwrap();
    // Keep the accepted retry parked while restoring the inode inherited by the successor.
    std::fs::remove_file(&proxy.published).unwrap();
    std::fs::rename(&proxy.original, &proxy.published).unwrap();
    std::fs::write(release, "continue").unwrap();
    tokio::time::timeout(WAIT, forwarding_done)
        .await
        .unwrap()
        .unwrap();
    let replacement = scenario.replacement().await;
    wait_for_text(&replacement, scenario.execution, "tool-control completed").await;
    assert_eq!(
        std::fs::read_to_string(scenario.workdir.path().join("tool-effects")).unwrap(),
        "exactly-once"
    );
    let (_, request) = tokio::time::timeout(WAIT, requests.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(request.to_string().contains("once"));
    replacement.shutdown_daemon().await.unwrap();
    drop(proxy);
}
