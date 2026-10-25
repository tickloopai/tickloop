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
use tickloop_harness::artifacts::ArtifactClient;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tokio_util::sync::CancellationToken;

const FIXTURE_ARTIFACT_TITLE: &str = "Fixture created artifact";
const FIXTURE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Default)]
struct ObservedRequests {
    linked: AtomicBool,
    listened: AtomicBool,
    attached_to_model: AtomicBool,
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

fn model_stream(create: bool) -> String {
    let block = if create {
        json!({"type":"tool_use","id":"create-call","name":"ArtifactEdit","input":{"action":"create","title":FIXTURE_ARTIFACT_TITLE,"format":"doc","content":"# Fixture"}})
    } else {
        json!({"type":"text","text":"Creation fixture finished."})
    };
    [
        json!({"type":"message_start","message":{"id":"fixture","type":"message","role":"assistant","content":[],"usage":{"input_tokens":1,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":block}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":if create {"tool_use"} else {"end_turn"}},"usage":{"output_tokens":1}}),
        json!({"type":"message_stop"}),
    ].iter().map(|event| format!("data: {event}\n\n")).collect()
}

#[tokio::test]
async fn console_creation_links_the_artifact_and_shows_only_the_artifact_tools() {
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
                let path = path.replace(
                    "e1b85a39-a57b-4c28-a8a5-ae9233aead3e",
                    "6rvWPvOnhVABug7llbHb26",
                );
                let user =
                    json!({"id":"fixture-owner","name":"Fixture","email":"fixture@example.test"});
                let (content_type, body) = if path
                    .starts_with("GET /.well-known/oauth-authorization-server/account ")
                {
                    ("application/json", json!({"issuer":format!("{origin}/account"),"token_endpoint":format!("{origin}/account/token"),"device_authorization_endpoint":format!("{origin}/account/device"),"userinfo_endpoint":format!("{origin}/account/userinfo"),"revocation_endpoint":format!("{origin}/account/revoke"),"grant_types_supported":["urn:ietf:params:oauth:grant-type:device_code"]}).to_string())
                } else if path.starts_with("POST /account/device ") {
                    ("application/json", json!({"device_code":"fixture-device","user_code":"TEST","verification_uri":format!("{origin}/account/device?code=TEST"),"expires_in":30,"interval":1}).to_string())
                } else if path.starts_with("POST /account/token ") {
                    ("application/json", json!({"access_token":"fixture-token","token_type":"Bearer","expires_in":600,"refresh_token":"fixture-refresh"}).to_string())
                } else if path.starts_with("GET /account/userinfo ") {
                    (
                        "application/json",
                        json!({"sub":user["id"],"name":user["name"],"email":user["email"]})
                            .to_string(),
                    )
                } else if path.starts_with("GET /artifact/api/v1/artifacts/6rvWPvOnhVABug7llbHb26 ")
                {
                    ("application/json", json!({"id":"6rvWPvOnhVABug7llbHb26","title":FIXTURE_ARTIFACT_TITLE,"format":"markdown","access_role":"owner","url":format!("{origin}/artifact/6rvWPvOnhVABug7llbHb26"),"agent_links":[]}).to_string())
                } else if path
                    .starts_with("PUT /artifact/api/v1/agent/links/6rvWPvOnhVABug7llbHb26 ")
                {
                    observed.linked.store(true, Ordering::SeqCst);
                    ("application/json", json!({"artifact_id":"6rvWPvOnhVABug7llbHb26","binding_id":request["binding_id"],"epoch":1}).to_string())
                } else if path.starts_with("GET /artifact/api/v1/agent/feedback?") {
                    observed.listened.store(true, Ordering::SeqCst);
                    (
                        "application/json",
                        json!({"items":[],"next_cursor":null}).to_string(),
                    )
                } else if path.starts_with("POST /artifact/mcp ") {
                    let result = match request["method"].as_str().unwrap_or("") {
                        "server/discover" => {
                            json!({"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{}},"ttlMs":0,"cacheScope":"private","_meta":{"io.modelcontextprotocol/serverInfo":{"name":"tickloop-artifacts","version":"1"}}})
                        }
                        "tools/list" => json!({"tools":[
                            {"name":"ArtifactRead","inputSchema":{"type":"object"},"annotations":{"readOnlyHint":true}},
                            {"name":"ArtifactEdit","inputSchema":{"type":"object","properties":{"action":{"type":"string"},"title":{"type":"string"},"format":{"type":"string"},"content":{"type":"string"}},"required":["action"]}},
                            {"name":"ArtifactComment","inputSchema":{"type":"object"}},
                            {"name":"unrelated_remote_tool","inputSchema":{"type":"object"}}
                        ]}),
                        "tools/call" => {
                            assert_eq!(request["params"]["name"], "ArtifactEdit");
                            let args = &request["params"]["arguments"];
                            json!({"content":[],"structuredContent":{"artifact_id":"6rvWPvOnhVABug7llbHb26","title":args["title"],"format":args["format"],"url":format!("{origin}/artifact/6rvWPvOnhVABug7llbHb26"),"revision":1}})
                        }
                        other => panic!("unexpected fixture MCP method: {other}"),
                    };
                    (
                        "application/json",
                        json!({"jsonrpc":"2.0","id":request["id"],"result":result}).to_string(),
                    )
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
        .write_all(b"Create the fixture artifact.\n")
        .await
        .unwrap();
    let completed = tokio::time::timeout(FIXTURE_TIMEOUT, async {
        while observed.model_calls.load(Ordering::SeqCst) < 2
            || !observed.listened.load(Ordering::SeqCst)
        {
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
    assert!(observed.attached_to_model.load(Ordering::SeqCst));
    assert!(
        completed.is_ok(),
        "fixture creation did not reach the model's second request: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        observed.linked.load(Ordering::SeqCst),
        "Console creation did not link the artifact to its Loop"
    );
    assert!(
        observed.listened.load(Ordering::SeqCst),
        "the linked Loop did not listen for @tickloop requests"
    );
    let tools = observed.model_tools.lock().unwrap();
    for name in ["ArtifactRead", "ArtifactEdit", "ArtifactComment"] {
        assert!(tools.iter().any(|tool| tool == name), "{name}");
    }
    assert!(
        !tools
            .iter()
            .any(|name| name.starts_with("mcp__") || *name == "unrelated_remote_tool"),
        "{tools:?}"
    );
}
