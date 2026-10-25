#![cfg(unix)]

use crate::support;

use std::{
    io::{Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixListener},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde_json::json;

fn incompatible_daemon_output(terminal: bool) -> String {
    let fixture = support::runtime_dir();
    let root = fixture.path().canonicalize().unwrap();
    let config = root.join("config with ' and $(literal)");
    std::fs::create_dir(&config).unwrap();
    let runtime = support::cli_runtime::adopt(&config);
    tickloop_code_client::daemon_endpoint::validate_directory(&runtime).unwrap();
    let socket = runtime.join("daemon.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = socket.clone();
    let cli_finished = Arc::new(AtomicBool::new(false));
    let daemon_cli_finished = cli_finished.clone();
    let daemon = thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(std::time::Instant::now() < deadline, "CLI never connected");
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("{error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut header = [0u8; 4];
        stream.read_exact(&mut header).unwrap();
        let mut request = vec![0u8; u32::from_be_bytes(header) as usize];
        stream.read_exact(&mut request).unwrap();
        let hello: serde_json::Value = serde_json::from_slice(&request).unwrap();
        assert!(hello.get("Hello").is_some());
        let response = serde_json::to_vec(&json!({"Hello": {
            "major": 1, "minor": 1, "daemon_epoch": uuid::Uuid::new_v4(),
            "session_id": uuid::Uuid::new_v4(), "capabilities": []
        }}))
        .unwrap();
        stream
            .write_all(&(response.len() as u32).to_be_bytes())
            .unwrap();
        stream.write_all(&response).unwrap();
        stream.flush().unwrap();
        let mut rest = Vec::new();
        stream.read_to_end(&mut rest).unwrap();
        assert!(
            rest.is_empty(),
            "no command may follow an incompatible Hello"
        );
        assert!(endpoint.exists(), "the existing daemon socket must remain");
        let mut identity_probe = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "no peer identity probe"
                    );
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("{error}"),
            }
        };
        identity_probe.set_nonblocking(true).unwrap();
        loop {
            match identity_probe.read(&mut [0u8; 1]) {
                Ok(0) => break,
                Ok(_) => panic!("the peer identity probe must not dispatch a request"),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "the peer identity probe did not close"
                    );
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => panic!("{error}"),
            }
        }
        while !daemon_cli_finished.load(Ordering::Acquire) {
            assert!(
                matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
                "cancel must not open a management connection or send ShutdownDaemon"
            );
            assert!(
                std::time::Instant::now() < deadline,
                "CLI did not finish after the incompatible daemon response"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
            "no extra connection may follow the peer identity probe"
        );
    });
    let canonical_executable = std::fs::canonicalize(env!("CARGO_BIN_EXE_tickloop")).unwrap();
    let executable = canonical_executable.to_str().unwrap();
    let working_directory = if terminal {
        root.clone()
    } else {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap()
    };
    let output = if terminal {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 32,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(executable);
        command.args(["--working-directory-is-trusted"]);
        command.cwd(&working_directory);
        command.env_clear();
        command.env("PATH", "/usr/bin:/bin");
        command.env("HOME", &root);
        command.env("TERM", "xterm-256color");
        command.env("TICKLOOP_CONFIG_DIR", &config);

        command.env("TICKLOOP_FIXTURE_API_KEY", "private-fixture-key");
        let mut child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let mut writer = pair.master.take_writer().unwrap();
        let reader = thread::spawn(move || {
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 4096];
            let mut answered = false;
            while let Ok(count) = reader.read(&mut chunk) {
                if count == 0 {
                    break;
                }
                bytes.extend_from_slice(&chunk[..count]);
                if !answered
                    && String::from_utf8_lossy(&bytes)
                        .contains("Stop the running daemon and continue?")
                {
                    writer.write_all(b"n\r").unwrap();
                    writer.flush().unwrap();
                    answered = true;
                }
            }
            assert!(
                answered,
                "interactive startup must offer confirmation before exiting: {}",
                String::from_utf8_lossy(&bytes)
            );
            bytes
        });
        assert_eq!(child.wait().unwrap().exit_code(), 0);
        String::from_utf8(reader.join().unwrap()).unwrap()
    } else {
        let output = Command::new(executable)
            .args(["--working-directory-is-trusted"])
            .current_dir(&working_directory)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &root)
            .env("TERM", "xterm-256color")
            .env("NO_COLOR", "1")
            .env("TICKLOOP_CONFIG_DIR", &config)
            .env("TICKLOOP_FIXTURE_API_KEY", "private-fixture-key")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        String::from_utf8(output.stderr).unwrap()
    };
    cli_finished.store(true, Ordering::Release);
    let output = output.replace('\r', "");
    daemon.join().unwrap();
    assert!(socket.exists());
    assert!(!runtime.join("daemon.lock").exists());
    assert!(!output.contains("private-fixture-key"));
    if terminal {
        assert!(output.contains("Stop the running daemon and continue?"));
        assert!(output.contains("Saved history is kept"));
        assert!(!output.contains("TICKLOOP_CONFIG_DIR="));
        return output;
    }
    assert!(!output.contains("incompatible AgentAccess protocol major"));
    assert!(output.contains("This stops running Agents. Saved history is kept."));
    assert!(output.contains("Stop the daemon and relaunch:"));
    assert!(!output.contains("mktemp"));
    let command = output
        .split("Stop the daemon and relaunch:")
        .nth(1)
        .unwrap()
        .split("This stops running Agents.")
        .next()
        .unwrap()
        .trim();
    let command = command.replace('\r', "");
    let mut syntax = Command::new("/bin/sh")
        .arg("-n")
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    syntax
        .stdin
        .take()
        .unwrap()
        .write_all(command.as_bytes())
        .unwrap();
    assert!(syntax.wait().unwrap().success());
    assert!(command.contains(&shell_words::quote(config.to_str().unwrap()).to_string()));
    let commands = command.lines().map(str::trim).collect::<Vec<_>>();
    assert_eq!(commands.len(), 2);
    assert!(commands[0].ends_with(" daemon stop"));
    let resolved_executable = std::fs::canonicalize(executable).unwrap();
    let resolved_executable = resolved_executable.to_str().unwrap();
    let quoted_executable = shell_words::quote(resolved_executable);
    assert!(
        commands[0].ends_with(&format!("{quoted_executable} daemon stop")),
        "{command}"
    );
    assert!(commands[1].ends_with(quoted_executable.as_ref()));
    assert!(output.contains(&format!(
        "Runtime: {}",
        runtime.canonicalize().unwrap().display()
    )));
    assert!(output.contains(&format!("Executable: {resolved_executable}")));
    assert!(output.contains(&format!(
        "client {}.{}",
        tickloop_code_access::PROTOCOL_MAJOR,
        tickloop_code_access::PROTOCOL_MINOR
    )));
    assert!(output.contains("peer protocol 1.1"));
    let config_argument = format!(
        "TICKLOOP_CONFIG_DIR={}",
        shell_words::quote(config.to_str().unwrap())
    );
    assert!(
        commands
            .iter()
            .all(|line| line.starts_with(&config_argument))
    );
    assert!(!command.contains("--working-directory-is-trusted"));
    assert!(!command.contains("--config"));
    output
}

