use std::path::Path;

use serde_json::{Value, json};
use tick_protocol::InstanceRef;
use tickhost_connect::{
    access::TickAccess,
    herdr::{HOST_TIMEOUT, HerdrContext, HerdrPort, SocketHerdr},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
    sync::oneshot,
};
use tokio_util::sync::CancellationToken;

fn adapter(socket: &Path) -> SocketHerdr {
    SocketHerdr::new(HerdrContext {
        socket: socket.to_owned(),
        caller_pane: "test-pane".into(),
    })
}

#[tokio::test]
async fn missing_host_socket_is_a_diagnostic_before_dispatch() {
    let directory = tempfile::tempdir().unwrap();
    let error = adapter(&directory.path().join("missing.sock"))
        .request("pane.current", json!({}), CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.code, "HostUnavailable");
    assert!(!error.dispatched);
}

async fn malformed_reply(reply: impl FnOnce(Value) -> Vec<u8> + Send + 'static) {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("herdr.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut request = String::new();
        reader.read_line(&mut request).await.unwrap();
        let request: Value = serde_json::from_str(&request).unwrap();
        reader.get_mut().write_all(&reply(request)).await.unwrap();
    });
    let error = adapter(&socket)
        .request("pane.current", json!({}), CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.code, "ProtocolViolation");
    assert!(error.dispatched);
    server.await.unwrap();
}

#[tokio::test]
async fn disconnected_invalid_incomplete_and_wrong_identity_replies_are_protocol_violations() {
    malformed_reply(|_| Vec::new()).await;
    malformed_reply(|_| b"not JSON\n".to_vec()).await;
    malformed_reply(|request| {
        format!("{}\n", json!({"id":request["id"],"wrong":"shape"})).into_bytes()
    })
    .await;
    malformed_reply(|_| b"{\"id\":\"another\",\"result\":{}}\n".to_vec()).await;
    malformed_reply(|request| format!("{}", json!({"id":request["id"],"result":{}})).into_bytes())
        .await;
}

#[tokio::test(start_paused = true)]
async fn response_timeout_after_host_receives_request_has_unknown_outcome() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("herdr.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let (received, request_received) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut request = String::new();
        reader.read_line(&mut request).await.unwrap();
        received.send(()).unwrap();
        std::future::pending::<()>().await;
    });
    let client = tokio::spawn(async move {
        adapter(&socket)
            .request(
                "pane.send_input",
                json!({"text":"once"}),
                CancellationToken::new(),
            )
            .await
            .unwrap_err()
    });
    request_received.await.unwrap();
    tokio::time::advance(HOST_TIMEOUT).await;
    let error = client.await.unwrap();
    assert_eq!(error.code, "OutcomeUnknown");
    assert!(error.dispatched);
    server.abort();
}

#[tokio::test]
async fn unsupported_terminal_read_format_is_reported_instead_of_returned_as_plain_text() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("herdr.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut request = String::new();
            reader.read_line(&mut request).await.unwrap();
            let request: Value = serde_json::from_str(&request).unwrap();
            let pane = json!({"pane_id":"p1","terminal_id":"t1","workspace_id":"w1","tab_id":"tab1","cwd":"/tmp","foreground_cwd":"/tmp"});
            let result = match request["method"].as_str().unwrap() {
                "pane.list" => json!({"panes":[pane]}),
                "pane.process_info" => {
                    json!({"process_info":{"foreground_process_group_id":1,"foreground_processes":[{"pid":std::process::id()}]}})
                }
                "pane.read" => {
                    json!({"read":{"source":"rendered","format":"ansi","text":"not plain text"}})
                }
                method => panic!("unexpected Herdr method {method}"),
            };
            reader
                .get_mut()
                .write_all(format!("{}\n", json!({"id":request["id"],"result":result})).as_bytes())
                .await
                .unwrap();
        }
    });
    let access = TickAccess::new(HerdrContext {
        socket,
        caller_pane: "p1".into(),
    });
    let discovered = access.discover("w1").await.unwrap();
    let instance: InstanceRef =
        serde_json::from_value(discovered["ticks"][0]["tick_instance_id"].clone()).unwrap();
    let error = access.read(&instance).await.unwrap_err();
    assert_eq!(error.code, "IncompatibleHost");
    assert!(!error.dispatched);
    server.abort();
}

