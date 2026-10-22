//! Real print CLI → daemon → worker → model transport, with private state only.

use crate::support;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

const LIMIT: Duration = Duration::from_secs(45);

async fn request(stream: &mut TcpStream) -> Value {
    let mut bytes = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        let count = stream.read(&mut buffer).await.unwrap();
        assert_ne!(count, 0);
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let length = String::from_utf8_lossy(&bytes[..end])
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|s| s.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            if bytes.len() >= end + 4 + length {
                return serde_json::from_slice(&bytes[end + 4..end + 4 + length]).unwrap();
            }
        }
    }
}

fn stream(block: Value) -> String {
    let stop = if block["type"] == "tool_use" {
        "tool_use"
    } else {
        "end_turn"
    };
    [
        json!({"type":"message_start","message":{"id":"print-fixture","type":"message","role":"assistant","content":[],"usage":{"input_tokens":1,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":block}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":stop},"usage":{"output_tokens":1}}),
        json!({"type":"message_stop"}),
    ].iter().map(|event| format!("data: {event}\n\n")).collect()
}

struct Fixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
    daemon: PathBuf,
    model: PathBuf,
    server: tokio::task::JoinHandle<()>,
    requests: Arc<Mutex<Vec<Value>>>,
}

impl Fixture {
    async fn new(block: Option<Value>) -> Self {
        Self::with_final(block, "fixture final".into()).await
    }