#[test]
fn incompatible_daemon_has_readable_plain_guidance_without_dispatching() {
    let output = incompatible_daemon_output(false);
    assert!(output.starts_with(&format!(
        "Error: Agent daemon version mismatch (client {}.{}).",
        tickloop_code_access::PROTOCOL_MAJOR,
        tickloop_code_access::PROTOCOL_MINOR
    )));
    assert!(!output.contains('\x1b'));
}

#[test]
fn incompatible_daemon_terminal_offers_cancel_without_stopping() {
    let output = incompatible_daemon_output(true);
    assert!(output.contains("Stop daemon and open this version"));
    assert!(output.contains("Cancel"));
}

fn accept_within(listener: &UnixListener, label: &str) -> std::os::unix::net::UnixStream {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                return stream;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(std::time::Instant::now() < deadline, "{label}");
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("{error}"),
        }
    }
}

fn read_json_frame(stream: &mut std::os::unix::net::UnixStream) -> serde_json::Value {
    let mut header = [0u8; 4];
    stream.read_exact(&mut header).unwrap();
    let mut request = vec![0u8; u32::from_be_bytes(header) as usize];
    stream.read_exact(&mut request).unwrap();
    serde_json::from_slice(&request).unwrap()
}

fn write_json_frame(stream: &mut std::os::unix::net::UnixStream, value: &serde_json::Value) {
    let response = serde_json::to_vec(value).unwrap();
    stream
        .write_all(&(response.len() as u32).to_be_bytes())
        .unwrap();
    stream.write_all(&response).unwrap();
    stream.flush().unwrap();
}

#[tokio::test]
async fn a_compatible_daemon_build_is_reused_without_management_or_a_restart_prompt() {
    use tickloop_code_access::{
        AgentAccess, AgentFilter, ObservationRequest, ObservationValue, Page,
    };

    let fixture = support::runtime_dir();
    let root = fixture.path().canonicalize().unwrap();
    let socket = root.join("daemon.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let epoch = uuid::Uuid::new_v4();
    let daemon = thread::spawn(move || {
        let mut stream = accept_within(&listener, "the client never connected");
        assert!(read_json_frame(&mut stream).get("Hello").is_some());
        let hello = tickloop_code_access::ServerHello {
            process_id: std::process::id(),
            major: tickloop_code_access::PROTOCOL_MAJOR,
            minor: tickloop_code_access::PROTOCOL_MINOR,
            daemon_epoch: epoch,
            session_id: uuid::Uuid::new_v4(),
            capabilities: Vec::new(),
            limits: tickloop_code_access::AccessLimits::DEFAULT.into(),
            build: Some("0.0.0+another-build".into()),
            executable: None,
            upgrade_protocol: None,
            handoff: None,
        };
        write_json_frame(&mut stream, &json!({ "Hello": hello }));
        let query = read_json_frame(&mut stream);
        assert!(query.get("Observe").is_some(), "{query}");
        write_json_frame(
            &mut stream,
            &json!({"Opened": {
                "request_id": query["Observe"]["request_id"],
                "observation_id": uuid::Uuid::new_v4(),
                "initial": {"Overview": []},
                "cursor": {"daemon_epoch": epoch, "execution": null, "sequence": 0},
                "reset": null,
                "feed": false
            }}),
        );
    });
    let client =
        tickloop_code_client::connect_or_start_at(root, env!("CARGO_BIN_EXE_tickloop").into())
            .await
            .unwrap();
    assert_eq!(
        client.protocol().build.as_deref(),
        Some("0.0.0+another-build")
    );
    let result = client
        .observe(ObservationRequest::Overview {
            filter: AgentFilter::default(),
            page: Page::default(),
            after: None,
        })
        .await
        .unwrap();
    assert!(matches!(result.initial, ObservationValue::Overview(rows) if rows.is_empty()));
    daemon.join().unwrap();
    assert!(socket.exists());
}
