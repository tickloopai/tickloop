mod support;

use std::{
    io::{Read, Write},
    net::TcpListener,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

#[test]
fn artifact_commands_are_visible_and_device_login_can_be_cancelled_without_a_model_or_herdr() {
    let service = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", service.local_addr().unwrap());
    let server_origin = origin.clone();
    let server = thread::spawn(move || {
        let (mut stream, _) = service.accept().unwrap();
        let mut request = [0_u8; 4096];
        let count = stream.read(&mut request).unwrap();
        assert!(
            String::from_utf8_lossy(&request[..count]).starts_with("POST /api/v1/auth/device ")
        );
        let body = format!(
            r#"{{"device_code":"private-code","user_code":"ABCD","verification_uri":"{server_origin}/device?code=ABCD","expires_in":30,"interval":2}}"#
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
    });
    let directory = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_tickloop"))
        .env(
            "TICKLOOP_AGENT_RUNTIME_DIR",
            support::cli_runtime::directory(),
        )
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
    let mut stdout = child.stdout.take().unwrap();
    let reader = thread::spawn(move || {
        let mut buffer = [0_u8; 2048];
        while let Ok(count) = stdout.read(&mut buffer) {
            if count == 0 {
                break;
            }
            captured
                .lock()
                .unwrap()
                .push_str(&String::from_utf8_lossy(&buffer[..count]));
        }
    });
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
    server.join().unwrap();
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
