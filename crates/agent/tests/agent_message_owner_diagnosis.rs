mod model_http_support;
mod support;

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use futures_util::FutureExt;
use serde_json::{Value, json};
use tickloop_agent::{
    agent_access::*,
    model::{Content, ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest},
    runtime::Agent,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, UnixListener, UnixStream},
    sync::{Mutex, Notify, mpsc::UnboundedSender},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Default)]
struct MessageBurstModel {
    parent_ready: Notify,
    send_messages: Notify,
    children_ready: AtomicUsize,
    emit_deltas: Notify,
}

fn chat_response(content: Value, has_tools: bool) -> String {
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"id":"schema-fixture","choices":[{"index":0,"delta":content,"finish_reason":null}]}),
        json!({"id":"schema-fixture","choices":[{"index":0,"delta":{},"finish_reason":if has_tools {"tool_calls"} else {"stop"}}]}),
    )
}

async fn write_json_frame(stream: &mut UnixStream, value: &Value) {
    let bytes = serde_json::to_vec(value).unwrap();
    stream.write_u32(bytes.len() as u32).await.unwrap();
    stream.write_all(&bytes).await.unwrap();
}

async fn read_json_frame(stream: &mut UnixStream) -> Option<Value> {
    let length = match stream.read_u32().await {
        Ok(length) => length as usize,
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return None,
        Err(error) => panic!("{error}"),
    };
    assert!(length <= MAX_FRAME_BYTES);
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).await.unwrap();
    Some(serde_json::from_slice(&bytes).unwrap())
}

async fn legacy_connection(endpoint: &std::path::Path) -> (UnixStream, Value) {
    let mut stream = UnixStream::connect(endpoint).await.unwrap();
    write_json_frame(&mut stream, &json!({"Hello":{"major":1,"minor":1}})).await;
    let hello = read_json_frame(&mut stream).await.unwrap()["Hello"].clone();
    assert_eq!(hello["major"], 1);
    (stream, hello)
}

