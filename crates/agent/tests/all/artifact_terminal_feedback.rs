use crate::support;

use std::{
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use serde_json::{Value, json};
use tickloop_agent_artifacts::ArtifactClient;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tokio_util::sync::CancellationToken;

const BRIDGE_URI: &str = "tickloop://event-bridge/v1";
const FIXTURE_ARTIFACT_TITLE: &str = "Fixture published artifact";
const FIXTURE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Default)]
struct ObservedRequests {
    missing_v2_read: AtomicBool,
    descriptor_read: AtomicBool,
    artifact_metadata_read: AtomicBool,
    attached_to_model: AtomicBool,
    associated: AtomicBool,
    model_calls: AtomicUsize,
    model_tools: Mutex<Vec<String>>,
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

fn model_stream(publish: bool) -> String {
    let block = if publish {
        json!({"type":"tool_use","id":"publish-call","name":"mcp__artifacts__artifact_publish","input":{"operation_id":"publish-one","source":"# Fixture"}})
    } else {
        json!({"type":"text","text":"Publication fixture finished."})
    };
    [
        json!({"type":"message_start","message":{"id":"fixture","type":"message","role":"assistant","content":[],"usage":{"input_tokens":1,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":block}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":if publish {"tool_use"} else {"end_turn"}},"usage":{"output_tokens":1}}),
        json!({"type":"message_stop"}),
    ].iter().map(|event| format!("data: {event}\n\n")).collect()
}

#[tokio::test]
async fn console_publication_enables_feedback_and_keeps_lifecycle_tools_host_only() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let observed = Arc::new(ObservedRequests::default());
    let server_observed = observed.clone();
    let server_origin = origin.clone();
    let server = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let observed = server_observed.clone();
            let origin = server_origin.clone();
            tokio::spawn(async move {
                let (path, request) = read_request(&mut stream).await;
                let user =
                    json!({"id":"fixture-owner","name":"Fixture","email":"fixture@example.test"});
                let (content_type, body) = if path.starts_with("POST /api/v1/auth/device ") {
                    ("application/json", json!({"device_code":"fixture-device","user_code":"TEST","verification_uri":format!("{origin}/device?code=TEST"),"expires_in":30,"interval":1}).to_string())
                } else if path.starts_with("POST /api/v1/auth/device/token ") {
                    ("application/json", json!({"access_token":"fixture-token","token_type":"Bearer","expires_in":60,"user":user}).to_string())
                } else if path.starts_with("GET /api/v1/auth/session ") {
                    (
                        "application/json",
                        json!({"user":user,"expires_at":"2099-01-01T00:00:00Z"}).to_string(),
                    )
                } else if path.starts_with("GET /api/v1/artifacts/fixture-artifact ") {
                    observed
                        .artifact_metadata_read
                        .store(true, Ordering::SeqCst);
                    ("application/json", json!({"id":"fixture-artifact","title":FIXTURE_ARTIFACT_TITLE,"format":"markdown","access_role":"owner","url":format!("{origin}/a/fixture-artifact")}).to_string())
                } else if path.starts_with("POST /mcp ") {
                    let response = if request["method"] == "resources/read"
                        && request["params"]["uri"] == "tickloop://event-bridge/v2"
                    {
                        observed.missing_v2_read.store(true, Ordering::SeqCst);
                        json!({"jsonrpc":"2.0","id":request["id"],"error":{
                            "code":rmcp::model::ErrorCode::RESOURCE_NOT_FOUND,
                            "message":"Resource not found", "data":{"uri":request["params"]["uri"]}
                        }})
                    } else {
                        let result = match request["method"].as_str().unwrap_or("") {
                            "server/discover" => {
                                json!({"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{},"resources":{}},"ttlMs":0,"cacheScope":"private"})
                            }
                            "tools/list" => {
                                let mut tools = vec![
                                    json!({"name":"artifact_publish","inputSchema":{"type":"object","properties":{"operation_id":{"type":"string"},"source":{"type":"string"}},"required":["operation_id","source"]}}),
                                ];
                                tools.extend(["associate","pending","claim","renew","release","operation"].map(|role| json!({"name":format!("event_{role}"),"inputSchema":{"type":"object"}})));
                                json!({"tools":tools})
                            }
                            "resources/list" => {
                                json!({"resources":[{"uri":BRIDGE_URI,"name":"legacy-event-bridge"}]})
                            }
                            "resources/read" => {
                                assert_eq!(request["params"]["uri"], BRIDGE_URI);
                                observed.descriptor_read.store(true, Ordering::SeqCst);
                                json!({"contents":[{"uri":BRIDGE_URI,"mimeType":"application/json","text":json!({"version":1,"publication_tools":[{"tool":"artifact_publish","operation_id_argument":"operation_id","source_key_result":"artifact_id","version_key_result":"version_id"}],"tools":{"associate":"event_associate","pending":"event_pending","claim":"event_claim","renew":"event_renew","release":"event_release","operation":"event_operation"}}).to_string()}]})
                            }
                            "tools/call" => {
                                let args = &request["params"]["arguments"];
                                let result = match request["params"]["name"].as_str().unwrap_or("")
                                {
                                    "artifact_publish" => {
                                        json!({"status":"committed","operation_id":args["operation_id"],"artifact_id":"fixture-artifact","version_id":"fixture-version"})
                                    }
                                    "event_associate" => {
                                        assert_eq!(args["source_key"], "fixture-artifact");
                                        observed.associated.store(true, Ordering::SeqCst);
                                        json!({"status":"committed","operation_id":args["operation_id"],"source_key":"fixture-artifact","association_epoch":1})
                                    }
                                    "event_pending" => json!({"events":[],"next_cursor":null}),
                                    other => panic!("unexpected fixture tool: {other}"),
                                };
                                json!({"content":[],"structuredContent":result})
                            }
                            other => panic!("unexpected fixture MCP method: {other}"),
                        };
                        json!({"jsonrpc":"2.0","id":request["id"],"result":result})
                    };
                    ("application/json", response.to_string())
                } else {
                    assert!(path.starts_with("POST /v1/messages "), "{path}");
                    *observed.model_tools.lock().unwrap() = request["tools"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|tool| tool["name"].as_str().unwrap().to_owned())
                        .collect();
                    if request.to_string().contains(FIXTURE_ARTIFACT_TITLE) {
                        observed.attached_to_model.store(true, Ordering::SeqCst);
                    }
                    let call = observed.model_calls.fetch_add(1, Ordering::SeqCst);
                    ("text/event-stream", model_stream(call == 0))
                };
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
            });
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let config_root = root.join("state");
    let account = ArtifactClient::for_origin(&config_root, &origin).unwrap();
    let device = account.start_device_login().await.unwrap();
    account
        .finish_device_login(device, &CancellationToken::new())
        .await
        .unwrap();
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
        .args([
            "--working-directory-is-trusted",
            "--mode",
            "fullAccess",
            "--transport-fixture",
        ])
        .arg(&config_path)
        .arg("--cwd")
        .arg(&root)
        .env("TICKLOOP_CONFIG_DIR", &config_root)
        .env("TICKLOOP_ARTIFACT_ORIGIN", &origin)
        .env_remove("HERDR_ENV")
        .env_remove("HERDR_PANE_ID")
        .env_remove("HERDR_SOCKET_PATH")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(b"Publish the fixture artifact.\n")
        .await
        .unwrap();
    let completed = tokio::time::timeout(FIXTURE_TIMEOUT, async {
        while observed.model_calls.load(Ordering::SeqCst) < 2 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    stdin.write_all(b"/exit\n").await.unwrap();
    drop(stdin);
    let output = tokio::time::timeout(FIXTURE_TIMEOUT, child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    support::cli_runtime::shutdown_daemon(&runtime_directory)
        .await
        .unwrap();
    server.abort();
    assert!(observed.artifact_metadata_read.load(Ordering::SeqCst));
    assert!(observed.attached_to_model.load(Ordering::SeqCst));
    assert!(
        completed.is_ok(),
        "fixture publication did not reach the model's second request: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        observed.missing_v2_read.load(Ordering::SeqCst),
        "Console did not try v2 before falling back on its explicit missing-resource result"
    );
    assert!(
        observed.descriptor_read.load(Ordering::SeqCst),
        "Console did not load the feedback bridge descriptor"
    );
    assert!(
        observed.associated.load(Ordering::SeqCst),
        "Console publication did not associate the artifact with its Loop"
    );
    let tools = observed.model_tools.lock().unwrap();
    assert!(
        tools
            .iter()
            .any(|name| name == "mcp__artifacts__artifact_publish")
    );
    assert!(
        !tools
            .iter()
            .any(|name| name.starts_with("mcp__artifacts__event_")),
        "host-only lifecycle tools leaked to the model"
    );
}
