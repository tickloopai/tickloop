#![cfg(unix)]

use crate::support;

use std::{
    io::{Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixListener},
    process::{Command, Stdio},
    thread,
    time::Duration,
};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde_json::json;

fn incompatible_daemon_output(terminal: bool) -> String {
    let fixture = support::runtime_dir();
    let root = fixture.path().canonicalize().unwrap();
    let runtime = root.join("runtime");
    std::fs::create_dir(&runtime).unwrap();
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = root.join("config with ' and $(literal)");
    std::fs::create_dir(&config).unwrap();
    let socket = runtime.join("daemon.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = socket.clone();
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
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
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
        command.env("TICKLOOP_DAEMON_RUNTIME_DIR", &runtime);
        command.env("TICKLOOP_FIXTURE_API_KEY", "private-fixture-key");
        let mut child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let reader = thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = reader.read_to_end(&mut bytes);
            bytes
        });
        assert_eq!(child.wait().unwrap().exit_code(), 1);
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
            .env("TICKLOOP_DAEMON_RUNTIME_DIR", &runtime)
            .env("TICKLOOP_FIXTURE_API_KEY", "private-fixture-key")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        String::from_utf8(output.stderr).unwrap()
    };
    let output = output.replace('\r', "");
    daemon.join().unwrap();
    assert!(socket.exists());
    assert!(!runtime.join("daemon.lock").exists());
    assert!(!output.contains("private-fixture-key"));
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
    assert!(output.contains(&format!("Runtime: {}", runtime.display())));
    assert!(output.contains(&format!("Executable: {resolved_executable}")));
    assert!(output.contains(&format!(
        "client {}.{}",
        tickloop_agent_protocol::PROTOCOL_MAJOR,
        tickloop_agent_protocol::PROTOCOL_MINOR
    )));
    assert!(output.contains("peer protocol 1.1"));
    let runtime_argument = format!(
        "TICKLOOP_DAEMON_RUNTIME_DIR={}",
        shell_words::quote(runtime.to_str().unwrap())
    );
    assert!(commands.iter().all(|line| line.contains(&runtime_argument)));
    assert!(!command.contains("--working-directory-is-trusted"));
    assert!(!command.contains("--config"));
    output
}

#[test]
fn incompatible_daemon_has_readable_plain_guidance_without_dispatching() {
    let output = incompatible_daemon_output(false);
    assert!(output.starts_with(&format!(
        "Error: Agent daemon version mismatch (client {}.{}).",
        tickloop_agent_protocol::PROTOCOL_MAJOR,
        tickloop_agent_protocol::PROTOCOL_MINOR
    )));
    assert!(!output.contains('\x1b'));
}

#[test]
fn incompatible_daemon_terminal_styles_only_the_title_and_preserves_guidance() {
    let output = incompatible_daemon_output(true);
    assert!(output.contains("\x1b["));
    assert!(output.contains("Error: Agent daemon version mismatch"));
    assert!(output.contains("\n\nStop the daemon and relaunch:"));
}