async fn legacy_request(stream: &mut UnixStream, kind: &str, mut body: Value) -> Value {
    let request_id = serde_json::to_value(Uuid::new_v4()).unwrap();
    body["request_id"] = request_id.clone();
    write_json_frame(
        stream,
        &Value::Object(serde_json::Map::from_iter([(kind.to_owned(), body)])),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let response = read_json_frame(stream).await.unwrap();
            for body in response.as_object().unwrap().values() {
                if body["request_id"] == request_id {
                    return body.clone();
                }
            }
        }
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated 334894e owner executable; never uses the default daemon"]
async fn legacy_owner_and_active_worker_survive_rejected_new_clients_and_workers() {
    let directory = tempfile::tempdir_in("/private/tmp").unwrap();
    let directory_path = directory.path().canonicalize().unwrap();
    let executable = directory_path.join("tickloop");
    std::fs::copy(
        std::env::var_os("TICKLOOP_DIAG_OLD_OWNER").unwrap(),
        &executable,
    )
    .unwrap();
    let runtime_directory = directory_path.join("runtime");
    let startup = tickloop_agent::agent_daemon::connect_or_start_at(
        runtime_directory.clone(),
        executable.clone(),
    )
    .await;
    assert!(startup.is_err(), "the new client must refuse the old owner");
    assert!(startup.err().unwrap().contains("left running"));
    let (mut owner, original_hello) =
        legacy_connection(&runtime_directory.join("owner.sock")).await;
    let epoch: Uuid = serde_json::from_value(original_hello["owner_epoch"].clone()).unwrap();
    let reply = chat_response(
        json!({"role":"assistant","content":"Original old worker completed normally."}),
        false,
    );
    let (base_url, mut requests, release) =
        model_http_support::serve_responses_with_gated_response(
            vec![(200, reply), (500, "unexpected extra model request".into())],
            0,
        )
        .await;
    let verification = async {
        for name in ["old-config", "new-config"] {
            std::fs::create_dir(directory_path.join(name)).unwrap();
            std::fs::set_permissions(
                directory_path.join(name),
                std::os::unix::fs::PermissionsExt::from_mode(0o700),
            )
            .unwrap();
        }
        let mut model = support::config();
        model.protocol = tickloop_agent::model::Protocol::ChatCompletions;
        model.base_url = base_url.trim_end_matches("/anthropic").into();
        model.api_key_env = "TICKLOOP_DIAG_FIXTURE_KEY".into();
        let mut launch = LaunchSpec::embedded(directory_path.clone());
        launch.config_root = Some(directory_path.join("old-config"));
        launch.model = Some(model);
        launch.environment = [
            ("HOME", directory_path.to_str().unwrap()),
            ("TICKLOOP_DIAG_FIXTURE_KEY", "fixture-only"),
        ]
        .into_iter()
        .map(|(name, value)| EnvironmentValue {
            name: name.as_bytes().to_vec(),
            value: value.as_bytes().to_vec(),
        })
        .collect();
        let initial = Some(UserInput::Text {
            text: "Complete the bounded fixture reply.".into(),
            contexts: vec![],
            paths: vec![],
        });
        let response = legacy_request(&mut owner,"Execute",json!({"command":CommandEnvelope{owner_epoch:epoch,command_id:Uuid::new_v4(),command:AgentCommand::Create{launch:launch.clone(),initial:initial.clone(),origin:None}}})).await;
        let receipt: CommandReceipt = serde_json::from_value(response["receipt"].clone()).unwrap();
        let CommandResult::Created { execution, .. } = receipt.into_result().unwrap() else {
            panic!("old worker created");
        };
        tokio::time::timeout(Duration::from_secs(5), requests.recv())
            .await
            .unwrap()
            .unwrap();
        let old_worker_socket = std::fs::read_dir(&runtime_directory)
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| entry.file_name().to_string_lossy().starts_with("w-"))
            .unwrap()
            .path();
        let (_, worker_hello) = legacy_connection(&old_worker_socket).await;
        let rejected = LocalAgentClient::connect(runtime_directory.join("owner.sock")).await;
        assert!(matches!(rejected,Err(ref error) if error.code==AccessErrorCode::VersionMismatch));
        let (_, still_hello) = legacy_connection(&runtime_directory.join("owner.sock")).await;
        assert_eq!(still_hello["process_id"], original_hello["process_id"]);
        assert_eq!(still_hello["owner_epoch"], original_hello["owner_epoch"]);
        let replacement = directory_path.join("replacement");
        std::fs::copy(env!("CARGO_BIN_EXE_tickloop"), &replacement).unwrap();
        std::fs::rename(replacement, &executable).unwrap();
        launch.config_root = Some(directory_path.join("new-config"));
        let command = CommandEnvelope {
            owner_epoch: epoch,
            command_id: Uuid::new_v4(),
            command: AgentCommand::Create {
                launch,
                initial,
                origin: None,
            },
        };
        let first = legacy_request(&mut owner, "Execute", json!({"command":command})).await;
        let rejected: CommandReceipt = serde_json::from_value(first["receipt"].clone()).unwrap();
        assert!(matches!(rejected.state, ReceiptState::Failed));
        assert!(rejected.target.is_none());
        assert!(
            matches!(rejected.error,Some(ref error) if error.code==AccessErrorCode::VersionMismatch)
        );
        let repeated = legacy_request(&mut owner, "Execute", json!({"command":command})).await;
        assert_eq!(first["receipt"], repeated["receipt"]);
        assert!(matches!(
            requests.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        let response =
            legacy_request(&mut owner, "Observe", json!({"query":overview_query()})).await;
        let ObservationValue::Overview(rows): ObservationValue =
            serde_json::from_value(response["initial"].clone()).unwrap()
        else {
            panic!("old directory");
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].execution, execution);
        assert_eq!(rows[0].availability, ExecutionAvailability::Live);
        assert_eq!(rows[0].state, tickloop_agent::runtime::LoopState::Running);
        let (_, still_worker) = legacy_connection(&old_worker_socket).await;
        assert_eq!(still_worker["process_id"], worker_hello["process_id"]);
        assert_eq!(still_worker["owner_epoch"], worker_hello["owner_epoch"]);
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let response =
                    legacy_request(&mut owner, "Observe", json!({"query":overview_query()})).await;
                let ObservationValue::Overview(rows): ObservationValue =
                    serde_json::from_value(response["initial"].clone()).unwrap()
                else {
                    panic!("old directory");
                };
                if rows[0].state == tickloop_agent::runtime::LoopState::Completed {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert!(matches!(
            requests.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        eprintln!(
            "[schema-guard] legacy owner/worker same PID and epoch; active old execution completed; new worker Create rejected before target/model; repeated CommandId same Failed receipt; HTTP requests=1 original, 0 new"
        );
    };
    let result = std::panic::AssertUnwindSafe(verification)
        .catch_unwind()
        .await;
    legacy_request(&mut owner, "ShutdownOwner", json!({"owner_epoch":epoch})).await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn legacy_wire_schema_is_rejected_before_pipelined_create_can_execute() {
    let directory = tempfile::tempdir_in("/private/tmp").unwrap();
    let socket = directory.path().join("worker.sock");
    let model = Arc::new(support::ScriptedModel::new(vec![support::text(
        "fixture reply",
    )]));
    let runtime = Agent::new_embedded(support::config(), model.clone());
    let epoch = Uuid::new_v4();
    let access = Arc::new(EmbeddedAgentAccess::for_worker(runtime.clone(), epoch));
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(serve_worker_listener(listener, access, epoch));
    let verification = async {
        let mut client = UnixStream::connect(socket).await.unwrap();
        let command = CommandEnvelope {
            owner_epoch: epoch,
            command_id: Uuid::new_v4(),
            command: AgentCommand::Create {
                launch: LaunchSpec::embedded(directory.path().into()),
                initial: Some(UserInput::Text {
                    text: "Produce the fixture reply.".into(),
                    contexts: vec![],
                    paths: vec![],
                }),
                origin: None,
            },
        };
        let mut pipelined = Vec::new();
        for value in [
            json!({"Hello":{"major":1,"minor":1}}),
            json!({"Execute":{"request_id":Uuid::new_v4(),"command":command}}),
        ] {
            let bytes = serde_json::to_vec(&value).unwrap();
            pipelined.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
            pipelined.extend_from_slice(&bytes);
        }
        client.write_all(&pipelined).await.unwrap();
        let response = read_json_frame(&mut client).await.unwrap();
        assert_eq!(
            response["Error"]["error"]["code"],
            serde_json::to_value(AccessErrorCode::VersionMismatch).unwrap()
        );
        assert!(read_json_frame(&mut client).await.is_none());
        assert!(runtime.list_chats().await.is_empty());
        assert!(model.requests.lock().unwrap().is_empty());
    };
    let result = std::panic::AssertUnwindSafe(verification)
        .catch_unwind()
        .await;
    runtime.shutdown().await;
    server.abort();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated 334894e worker executable; never uses the default daemon"]
async fn new_owner_rejects_an_old_worker_immediately_and_keeps_its_active_execution() {
    let directory = tempfile::tempdir_in("/private/tmp").unwrap();
    let directory_path = directory.path().canonicalize().unwrap();
    let executable = directory_path.join("tickloop");
    std::fs::copy(env!("CARGO_BIN_EXE_tickloop"), &executable).unwrap();
    let runtime_directory = directory_path.join("runtime");
    let owner = tickloop_agent::agent_daemon::connect_or_start_at(
        runtime_directory.clone(),
        executable.clone(),
    )
    .await
    .unwrap();
    let original_hello = owner.protocol().clone();
    let reply = chat_response(
        json!({"role":"assistant","content":"Original new worker completed normally."}),
        false,
    );
    let (base_url, mut requests, release) =
        model_http_support::serve_responses_with_gated_response(
            vec![(200, reply), (500, "unexpected extra model request".into())],
            0,
        )
        .await;
    let verification = async {
        for name in ["original-config", "rejected-config"] {
            std::fs::create_dir(directory_path.join(name)).unwrap();
            std::fs::set_permissions(
                directory_path.join(name),
                std::os::unix::fs::PermissionsExt::from_mode(0o700),
            )
            .unwrap();
        }
        let mut model = support::config();
        model.protocol = tickloop_agent::model::Protocol::ChatCompletions;
        model.base_url = base_url.trim_end_matches("/anthropic").into();
        model.api_key_env = "TICKLOOP_DIAG_FIXTURE_KEY".into();
        let mut launch = LaunchSpec::embedded(directory_path.clone());
        launch.config_root = Some(directory_path.join("original-config"));
        launch.model = Some(model);
        launch.environment = [
            ("HOME", directory_path.to_str().unwrap()),
            ("TICKLOOP_DIAG_FIXTURE_KEY", "fixture-only"),
        ]
        .into_iter()
        .map(|(name, value)| EnvironmentValue {
            name: name.as_bytes().to_vec(),
            value: value.as_bytes().to_vec(),
        })
        .collect();
        let initial = Some(UserInput::Text {
            text: "Complete the bounded fixture reply.".into(),
            contexts: vec![],
            paths: vec![],
        });
        let created = owner
            .execute(CommandEnvelope {
                owner_epoch: owner.owner_epoch(),
                command_id: Uuid::new_v4(),
                command: AgentCommand::Create {
                    launch: launch.clone(),
                    initial: initial.clone(),
                    origin: None,
                },
            })
            .await
            .unwrap()
            .into_result()
            .unwrap();
        let CommandResult::Created { execution, .. } = created else {
            panic!("original worker");
        };
        tokio::time::timeout(Duration::from_secs(5), requests.recv())
            .await
            .unwrap()
            .unwrap();
        let original_socket = std::fs::read_dir(&runtime_directory)
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| entry.file_name().to_string_lossy().starts_with("w-"))
            .unwrap()
            .path();
        let original_worker = LocalAgentClient::connect(&original_socket)
            .await
            .unwrap()
            .protocol()
            .clone();
        let replacement = directory_path.join("replacement");
        std::fs::copy(
            std::env::var_os("TICKLOOP_DIAG_OLD_OWNER").unwrap(),
            &replacement,
        )
        .unwrap();
        std::fs::rename(replacement, &executable).unwrap();
        launch.config_root = Some(directory_path.join("rejected-config"));
        let command = CommandEnvelope {
            owner_epoch: owner.owner_epoch(),
            command_id: Uuid::new_v4(),
            command: AgentCommand::Create {
                launch,
                initial,
                origin: None,
            },
        };
        let started = std::time::Instant::now();
        let rejected = owner.execute(command.clone()).await.unwrap();
        let elapsed = started.elapsed();
        assert!(
            matches!(rejected.error,Some(ref error) if error.code==AccessErrorCode::VersionMismatch)
        );
        assert!(matches!(rejected.state, ReceiptState::Failed));
        assert!(rejected.target.is_none());
        let repeated = owner.execute(command).await.unwrap();
        assert_eq!(
            serde_json::to_value(rejected).unwrap(),
            serde_json::to_value(repeated).unwrap()
        );
        assert!(matches!(
            requests.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        let ObservationValue::Overview(rows) =
            owner.observe(overview_query()).await.unwrap().initial
        else {
            panic!("directory");
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].execution, execution);
        assert_eq!(rows[0].availability, ExecutionAvailability::Live);
        assert_eq!(rows[0].state, tickloop_agent::runtime::LoopState::Running);
        let same_owner = LocalAgentClient::connect(runtime_directory.join("owner.sock"))
            .await
            .unwrap();
        assert_eq!(same_owner.protocol().process_id, original_hello.process_id);
        assert_eq!(same_owner.owner_epoch(), original_hello.owner_epoch);
        let same_worker = LocalAgentClient::connect(&original_socket).await.unwrap();
        assert_eq!(
            same_worker.protocol().process_id,
            original_worker.process_id
        );
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let ObservationValue::Overview(rows) =
                    owner.observe(overview_query()).await.unwrap().initial
                else {
                    panic!("directory");
                };
                if rows[0].state == tickloop_agent::runtime::LoopState::Completed {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert!(matches!(
            requests.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        eprintln!(
            "[schema-guard] new owner rejects old worker in {elapsed:?}; original owner/worker PID+epoch unchanged and original execution Completed; 0 new HTTP, same failed CommandId receipt"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "a deterministic VersionMismatch must not consume the worker startup retry deadline"
        );
    };
    let result = std::panic::AssertUnwindSafe(verification)
        .catch_unwind()
        .await;
    owner.shutdown_owner().await.unwrap();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

async fn schema_model() -> (
    String,
    Arc<Notify>,
    Arc<Notify>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let send_ready = Arc::new(Notify::new());
    let release_send = Arc::new(Notify::new());
    let ready = send_ready.clone();
    let release = release_send.clone();
    let server = tokio::spawn(async move {
        let mut root_calls = 0;
        let mut child_calls = std::collections::HashMap::<String, usize>::new();
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0; 4096];
            let (start, length) = loop {
                let count = socket.read(&mut buffer).await.unwrap();
                assert_ne!(count, 0);
                bytes.extend_from_slice(&buffer[..count]);
                if let Some(end) = bytes.windows(4).position(|value| value == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..end]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap();
                    break (end + 4, length);
                }
            };
            while bytes.len() < start + length {
                let count = socket.read(&mut buffer).await.unwrap();
                assert_ne!(count, 0);
                bytes.extend_from_slice(&buffer[..count]);
            }
            let request: Value = serde_json::from_slice(&bytes[start..start + length]).unwrap();
            let host = request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find_map(|message| {
                    message["content"]
                        .as_str()?
                        .rsplit_once("Current host state: ")
                        .and_then(|(_, value)| serde_json::from_str::<Value>(value).ok())
                })
                .unwrap();
            let calls = if let Some(task) = host["delegation"]["task"].as_str() {
                let calls = child_calls.entry(task.to_owned()).or_default();
                *calls += 1;
                if *calls == 2 {
                    vec![
                        json!({"index":0,"id":format!("ready-{task}"),"type":"function","function":{"name":"SendMessage","arguments":json!({"to":"parent","message":format!("READY {task}")}).to_string()}}),
                    ]
                } else {
                    vec![]
                }
            } else {
                root_calls += 1;
                if root_calls == 1 {
                    ["A", "B", "C"].into_iter().enumerate().map(|(index, task)| json!({"index":index,"id":format!("delegate-{task}"),"type":"function","function":{"name":"Subagent","arguments":json!({"task":format!("Child {task}")}).to_string()}})).collect()
                } else if root_calls == 2 {
                    let calls = ["A", "B", "C"].into_iter().enumerate().map(|(index, task)| {
                        let id = format!("delegate-{task}");
                        let result: Value = serde_json::from_str(request["messages"].as_array().unwrap().iter().find(|message| message["tool_call_id"] == id).unwrap()["content"].as_str().unwrap()).unwrap();
                        json!({"index":index,"id":format!("send-{task}"),"type":"function","function":{"name":"SendMessage","arguments":json!({"to":result["child_chat_id"],"message":"READY follow-up"}).to_string()}})
                    }).collect();
                    ready.notify_one();
                    release.notified().await;
                    calls
                } else {
                    vec![]
                }
            };
            let has_tools = !calls.is_empty();
            let delta = if has_tools {
                json!({"role":"assistant","tool_calls":calls})
            } else {
                json!({"role":"assistant","content":"Schema fixture completed."})
            };
            let body = chat_response(delta, has_tools);
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
    });
    (
        format!("http://{address}"),
        send_ready,
        release_send,
        server,
    )
}

async fn legacy_snapshot_decode(decoder: &std::ffi::OsStr, value: &ObservationValue) -> String {
    let mut process = tokio::process::Command::new(decoder)
        .env_clear()
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    process
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(value).unwrap())
        .await
        .unwrap();
    String::from_utf8(process.wait_with_output().await.unwrap().stdout).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "historical RED requires the pre-fix major-1 producer/client plus isolated 334894e owner; current major 2 rejects setup"]
async fn legacy_owner_new_worker_messages_do_not_fence_a_still_live_family() {
    let legacy = PathBuf::from(std::env::var_os("TICKLOOP_DIAG_OLD_OWNER").unwrap());
    let owned_directory = tempfile::tempdir_in("/private/tmp").unwrap();
    let directory = owned_directory.path().canonicalize().unwrap();
    let executable = directory.as_path().join("tickloop");
    std::fs::create_dir(directory.as_path().join("config")).unwrap();
    std::fs::set_permissions(
        directory.as_path().join("config"),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    std::fs::copy(legacy, &executable).unwrap();
    let owner = tickloop_agent::agent_daemon::connect_or_start_at(
        directory.as_path().join("runtime"),
        executable.clone(),
    )
    .await
    .unwrap();
    let replacement = directory.as_path().join("replacement");
    std::fs::copy(env!("CARGO_BIN_EXE_tickloop"), &replacement).unwrap();
    std::fs::rename(replacement, executable).unwrap();
    let (base_url, send_ready, release_send, model_server) = schema_model().await;
    let verification = async {
        let mut model = support::config();
        model.protocol = tickloop_agent::model::Protocol::ChatCompletions;
        model.base_url = base_url;
        model.api_key_env = "TICKLOOP_DIAG_FIXTURE_KEY".into();
        let mut launch = LaunchSpec::embedded(directory.as_path().into());
        launch.config_root = Some(directory.as_path().join("config"));
        launch.model = Some(model);
        launch.environment = [
            ("HOME", directory.as_path().to_str().unwrap()),
            ("TICKLOOP_DIAG_FIXTURE_KEY", "fixture-only"),
        ]
        .into_iter()
        .map(|(name, value)| EnvironmentValue {
            name: name.as_bytes().to_vec(),
            value: value.as_bytes().to_vec(),
        })
        .collect();
        let created = owner
            .execute(CommandEnvelope {
                owner_epoch: owner.owner_epoch(),
                command_id: Uuid::new_v4(),
                command: AgentCommand::Create {
                    launch,
                    initial: None,
                    origin: None,
                },
            })
            .await
            .unwrap()
            .into_result()
            .unwrap();
        let CommandResult::Created {
            execution: parent, ..
        } = created
        else {
            panic!("create");
        };
        let worker_socket = std::fs::read_dir(directory.as_path().join("runtime"))
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| entry.file_name().to_string_lossy().starts_with("w-"))
            .unwrap()
            .path();
        let worker = LocalAgentClient::connect(worker_socket).await.unwrap();
        assert_ne!(owner.protocol().process_id, worker.protocol().process_id);
        assert_eq!(owner.owner_epoch(), worker.owner_epoch());
        let baseline = owner.observe(overview_query()).await.unwrap();
        assert!(
            matches!(baseline.initial, ObservationValue::Overview(ref rows) if rows.len()==1 && rows[0].availability==ExecutionAvailability::Live)
        );
        owner
            .execute(CommandEnvelope {
                owner_epoch: owner.owner_epoch(),
                command_id: Uuid::new_v4(),
                command: AgentCommand::Submit {
                    execution: parent,
                    target_loop: None,
                    input: UserInput::Text {
                        text: "Delegate three children, let them send READY, then complete.".into(),
                        contexts: vec![],
                        paths: vec![],
                    },
                    origin: None,
                },
            })
            .await
            .unwrap()
            .into_result()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), send_ready.notified())
            .await
            .unwrap();
        let ObservationValue::Overview(registered) =
            owner.observe(overview_query()).await.unwrap().initial
        else {
            panic!("registered directory");
        };
        assert_eq!(registered.len(), 4);
        assert!(
            registered
                .iter()
                .all(|row| row.availability == ExecutionAvailability::Live)
        );
        let mut active_feeds = Vec::new();
        for row in &registered {
            active_feeds.push(
                owner
                    .observe(ObservationRequest::Agent {
                        execution: row.execution,
                        detail: AgentDetailQuery::Snapshot,
                        after: None,
                    })
                    .await
                    .unwrap(),
            );
        }
        let polling = owner.clone();
        let concurrent_directory = tokio::spawn(async move {
            for _ in 0..200 {
                let _ = polling.observe(overview_query()).await;
                tokio::task::yield_now().await;
            }
        });
        release_send.notify_one();
        let rows = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let ObservationValue::Overview(rows) =
                    worker.observe(overview_query()).await.unwrap().initial
                else {
                    panic!("worker directory");
                };
                if rows.len() == 4
                    && rows
                        .iter()
                        .all(|row| row.state == tickloop_agent::runtime::LoopState::Completed)
                {
                    break rows;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        concurrent_directory.await.unwrap();
        let actual = worker
            .observe(ObservationRequest::Agent {
                execution: parent,
                detail: AgentDetailQuery::Snapshot,
                after: None,
            })
            .await
            .unwrap();
        let ObservationValue::Agent(ref snapshot) = actual.initial else {
            panic!("worker snapshot");
        };
        assert!(snapshot.events.iter().any(|event| matches!(
            event.kind,
            tickloop_agent::runtime::EventKind::AgentMessageQueued { .. }
        )));
        let accepted = snapshot.events.iter().filter(|event| matches!(&event.kind,tickloop_agent::runtime::EventKind::ToolCompleted{result,..} if !result.is_error && result.content.contains("accepted"))).count();
        eprintln!(
            "[schema-diagnosis] worker members={} completed={} accepted_tools={accepted}; both advertised major={} minor={}",
            rows.len(),
            rows.iter()
                .filter(|row| row.state == tickloop_agent::runtime::LoopState::Completed)
                .count(),
            owner.protocol().major,
            owner.protocol().minor
        );
        if let Some(decoder) = std::env::var_os("TICKLOOP_DIAG_OLD_DECODER") {
            let error = legacy_snapshot_decode(&decoder, &actual.initial).await;
            eprintln!("[schema-diagnosis] legacy actual ObservationValue decoder: {error}");
            assert!(error.contains("unknown variant `AgentMessageQueued`"));
            let mut control = actual.initial.clone();
            let ObservationValue::Agent(ref mut snapshot) = control else {
                panic!("snapshot");
            };
            snapshot.events.retain(|event| {
                !matches!(
                    event.kind,
                    tickloop_agent::runtime::EventKind::AgentMessageQueued { .. }
                )
            });
            let decoded = legacy_snapshot_decode(&decoder, &control).await;
            eprintln!(
                "[schema-diagnosis] identical snapshot with only new EventKind removed: {decoded}"
            );
            assert_eq!(decoded.trim(), "OK");
        }
        let through_owner = owner
            .observe(ObservationRequest::Agent {
                execution: parent,
                detail: AgentDetailQuery::Snapshot,
                after: None,
            })
            .await;
        eprintln!(
            "[schema-diagnosis] legacy owner snapshot result={}",
            through_owner
                .as_ref()
                .err()
                .map_or_else(|| "OK".into(), ToString::to_string)
        );
        let ObservationValue::Overview(owner_rows) =
            owner.observe(overview_query()).await.unwrap().initial
        else {
            panic!("owner directory");
        };
        let ObservationValue::Overview(worker_rows) =
            worker.observe(overview_query()).await.unwrap().initial
        else {
            panic!("worker directory");
        };
        assert_eq!(worker_rows.len(), 4);
        assert!(
            worker_rows
                .iter()
                .all(|row| row.availability == ExecutionAvailability::Live)
        );
        assert_eq!(owner_rows.len(), 4);
        assert_eq!(
            owner_rows
                .iter()
                .map(|row| row.execution)
                .collect::<Vec<_>>(),
            worker_rows
                .iter()
                .map(|row| row.execution)
                .collect::<Vec<_>>()
        );
        eprintln!(
            "[schema-diagnosis] same live worker: owner Lost={} worker Live={}",
            owner_rows
                .iter()
                .filter(|row| row.availability == ExecutionAvailability::Lost)
                .count(),
            worker_rows.len()
        );
        drop(active_feeds);
        assert!(
            owner_rows
                .iter()
                .all(|row| row.availability == ExecutionAvailability::Live),
            "an old schema reader must not silently label a surviving completed family Lost"
        );
    };
    let result = std::panic::AssertUnwindSafe(verification)
        .catch_unwind()
        .await;
    owner.shutdown_owner().await.unwrap();
    model_server.abort();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

fn tool_result(request: &ModelRequest, id: &str) -> Option<Value> {
    request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|block| {
            if let Content::ToolResult {
                tool_use_id,
                content,
                ..
            } = block
                && tool_use_id == id
            {
                serde_json::from_str(&content.text()).ok()
            } else {
                None
            }
        })
}

#[async_trait]
impl ModelClient for MessageBurstModel {
    async fn generate_title(
        &self,
        request: tickloop_agent::model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent::model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent::model::ModelOutput, tickloop_agent::model::ModelError> {
        Ok(support::title_output(request))
    }

    fn identity(&self) -> tickloop_agent::model::ModelIdentity {
        support::ScriptedModel::new(vec![]).identity()
    }

    async fn complete(
        &self,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        let host: Value = request
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .find_map(|block| match block {
                Content::Text { text } => text
                    .rsplit_once("Current host state: ")
                    .and_then(|(_, value)| serde_json::from_str(value).ok()),
                _ => None,
            })
            .unwrap();
        let content = if let Some(task) = host["delegation"]["task"].as_str() {
            let delivered = request
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .any(|block| matches!(block, Content::Text { text } if text.contains("GO-BURST")));
            if delivered {
                let emit = self.emit_deltas.notified();
                tokio::pin!(emit);
                emit.as_mut().enable();
                self.children_ready.fetch_add(1, Ordering::SeqCst);
                tokio::select! {
                    _ = &mut emit => {},
                    _ = cancel.cancelled() => return Err(ModelError::Cancelled),
                }
                let mut body = String::new();
                for index in 0..1100 {
                    let delta = format!("{task}: delta {index:04}\n");
                    events.send(ModelDelta::Text(delta.clone())).unwrap();
                    body.push_str(&delta);
                }
                support::text(&body)
            } else if tool_result(&request, "ready").is_none() {
                support::call(
                    "ready",
                    "SendMessage",
                    json!({"to":"parent","message":format!("READY {task}")}),
                )
            } else {
                support::text("Ready for the next pushed context.")
            }
        } else if tool_result(&request, "delegate-A").is_none() {
            ["A", "B", "C"]
                .into_iter()
                .flat_map(|label| {
                    support::call(
                        &format!("delegate-{label}"),
                        "Subagent",
                        json!({"task":format!("Child {label}")}),
                    )
                })
                .collect()
        } else if tool_result(&request, "send-A").is_none() {
            self.parent_ready.notify_one();
            tokio::select! {
                _ = self.send_messages.notified() => {},
                _ = cancel.cancelled() => return Err(ModelError::Cancelled),
            }
            ["A", "B", "C"].into_iter().flat_map(|label| {
                support::call(&format!("send-{label}"), "SendMessage", json!({"to":tool_result(&request, &format!("delegate-{label}")).unwrap()["child_chat_id"],"message":"GO-BURST"}))
            }).collect()
        } else {
            support::text("Parent received the accepted messages and child results.")
        };
        Ok(ModelOutput {
            content,
            usage: Default::default(),
            stop_reason: "end_turn".into(),
            private_source: None,
        })
    }
}

struct SocketWorkerFactory {
    directory: PathBuf,
    model: Arc<MessageBurstModel>,
    runtime: Mutex<Option<Agent>>,
    servers: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

#[async_trait]
impl WorkerFactory for SocketWorkerFactory {
    async fn start(
        &self,
        _: &LaunchSpec,
        epoch: Uuid,
    ) -> Result<Arc<dyn AgentAccess>, AccessError> {
        let runtime = Agent::new_embedded(support::config(), self.model.clone());
        *self.runtime.lock().await = Some(runtime.clone());
        let served = Arc::new(EmbeddedAgentAccess::for_worker(runtime, epoch));
        let socket = self.directory.join("worker.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        self.servers.lock().await.push(tokio::spawn(async move {
            serve_worker_listener(listener, served, epoch)
                .await
                .unwrap();
        }));
        let client = LocalAgentClient::connect(&socket).await?;
        Ok(Arc::new(worker_link(client, socket, || true)))
    }

    async fn exited(&self, _: &Arc<dyn AgentAccess>) -> bool {
        false
    }
}

fn overview_query() -> ObservationRequest {
    ObservationRequest::Overview {
        filter: AgentFilter::default(),
        page: Page {
            offset: 0,
            limit: 256,
        },
        after: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn accepted_parent_child_messages_and_burst_feed_cancellation_keep_the_family_live() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let model = Arc::new(MessageBurstModel::default());
    let factory = Arc::new(SocketWorkerFactory {
        directory: directory.path().into(),
        model: model.clone(),
        runtime: Mutex::new(None),
        servers: Mutex::new(Vec::new()),
    });
    let epoch = Uuid::new_v4();
    let owner = Arc::new(OwnerServer::new(epoch, factory.clone()));
    let listener = UnixListener::bind(directory.path().join("owner.sock")).unwrap();
    let server = tokio::spawn(serve_listener(listener, owner, epoch));
    let client = LocalAgentClient::connect(directory.path().join("owner.sock"))
        .await
        .unwrap();
    let created = client
        .execute(CommandEnvelope {
            owner_epoch: epoch,
            command_id: Uuid::new_v4(),
            command: AgentCommand::Create {
                launch: LaunchSpec::embedded(directory.path().into()),
                initial: Some(UserInput::Text {
                    text: "Delegate three scoped children and send follow-up context.".into(),
                    contexts: vec![],
                    paths: vec![],
                }),
                origin: None,
            },
        })
        .await
        .unwrap()
        .into_result()
        .unwrap();
    let CommandResult::Created {
        execution: parent, ..
    } = created
    else {
        panic!("create result");
    };
    tokio::time::timeout(Duration::from_secs(5), model.parent_ready.notified())
        .await
        .unwrap();
    let observed = client.observe(overview_query()).await.unwrap();
    let ObservationValue::Overview(rows) = observed.initial else {
        panic!("directory");
    };
    assert_eq!(rows.len(), 4);
    let targets = rows.iter().map(|row| row.execution).collect::<Vec<_>>();
    let mut feeds = Vec::new();
    for execution in &targets {
        feeds.push(
            client
                .observe(ObservationRequest::Agent {
                    execution: *execution,
                    detail: AgentDetailQuery::Snapshot,
                    after: None,
                })
                .await
                .unwrap(),
        );
    }
    model.send_messages.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        while model.children_ready.load(Ordering::SeqCst) != 3 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let before = client
        .observe(ObservationRequest::Agent {
            execution: parent,
            detail: AgentDetailQuery::Snapshot,
            after: None,
        })
        .await
        .unwrap();
    let ObservationValue::Agent(snapshot) = before.initial else {
        panic!("parent snapshot");
    };
    let accepted = snapshot
        .events
        .iter()
        .filter(|event| {
            matches!(&event.kind,
        tickloop_agent::runtime::EventKind::ToolCompleted { call_id, result }
        if call_id.starts_with("send-") && !result.is_error && result.content.contains("accepted"))
        })
        .count();
    assert_eq!(
        accepted, 3,
        "actual SendMessage tools must be accepted before the stress"
    );
    model.emit_deltas.notify_waiters();
    // Forty rounds of feed churn; on a machine busy with parallel tests they took over 15 s.
    let result = tokio::time::timeout(Duration::from_secs(60), async {
        for _ in 0..40 {
            for execution in &targets {
                drop(
                    client
                        .observe(ObservationRequest::Agent {
                            execution: *execution,
                            detail: AgentDetailQuery::Snapshot,
                            after: None,
                        })
                        .await
                        .unwrap(),
                );
            }
            let ObservationValue::Overview(rows) =
                client.observe(overview_query()).await.unwrap().initial
            else {
                panic!("directory");
            };
            assert_eq!(rows.len(), 4);
            assert!(
                rows.iter()
                    .all(|row| row.availability == ExecutionAvailability::Live),
                "accepted messages caused family Lost: {rows:?}"
            );
        }
        for execution in &targets {
            let ObservationValue::Agent(_) = client
                .observe(ObservationRequest::Agent {
                    execution: *execution,
                    detail: AgentDetailQuery::Snapshot,
                    after: None,
                })
                .await
                .unwrap()
                .initial
            else {
                panic!("snapshot");
            };
        }
    })
    .await;
    drop(feeds.pop());
    drop(feeds);
    let runtime = factory.runtime.lock().await.take().unwrap();
    runtime.shutdown().await;
    for task in factory.servers.lock().await.drain(..) {
        task.abort();
    }
    server.abort();
    result.unwrap();
}
