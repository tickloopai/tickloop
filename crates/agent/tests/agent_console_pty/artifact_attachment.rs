use super::*;
use tickloop_agent::artifacts::ArtifactClient;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const CREATED_TITLE: &str = "LIVE-CREATED-ARTIFACT";
const ATTACHMENT_URI: &str = "tickloop://artifact-tick-attachments/v1";

async fn artifact_service() -> (String, Arc<Mutex<Vec<Value>>>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let requests = recorded.clone();
    let service_origin = origin.clone();
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let origin = service_origin.clone();
            let recorded = recorded.clone();
            tokio::spawn(async move {
                let mut bytes = Vec::new();
                let (end, length) = loop {
                    let mut buffer = [0; 4096];
                    let count = stream.read(&mut buffer).await.unwrap();
                    if count == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&buffer[..count]);
                    if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
                        let length = headers
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length: "))
                            .and_then(|value| value.parse::<usize>().ok())
                            .unwrap_or(0);
                        if bytes.len() >= end + 4 + length {
                            break (end, length);
                        }
                    }
                };
                let head = String::from_utf8_lossy(&bytes[..end]);
                let path = head.lines().next().unwrap();
                let body: Value = serde_json::from_slice(&bytes[end + 4..end + 4 + length])
                    .unwrap_or(Value::Null);
                recorded.lock().unwrap().push(body.clone());
                let response = if path.starts_with("POST /api/v1/auth/device/token ") {
                    json!({"access_token":"test-artifact-token","token_type":"Bearer","expires_in":3600,"user":{"id":"owner","name":"Owner","email":"owner@example.test"}})
                } else if path.starts_with("POST /api/v1/auth/device ") {
                    json!({"device_code":"device","user_code":"TEST","verification_uri":format!("{origin}/device?code=TEST"),"expires_in":30,"interval":1})
                } else if path.starts_with("GET /api/v1/auth/session ") {
                    json!({"user":{"id":"owner","name":"Owner","email":"owner@example.test"},"expires_at":"2099-01-01T00:00:00Z"})
                } else if path.starts_with("GET /api/v1/artifacts/document ") {
                    json!({"id":"document","title":CREATED_TITLE,"format":"markdown","access_role":"owner","url":format!("{origin}/a/document")})
                } else if path.starts_with("POST /mcp ") {
                    let result = match body["method"].as_str().unwrap() {
                        "server/discover" => {
                            json!({"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{},"resources":{}},"ttlMs":0,"cacheScope":"private"})
                        }
                        "tools/list" => {
                            let mut tools = ["associate","pending","claim","renew","release","operation"].into_iter().map(|role|json!({"name":format!("event_{role}"),"inputSchema":{"type":"object"}})).collect::<Vec<_>>();
                            tools.push(
                                json!({"name":"publish_custom","inputSchema":{"type":"object"}}),
                            );
                            tools.push(json!({"name":"create_custom","inputSchema":{"type":"object","properties":{"operation_id":{"type":"string"},"title":{"type":"string"},"format":{"type":"string"}},"required":["operation_id","title","format"]}}));
                            json!({"tools":tools})
                        }
                        "resources/list" => {
                            json!({"resources":[{"uri":ATTACHMENT_URI,"name":"attachment"}]})
                        }
                        "resources/read" => {
                            let uri = body["params"]["uri"].as_str().unwrap();
                            let descriptor = if uri == ATTACHMENT_URI {
                                json!({"version":1,"creation_tools":[{"tool":"create_custom","operation_id_argument":"operation_id","artifact_id_result":"artifact_id","title_argument":"title","format_argument":"format","url_result":"url"}]})
                            } else {
                                json!({"version":1,"publication_tools":[{"tool":"publish_custom","operation_id_argument":"operation_id","source_key_result":"artifact_id","version_key_result":"version_id"}],"tools":{"associate":"event_associate","pending":"event_pending","claim":"event_claim","renew":"event_renew","release":"event_release","operation":"event_operation"}})
                            };
                            json!({"contents":[{"uri":uri,"mimeType":"application/json","text":descriptor.to_string()}]})
                        }
                        "tools/call" => {
                            assert_eq!(body["params"]["name"], "create_custom");
                            json!({"content":[],"structuredContent":{"status":"committed","artifact_id":"document","operation_id":body["params"]["arguments"]["operation_id"],"url":format!("{origin}/a/document")}})
                        }
                        other => panic!("unexpected method {other}"),
                    };
                    json!({"jsonrpc":"2.0","id":body["id"],"result":result})
                } else {
                    json!({"error":"not_found"})
                };
                let response = response.to_string();
                let message = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                    response.len()
                );
                let _ = stream.write_all(message.as_bytes()).await;
            });
        }
    });
    (origin, requests, server)
}

#[tokio::test]
async fn actual_cli_creation_adds_an_artifact_tick_before_the_next_model_reply() {
    let fixture = PrivateFixture::new();
    let (origin, calls, server) = artifact_service().await;
    let result = AssertUnwindSafe(async {
        let account = ArtifactClient::for_origin(fixture.path("config").canonicalize().unwrap(), &origin).unwrap();
        let device = account.start_device_login().await.unwrap();
        account.finish_device_login(device, &tokio_util::sync::CancellationToken::new()).await.unwrap();
        let (model_url, mut requests, release) = model_http_support::serve_responses_with_gated_response(vec![
            (200,chat_reply(Some(("mcp__artifacts__create_custom","creation",json!({"operation_id":"creation-one","title":CREATED_TITLE,"format":"markdown"}))),"")),
            (200,chat_reply(None,"CREATION-REPLY-FINISHED")),
        ],1).await;
        fixture.set_chat_model(&model_url);
        let mut pty = PtyView::launch(&fixture, &["--dangerously-skip-permissions"], &[("TICKLOOP_ARTIFACT_ORIGIN", std::path::Path::new(&origin))], true);
        pty.resize(160,32).await;
        pty.wait_for("artifact View ready",new_input_is_visible).await;
        pty.send(b"Create a document\r");
        let _ = tokio::time::timeout(UI_TIMEOUT,requests.recv()).await.unwrap().unwrap();
        let next = tokio::time::timeout(UI_TIMEOUT, requests.recv()).await;
        assert!(next.is_ok(), "frame: {}\ncalls: {:?}", pty.frame(), calls.lock().unwrap());
        next.unwrap().unwrap();

        let frame = pty.wait_for("creation is attached before the reply",|frame|frame.contains("Monitor 1") && frame.contains(CREATED_TITLE) && frame.contains("Artifacts (1)")).await;
        assert!(!frame.contains("CREATION-REPLY-FINISHED"));
        eprintln!("[artifact-created-live]\n{frame}\n[/artifact-created-live]");
        assert!(!calls.lock().unwrap().iter().any(|body|body["params"]["name"]=="event_associate"));
        release.send(()).unwrap();
        pty.wait_for("creation run finished",|frame|frame.contains("CREATION-REPLY-FINISHED")).await;
        pty.close().await;
    }).catch_unwind().await;
    let stopped = std::process::Command::new(env!("CARGO_BIN_EXE_tickloop"))
        .args(["daemon", "stop"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", fixture.path("home"))
        .env("TICKLOOP_CONFIG_DIR", fixture.path("config"))
        .env("TICKLOOP_DAEMON_RUNTIME_DIR", fixture.path("runtime"))
        .output()
        .unwrap();
    fixture.shutdown().await;
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    server.abort();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