    async fn with_final(block: Option<Value>, final_text: String) -> Self {
        let directory = tempfile::Builder::new()
            .prefix("prt")
            .tempdir_in(tickloop_agent_test_support::SHORT_TEMP_ROOT)
            .unwrap();
        let root = directory.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("home")).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let observed = requests.clone();
        let server = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let observed = observed.clone();
                let block = block.clone();
                let final_text = final_text.clone();
                tokio::spawn(async move {
                    let input = request(&mut socket).await;
                    let has_result = input["messages"].as_array().unwrap().iter().any(|message| {
                        message["content"].as_array().is_some_and(|content| {
                            content.iter().any(|part| part["type"] == "tool_result")
                        })
                    });
                    let ordinary = input["messages"].to_string().contains("ordinary sentinel");
                    observed.lock().unwrap().push(input);
                    let block = if ordinary {
                        Some(
                            json!({"type":"tool_use","id":"ordinary","name":"Bash","input":{"command":"printf '%s' $$ > console.pid; exec sleep 60","run_in_background":true,"description":"Keep Console test alive"}}),
                        )
                    } else {
                        block
                    };
                    let Some(block) = block else {
                        // Keep the request open until cancellation closes its model stream.
                        let mut byte = [0; 1];
                        let _ = socket.read(&mut byte).await;
                        return;
                    };
                    let block = if has_result {
                        json!({"type":"text","text":final_text})
                    } else {
                        block
                    };
                    let body = stream(block);
                    let _ = socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await;
                });
            }
        });
        let model = root.join("model.json");
        std::fs::write(&model, serde_json::to_vec(&json!({"name":"fixture","protocol":"messages","base_url":origin,"model":"fixture","api_key_env":"","max_output_tokens":256})).unwrap()).unwrap();
        Self {
            _directory: directory,
            root,
            daemon: support::cli_runtime::directory(),
            model,
            server,
            requests,
        }
    }

    fn base_command(&self) -> tokio::process::Command {
        let mut command = support::fixture_cli::tokio_command();
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.root.join("home"))
            .env("TICKLOOP_CONFIG_DIR", self.root.join("state"))
            .env("TICKLOOP_DAEMON_RUNTIME_DIR", &self.daemon)
            .env("TICKLOOP_TICKHOST_RUNTIME_DIR", self.root.join("h"))
            .arg("--transport-fixture")
            .arg(&self.model)
            .args(["--mode", "fullAccess", "--cwd"])
            .arg(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }

    fn command(&self, alias: &str, prompt: &str, format: &str) -> tokio::process::Command {
        let mut command = self.base_command();
        command.args([
            alias,
            prompt,
            "--working-directory-is-trusted",
            "--output-format",
            format,
        ]);
        command
    }

    async fn finish(&self) {
        support::cli_runtime::shutdown_daemon(&self.daemon)
            .await
            .unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn print_text_json_and_stream_json_are_clean_and_versioned() {
    let fixture = Fixture::new(Some(json!({"type":"text","text":"fixture final"}))).await;
    for format in ["text", "json", "stream-json"] {
        let output = tokio::time::timeout(LIMIT, fixture.command("-p", "finish", format).output())
            .await
            .unwrap()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        if format == "text" {
            assert_eq!(stdout, "fixture final\n");
        } else if format == "json" {
            let result: Value = serde_json::from_str(&stdout).unwrap();
            assert_eq!(result["text"], "fixture final");
            assert_eq!(result["exit_code"], 0);
            assert_eq!(result["schema_version"], 1);
            assert!(result["chat_id"].is_string());
        } else {
            let events = stdout
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .collect::<Vec<_>>();
            assert!(!events.is_empty());
            let result = events.last().unwrap();
            assert_eq!(result["type"], "result");
            assert_eq!(result["data"]["text"], "fixture final");
            assert_eq!(result["data"]["exit_code"], 0);
        }
    }
    fixture.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn print_long_alias_reads_pipe_to_eof() {
    let fixture = Fixture::new(Some(json!({"type":"text","text":"fixture final"}))).await;
    let mut child = fixture
        .command("--print", "-", "text")
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"pipe-only prompt").await.unwrap();
    drop(stdin);
    let output = tokio::time::timeout(LIMIT, child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    fixture.finish().await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"fixture final\n");
    assert!(
        fixture
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| request["messages"].to_string().contains("pipe-only prompt"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn print_background_shell_is_stopped_before_success() {
    let fixture = Fixture::new(Some(json!({"type":"tool_use","id":"shell","name":"Bash","input":{"command":"printf '%s' $$ > background.pid; exec sleep 60","run_in_background":true,"description":"Wait in print test"}}))).await;
    let output = tokio::time::timeout(
        LIMIT,
        fixture
            .command("-p", "start background shell", "json")
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    let pid = std::fs::read_to_string(fixture.root.join("background.pid"))
        .unwrap()
        .parse::<libc::pid_t>()
        .unwrap();
    let alive = unsafe { libc::kill(pid, 0) == 0 };
    fixture.finish().await;
    assert!(!alive, "background Shell survives print settlement: {pid}");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["text"], "fixture final");
    let requests = fixture.requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        2,
        "managed completion must not start another model turn"
    );
    assert!(
        requests[1]["messages"].to_string().contains("shell:"),
        "{}",
        requests[1]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn print_hosted_tick_is_stopped_before_success() {
    let fixture = Fixture::new(Some(json!({"type":"tool_use","id":"host","name":"TickManage","input":{"action":"open","command":["/bin/sh","-c","printf '%s' $$ > hosted.pid; exec sleep 60"]}}))).await;
    let output = tokio::time::timeout(
        LIMIT,
        fixture.command("-p", "open a hidden Tick", "json").output(),
    )
    .await
    .unwrap()
    .unwrap();
    let pid = std::fs::read_to_string(fixture.root.join("hosted.pid"))
        .unwrap()
        .parse::<libc::pid_t>()
        .unwrap();
    let alive = unsafe { libc::kill(pid, 0) == 0 };
    fixture.finish().await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!alive, "hosted Tick survives print settlement: {pid}");
    let requests = fixture.requests.lock().unwrap();
    assert!(requests[1]["messages"].to_string().contains("hosted:"));
}

#[tokio::test(flavor = "multi_thread")]
async fn print_broken_and_unread_stdout_fail_without_leaking_background_work() {
    for broken in [true, false] {
        let fixture = Fixture::with_final(Some(json!({"type":"tool_use","id":"shell","name":"Bash","input":{"command":"printf '%s' $$ > output.pid; exec sleep 60","run_in_background":true,"description":"Wait for output cleanup"}})), "x".repeat(2 * 1024 * 1024)).await;
        let mut child = fixture
            .command("-p", "start background work", "text")
            .spawn()
            .unwrap();
        // A reader which stays open without draining must not block cancellation/cleanup.
        let stdout = child.stdout.take().unwrap();
        let unread = if broken {
            drop(stdout);
            None
        } else {
            Some(stdout)
        };
        let output = tokio::time::timeout(LIMIT, child.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        drop(unread);
        let pid = std::fs::read_to_string(fixture.root.join("output.pid"))
            .unwrap()
            .parse::<libc::pid_t>()
            .unwrap();
        let alive = unsafe { libc::kill(pid, 0) == 0 };
        fixture.finish().await;
        assert_eq!(
            output.status.code(),
            Some(1),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!alive, "output failure leaked Shell: {pid}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn print_cleanup_preserves_an_unrelated_console_shell() {
    let fixture = Fixture::new(Some(json!({"type":"tool_use","id":"managed","name":"Bash","input":{"command":"printf '%s' $$ > managed.pid; exec sleep 60","run_in_background":true,"description":"Managed cleanup"}}))).await;
    let mut console = fixture
        .base_command()
        .arg("--working-directory-is-trusted")
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = console.stdin.take().unwrap();
    input.write_all(b"ordinary sentinel\n").await.unwrap();
    tokio::time::timeout(LIMIT, async {
        while !fixture.root.join("console.pid").exists()
            || fixture.requests.lock().unwrap().len() < 2
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let console_pid = std::fs::read_to_string(fixture.root.join("console.pid"))
        .unwrap()
        .parse::<libc::pid_t>()
        .unwrap();
    let output = tokio::time::timeout(
        LIMIT,
        fixture.command("-p", "managed work", "text").output(),
    )
    .await
    .unwrap()
    .unwrap();
    let console_alive = console.try_wait().unwrap().is_none();
    let shell_alive = unsafe { libc::kill(console_pid, 0) == 0 };
    let managed_pid = std::fs::read_to_string(fixture.root.join("managed.pid"))
        .unwrap()
        .parse::<libc::pid_t>()
        .unwrap();
    let managed_alive = unsafe { libc::kill(managed_pid, 0) == 0 };
    input.write_all(b"/exit\n").await.unwrap();
    drop(input);
    let console_output = tokio::time::timeout(LIMIT, console.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    fixture.finish().await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        console_alive && shell_alive,
        "print settlement stopped unrelated Console or Shell"
    );
    assert!(!managed_alive, "managed Shell still alive");
    assert!(
        console_output.status.success(),
        "{}",
        String::from_utf8_lossy(&console_output.stderr)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn print_untrusted_directory_is_refused_without_model_or_persisting_trust() {
    let fixture = Fixture::new(Some(json!({"type":"text","text":"must not run"}))).await;
    for _ in 0..2 {
        let output = tokio::time::timeout(
            LIMIT,
            fixture
                .base_command()
                .args(["-p", "untrusted", "--output-format", "json"])
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            output.status.code(),
            Some(3),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["status"], "human_input_required");
        assert!(fixture.requests.lock().unwrap().is_empty());
    }
    fixture.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn print_large_answer_succeeds_while_stdout_is_drained() {
    let answer = "drained answer ".repeat(20_000);
    let fixture = Fixture::new(Some(json!({"type":"text","text":answer}))).await;
    let output = tokio::time::timeout(
        LIMIT,
        fixture.command("-p", "large answer", "text").output(),
    )
    .await
    .unwrap()
    .unwrap();
    fixture.finish().await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("{answer}\n")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn print_timeout_and_signals_cancel_a_live_model_request() {
    for (signal, exit) in [
        (None, 124),
        (Some(libc::SIGINT), 130),
        (Some(libc::SIGTERM), 143),
    ] {
        let fixture = Fixture::new(None).await;
        let mut command = fixture.command("-p", "wait", "json");
        if signal.is_none() {
            command.args(["--timeout", "1"]);
        }
        let child = command.spawn().unwrap();
        let pid = child.id().unwrap();
        tokio::time::timeout(LIMIT, async {
            while fixture.requests.lock().unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        if let Some(signal) = signal {
            assert_eq!(unsafe { libc::kill(pid as libc::pid_t, signal) }, 0);
        }
        let output = tokio::time::timeout(LIMIT, child.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        fixture.finish().await;
        assert_eq!(
            output.status.code(),
            Some(exit),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["exit_code"], exit);
    }
}