#[tokio::test]
async fn missing_opaque_target_is_rejected_before_any_target_lookup_or_input() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("herdr.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut request = String::new();
        reader.read_line(&mut request).await.unwrap();
        let request: Value = serde_json::from_str(&request).unwrap();
        let method = request["method"].as_str().unwrap().to_owned();
        reader
            .get_mut()
            .write_all(
                format!("{}\n", json!({"id":request["id"],"result":{"pane":{"pane_id":"agent","terminal_id":"agent-term","workspace_id":"w1","tab_id":"tab1"}}})).as_bytes(),
            )
            .await
            .unwrap();
        let next =
            tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept()).await;
        (method, next.is_ok())
    });
    let access = TickAccess::new(HerdrContext {
        socket,
        caller_pane: "agent".into(),
    });
    let error = access
        .prepare(
            "modify_tick",
            &json!({"action":"send_input","text":"never sent"}),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "InvalidArguments");
    assert!(!error.dispatched);
    assert!(error.message.contains("tick_instance_id"));
    let (method, extra_request) = server.await.unwrap();
    assert_eq!(method, "pane.current");
    assert!(!extra_request);
}

#[tokio::test]
async fn open_forwards_the_agents_locale_to_the_new_pane() {
    const CHILD_MARKER: &str = "TICKLOOP_TEST_LOCALE_CHILD";
    const EXPECTED_ENVIRONMENT: [(&str, &str); 10] = [
        ("LANG", "en_US.UTF-8"),
        ("LC_ALL", "C"),
        ("LC_CTYPE", "en_US.UTF-8"),
        ("LC_COLLATE", "C"),
        ("LC_MESSAGES", "C"),
        ("LC_MONETARY", "C"),
        ("LC_NUMERIC", "C"),
        ("LC_TIME", "C"),
        ("TICKLOOP_CONFIG_DIR", "/private/tmp/tickloop-test-config"),
        (
            "TICKLOOP_TICK_RUNTIME_DIR",
            "/private/tmp/tickloop-test-runtime",
        ),
    ];
    if std::env::var_os(CHILD_MARKER).is_none() {
        let output = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "open_forwards_the_agents_locale_to_the_new_pane"])
            .env(CHILD_MARKER, "1")
            .envs(EXPECTED_ENVIRONMENT)
            .output()
            .await
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "child test failed:\n{stdout}\n{stderr}"
        );
        assert!(
            stdout.contains("1 passed"),
            "child test did not run:\n{stdout}"
        );
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("herdr.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let mut launch_environment = None;
        for _ in 0..2 {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut request = String::new();
            reader.read_line(&mut request).await.unwrap();
            let request: Value = serde_json::from_str(&request).unwrap();
            let response = match request["method"].as_str().unwrap() {
                "pane.current" => {
                    json!({"id":request["id"],"result":{"pane":{"pane_id":"agent","terminal_id":"agent-term","workspace_id":"w1","tab_id":"tab1"}}})
                }
                "layout.apply" => {
                    launch_environment = Some(request["params"]["root"]["env"].clone());
                    json!({"id":request["id"],"error":{"code":"Captured","message":"layout captured"}})
                }
                method => panic!("unexpected Herdr method {method}"),
            };
            reader
                .get_mut()
                .write_all(format!("{response}\n").as_bytes())
                .await
                .unwrap();
        }
        launch_environment.unwrap()
    });
    let access = TickAccess::new(HerdrContext {
        socket,
        caller_pane: "agent".into(),
    });
    let prepared = access
        .prepare(
            "modify_tick",
            &json!({"action":"open","command":["/bin/sh"],"cwd":directory.path()}),
        )
        .await
        .unwrap();
    let error = access
        .execute(prepared, CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.code, "Captured");
    let environment = server.await.unwrap();
    for (name, value) in EXPECTED_ENVIRONMENT {
        assert_eq!(environment[name], value, "{name} was not forwarded");
    }
}

struct ManyPanes {
    panes: std::sync::Mutex<usize>,
}

