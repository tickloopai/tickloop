use crate::support;
use tickloop_harness::test_support::screen;

use std::{
    io::{Read, Write},
    net::TcpListener,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

#[test]
fn post_login_account_failure_preserves_the_view_and_artifacts_can_retry() {
    const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
    use std::sync::atomic::{AtomicBool, Ordering};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let endpoint = origin.clone();
    let stopped = Arc::new(AtomicBool::new(false));
    let stop = stopped.clone();
    let server = thread::spawn(move || {
        let mut sessions = 0;
        while !stop.load(Ordering::SeqCst) {
            let Ok((mut socket, _)) = listener.accept() else {
                thread::sleep(Duration::from_millis(10));
                continue;
            };
            socket.set_nonblocking(false).unwrap();
            socket.set_read_timeout(Some(HTTP_REQUEST_TIMEOUT)).unwrap();
            let mut bytes = [0_u8; 8192];
            let mut count = 0;
            while !bytes[..count].windows(4).any(|end| end == b"\r\n\r\n") {
                assert!(
                    count < bytes.len(),
                    "fixture HTTP headers exceed the request budget"
                );
                let received = socket.read(&mut bytes[count..]).unwrap();
                assert!(
                    received > 0,
                    "fixture HTTP request ended before its headers"
                );
                count += received;
            }
            let request = String::from_utf8_lossy(&bytes[..count]);
            let user =
                serde_json::json!({"id":"owner","name":"Owner","email":"owner@example.test"});
            let (status, body) = if request
                .starts_with("GET /.well-known/oauth-authorization-server/account ")
            {
                (
                    200,
                    serde_json::json!({"issuer":format!("{endpoint}/account"),"token_endpoint":format!("{endpoint}/account/token"),"device_authorization_endpoint":format!("{endpoint}/account/device"),"userinfo_endpoint":format!("{endpoint}/account/userinfo"),"revocation_endpoint":format!("{endpoint}/account/revoke"),"grant_types_supported":["urn:ietf:params:oauth:grant-type:device_code"]}),
                )
            } else if request.starts_with("POST /account/device ") {
                (
                    200,
                    serde_json::json!({"device_code":"device","user_code":"CODE","verification_uri":format!("{endpoint}/account/device?code=CODE"),"expires_in":60,"interval":1}),
                )
            } else if request.starts_with("POST /account/token ") {
                (
                    200,
                    serde_json::json!({"access_token":"test-token","token_type":"Bearer","expires_in":600,"refresh_token":"test-refresh"}),
                )
            } else if request.starts_with("GET /account/userinfo ") {
                sessions += 1;
                if sessions == 2 {
                    (503, serde_json::json!({"error":"temporarily_unavailable"}))
                } else {
                    (
                        200,
                        serde_json::json!({"sub":user["id"],"name":user["name"],"email":user["email"]}),
                    )
                }
            } else {
                (
                    200,
                    serde_json::json!({"artifacts":[{"id":"doc","title":"Retry worked","format":"markdown","access_role":"owner","url":format!("{endpoint}/a/doc")},{"id":"deck","title":"Launch review","format":"slides","access_role":"editor","url":format!("{endpoint}/a/deck")}],"next_cursor":null}),
                )
            };
            let body = body.to_string();
            socket.write_all(format!("HTTP/1.1 {status} Result\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).unwrap();
        }
    });
    let directory = tempfile::tempdir().unwrap();
    support::cli_runtime::adopt(&directory.path().join("state"));
    let mut child = Command::new(env!("CARGO_BIN_EXE_tickloop"))
        .args(["--working-directory-is-trusted", "--cwd"])
        .arg(directory.path())
        .env("TICKLOOP_CONFIG_DIR", directory.path().join("state"))
        .env("TICKLOOP_ARTIFACT_ORIGIN", &origin)
        .env_remove("HERDR_ENV")
        .env_remove("HERDR_PANE_ID")
        .env_remove("HERDR_SOCKET_PATH")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let output = Arc::new(Mutex::new(String::new()));
    let captured = output.clone();
    let stdout = child.stdout.take().unwrap();
    let reader = thread::spawn(move || screen::capture_output(stdout, captured));
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"/artifacts\n").unwrap();
    stdin.flush().unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while !output.lock().unwrap().contains("connection failed") && Instant::now() < deadline {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    let alive = child.try_wait().unwrap().is_none();
    if alive {
        stdin.write_all(b"/artifacts\n").unwrap();
        stdin.flush().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !output.lock().unwrap().contains("Retry worked") && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        stdin.write_all(b"/exit\n").unwrap();
    }
    drop(stdin);
    let status = child.wait().unwrap();
    stopped.store(true, Ordering::SeqCst);
    server.join().unwrap();
    reader.join().unwrap();
    let output = output.lock().unwrap();
    assert!(alive, "post-login refresh closed the View: {output}");
    assert!(
        status.success()
            && output.contains("connection failed")
            && output.contains("Retry worked · Doc · owner")
            && output.contains("Launch review · Slides · editor"),
        "{output}"
    );
}

#[test]
fn artifact_commands_are_visible_and_device_login_can_be_cancelled_without_a_model_or_herdr() {
    let server = tick_oauth_test_server::AuthorizationServer::start_with_path("/account");
    server.edit_device_authorization(|device| {
        device["user_code"] = serde_json::json!("ABCD");
        device["verification_uri_complete"] = serde_json::Value::Null;
    });
    let origin = server.issuer().strip_suffix("/account").unwrap().to_owned();
    let directory = tempfile::tempdir().unwrap();
    support::cli_runtime::adopt(&directory.path().join("state"));
    let mut child = Command::new(env!("CARGO_BIN_EXE_tickloop"))
        .args(["--working-directory-is-trusted", "--cwd"])
        .arg(directory.path())
        .env("TICKLOOP_CONFIG_DIR", directory.path().join("state"))
        .env("TICKLOOP_ARTIFACT_ORIGIN", &origin)
        .env_remove("HERDR_ENV")
        .env_remove("HERDR_PANE_ID")
        .env_remove("HERDR_SOCKET_PATH")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let output = Arc::new(Mutex::new(String::new()));
    let captured = output.clone();
    let stdout = child.stdout.take().unwrap();
    let reader = thread::spawn(move || screen::capture_output(stdout, captured));
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"/help\n/artifacts\n").unwrap();
    stdin.flush().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !output.lock().unwrap().contains("Enter code: ABCD") && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        output.lock().unwrap().contains("Enter code: ABCD"),
        "terminal did not show the device authorization: {}",
        output.lock().unwrap()
    );
    stdin
        .write_all(b"cancel\n/artifacts status\n/artifacts logout\n/exit\n")
        .unwrap();
    drop(stdin);
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "terminal did not exit");
        thread::sleep(Duration::from_millis(20));
    };
    reader.join().unwrap();
    assert_eq!(
        server
            .requests_to(tick_oauth_test_server::DEVICE_AUTHORIZATION_PATH)
            .len(),
        1
    );
    let output = output.lock().unwrap();
    assert!(status.success(), "terminal exited with {status}");
    assert!(
        output.contains("Choose artifacts to add to this Chat"),
        "{output}"
    );
    assert!(output.contains("Login cancelled."), "{output}");
    assert!(output.contains("Use /artifacts"), "{output}");
    assert!(!output.contains("private-code"), "{output}");
}