#[async_trait::async_trait]
impl HerdrPort for ManyPanes {
    async fn request(
        &self,
        method: &str,
        params: Value,
        _: CancellationToken,
    ) -> Result<Value, tickhost_connect::host_error::HostError> {
        let pane = |index: usize| json!({"pane_id":format!("p{index:03}"),"terminal_id":format!("t{index:03}"),"workspace_id":"w1","tab_id":"tab1","cwd":"/tmp","foreground_cwd":"/tmp"});
        Ok(match method {
            "pane.list" => {
                json!({"panes":(0..*self.panes.lock().unwrap()).map(pane).collect::<Vec<_>>()})
            }
            "pane.process_info" => {
                json!({"process_info":{"foreground_process_group_id":1,"foreground_processes":[{"pid":std::process::id()}]}})
            }
            "pane.get" => {
                let index = params["pane_id"].as_str().unwrap()[1..].parse().unwrap();
                json!({"pane":pane(index)})
            }
            method => panic!("unexpected Herdr method {method}"),
        })
    }
}

async fn read_workspace(access: &TickAccess, input: Value) -> Result<Value, String> {
    let prepared = access
        .prepare("read_workspace", &input)
        .await
        .map_err(|error| error.to_string())?;
    access
        .execute(prepared, CancellationToken::new())
        .await
        .map_err(|error| error.to_string())
}

#[tokio::test]
async fn workspace_discovery_pages_the_complete_set_and_expires_changed_cursors() {
    let host = std::sync::Arc::new(ManyPanes {
        panes: std::sync::Mutex::new(300),
    });
    let access = TickAccess::with_host(
        HerdrContext::without_view("/tmp/paged-herdr.sock".into()),
        host.clone(),
    );
    let mut seen = std::collections::BTreeSet::new();
    let mut cursor = Value::Null;
    loop {
        let mut input = json!({"workspace_id":"w1"});
        if !cursor.is_null() {
            input["cursor"] = cursor.clone();
        }
        let page = read_workspace(&access, input).await.unwrap();
        assert_eq!(page["total"], 300);
        let ticks = page["ticks"].as_array().unwrap();
        assert!(ticks.len() <= 128, "a page holds at most 128 programs");
        for tick in ticks {
            assert!(
                seen.insert(tick["tick_instance_id"].as_str().unwrap().to_owned()),
                "no program repeats across pages"
            );
        }
        cursor = page["next_cursor"].clone();
        if cursor.is_null() {
            break;
        }
    }
    assert_eq!(seen.len(), 300, "every program is reachable");

    let first = read_workspace(&access, json!({"workspace_id":"w1","limit":10}))
        .await
        .unwrap();
    assert_eq!(first["ticks"].as_array().unwrap().len(), 10);
    *host.panes.lock().unwrap() = 299;
    let expired = read_workspace(
        &access,
        json!({"workspace_id":"w1","cursor":first["next_cursor"].clone()}),
    )
    .await
    .unwrap_err();
    assert!(expired.contains("CursorExpired"), "{expired}");
    for limit in [0, 129] {
        let error = read_workspace(&access, json!({"workspace_id":"w1","limit":limit}))
            .await
            .unwrap_err();
        assert!(error.contains("InvalidArguments"), "{error}");
    }
}

struct FailingProcessInfo;

#[async_trait::async_trait]
impl HerdrPort for FailingProcessInfo {
    async fn request(
        &self,
        method: &str,
        _: Value,
        _: CancellationToken,
    ) -> Result<Value, tickhost_connect::host_error::HostError> {
        match method {
            "pane.list" => Ok(
                json!({"panes":[{"pane_id":"p1","terminal_id":"t1","workspace_id":"w1","tab_id":"tab1"}]}),
            ),
            "pane.process_info" => Err(tickhost_connect::host_error::HostError::before(
                "HostUnavailable",
                "process_info failed",
            )),
            method => panic!("unexpected Herdr method {method}"),
        }
    }
}

#[tokio::test]
async fn a_program_that_cannot_be_identified_counts_as_uncertain_not_absent() {
    let access = TickAccess::with_host(
        HerdrContext::without_view("/tmp/failing-herdr.sock".into()),
        std::sync::Arc::new(FailingProcessInfo),
    );
    let (located, uncertain) = access.locate_all().await.unwrap();
    assert!(located.is_empty());
    assert_eq!(
        uncertain, 1,
        "an unanswered program may still be the member"
    );
}
