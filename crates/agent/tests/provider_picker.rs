#[path = "support/screen.rs"]
mod screen;

mod support;

use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde_json::Value;

const TIMEOUT: Duration = Duration::from_secs(10);
const FRAME_SETTLE: Duration = Duration::from_millis(40);
const TEST_KEY_NAME: &str = "TICKLOOP_PROVIDER_PICKER_TEST_KEY";
const TEST_KEY: &str = "fixture-provider-picker-key";

struct Terminal {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output: Arc<Mutex<String>>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

impl Terminal {
    fn start(workdir: &Path, provider: Option<&Path>) -> Self {
        Self::start_with_arguments(workdir, provider, &[])
    }

    fn start_with_arguments(workdir: &Path, provider: Option<&Path>, arguments: &[&str]) -> Self {
        let pty = native_pty_system()
            .openpty(PtySize {
                rows: 38,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_tickloop"));
        command.args(["--working-directory-is-trusted", "--mode", "acceptEdits"]);
        command.arg("--cwd");
        command.arg(workdir);
        command.args(arguments);
        if let Some(provider) = provider {
            command.arg("--config");
            command.arg(provider);
        }
        command.env("TICKLOOP_CONFIG_DIR", workdir.join("state"));
        command.env("TICKLOOP_AGENT_TUI", "inline");
        command.env_remove(TEST_KEY_NAME);
        command.env_remove("DEEPSEEK_API_KEY");
        command.env_remove("TYPESAFE_API_KEY");
        command.env_remove("TMUX");
        let child = pty.slave.spawn_command(command).unwrap();
        drop(pty.slave);
        let mut reader = pty.master.try_clone_reader().unwrap();
        let writer = pty.master.take_writer().unwrap();
        let output = Arc::new(Mutex::new(String::new()));
        let captured = output.clone();
        thread::spawn(move || {
            let mut bytes = [0_u8; 8192];
            while let Ok(count) = reader.read(&mut bytes) {
                if count == 0 {
                    break;
                }
                captured
                    .lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&bytes[..count]));
            }
        });
        Self {
            child,
            writer,
            output,
            _master: pty.master,
        }
    }

    fn send(&mut self, input: &[u8]) {
        self.writer.write_all(input).unwrap();
        self.writer.flush().unwrap();
    }

    fn mark(&self) -> usize {
        self.output.lock().unwrap().len()
    }

    fn wait_for(&self, expected: &str) {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            if self.visible_output().contains(expected) {
                thread::sleep(FRAME_SETTLE);
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let output = self.output.lock().unwrap();
        let frame = latest_frame(&output).map(strip_ansi).unwrap_or_default();
        panic!("terminal did not show {expected:?}; latest frame:\n{frame}");
    }

    fn wait_for_frame_after(&self, mark: usize, label: &str, matches: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            let output = self.output.lock().unwrap().clone();
            if output[mark..].contains("\x1b[?2026l")
                && matches(&screen::visible_screen(&output, 120, 38))
            {
                thread::sleep(FRAME_SETTLE);
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let output = self.output.lock().unwrap();
        let last_frame = latest_frame(&output).map(strip_ansi).unwrap_or_default();
        panic!("terminal did not show expected {label} frame; latest:\n{last_frame}");
    }

    fn visible_output(&self) -> String {
        strip_ansi(&self.output.lock().unwrap())
    }

    fn exit(&mut self) {
        self.send(b"/exit\r\r");
        let deadline = Instant::now() + TIMEOUT;
        while self.child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "terminal did not exit");
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn latest_frame(output: &str) -> Option<&str> {
    const BEGIN: &str = "\x1b[?2026h";
    const END: &str = "\x1b[?2026l";
    output
        .match_indices(BEGIN)
        .filter_map(|(start, _)| {
            let start = start + BEGIN.len();
            let end = start + output[start..].find(END)?;
            Some(&output[start..end])
        })
        .last()
}

fn strip_ansi(output: &str) -> String {
    regex::Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]")
        .unwrap()
        .replace_all(output, "")
        .into_owned()
}

fn write_provider(workdir: &Path, address: std::net::SocketAddr) -> PathBuf {
    let provider = workdir.join("picker-provider.yaml");
    fs::write(
        &provider,
        format!(
            r#"id: picker-fixture
display_name: Picker Fixture
auth: api_key_env
api_key_env: {TEST_KEY_NAME}
default_model: picker-messages
endpoints:
  messages: http://{address}
  chat_completions: http://{address}
models:
  - id: picker-messages
    service_model_id: picker-messages-service
    display_name: Fixture Messages
    adapter: messages
    max_output_tokens: 256
    thinking: toggle
    effort_levels: [low, high, max]
  - id: picker-messages-second
    service_model_id: picker-messages-second-service
    display_name: Fixture Messages Two
    adapter: messages
    max_output_tokens: 256
    thinking: toggle
    effort_levels: [low, high, max]
  - id: picker-chat
    service_model_id: picker-chat-service
    display_name: Fixture OpenAI Chat Completions
    adapter: chat_completions
    max_output_tokens: 256
    thinking: toggle
    effort_levels: [low, high, max]
"#
        ),
    )
    .unwrap();
    provider
}

fn write_saved_test_key(workdir: &Path) {
    let state = workdir.join("state");
    fs::create_dir_all(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(state.join(".env"), format!("{TEST_KEY_NAME}={TEST_KEY}\n")).unwrap();
    fs::set_permissions(state.join(".env"), fs::Permissions::from_mode(0o600)).unwrap();
}

#[test]
fn bundled_model_names_match_the_current_model_names_without_transport_suffixes() {
    use tickloop_agent::model::cards::ModelCatalog;

    let catalog = ModelCatalog::defaults();
    for (provider, model_id, expected_name) in [
        (
            "deepseek-api",
            "deepseek-flash-messages",
            "DeepSeek-V4.1-Flash",
        ),
        (
            "deepseek-api",
            "deepseek-flash-responses",
            "DeepSeek-V4.1-Flash",
        ),
        (
            "deepseek-api",
            "deepseek-v4-pro-messages",
            "DeepSeek-V4-Pro-0813",
        ),
        (
            "deepseek-api",
            "deepseek-v4-flash-messages",
            "DeepSeek-V4.1-Flash",
        ),
        ("openai-codex", "gpt-5.5-responses", "GPT-5.5"),
        ("openai-codex", "gpt-5.6-sol-responses", "GPT-5.6 Sol"),
        ("openai-codex", "gpt-5.6-terra-responses", "GPT-5.6 Terra"),
        ("openai-codex", "gpt-5.6-luna-responses", "GPT-5.6 Luna"),
    ] {
        assert_eq!(catalog.get(provider, model_id).unwrap().name, expected_name);
    }
}

#[test]
fn models_keep_their_bound_protocols_and_thinking_choices_are_drafts_until_enter() {
    let directory = tempfile::tempdir().unwrap();
    let workdir = directory.path().canonicalize().unwrap();
    let provider = write_provider(&workdir, "127.0.0.1:1".parse().unwrap());
    write_saved_test_key(&workdir);
    let mut terminal = Terminal::start(&workdir, Some(&provider));
    terminal.wait_for("picker-messages-service");

    terminal.send(b"/model\r");
    terminal.wait_for("Choose Provider");
    assert!(!terminal.visible_output().contains("a Adapter"));
    let mark = terminal.mark();
    terminal.send(b"a\r");
    terminal.wait_for_frame_after(mark, "all models without an API style shortcut", |frame| {
        frame.contains("Choose model · Picker Fixture")
            && frame.contains("Fixture OpenAI Chat Completions")
            && frame.contains("Fixture Messages")
            && frame.contains("Fixture Messages Two")
            && !frame.contains("Set API key")
            && !frame.contains("Back to Providers")
    });
    let mark = terminal.mark();
    terminal.send(b"jj");
    terminal.wait_for_frame_after(mark, "model with another bound protocol", |frame| {
        frame.contains("❯ 3. Fixture OpenAI Chat Completions")
    });

    let mark = terminal.mark();
    terminal.send(b"l");
    terminal.wait_for_frame_after(mark, "Thinking draft changed", |frame| {
        frame.contains("Choose model · Picker Fixture") && frame.contains("Thinking ‹ off ›")
    });
    let mark = terminal.mark();
    terminal.send(b"h");
    terminal.wait_for_frame_after(mark, "Thinking draft moved back", |frame| {
        frame.contains("Choose model · Picker Fixture") && frame.contains("Thinking ‹ auto ›")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b[C");
    terminal.wait_for_frame_after(mark, "right arrow changes Thinking draft", |frame| {
        frame.contains("Choose model · Picker Fixture") && frame.contains("Thinking ‹ off ›")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b[D");
    terminal.wait_for_frame_after(mark, "left arrow restores Thinking draft", |frame| {
        frame.contains("Choose model · Picker Fixture") && frame.contains("Thinking ‹ auto ›")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "Provider after draft cancellation", |frame| {
        frame.contains("Choose Provider") && !frame.contains("Choose model ·")
    });
    assert!(!workdir.join("state/settings.json").exists());

    let mark = terminal.mark();
    terminal.send(b"\r");
    terminal.wait_for_frame_after(mark, "all bound models after cancellation", |frame| {
        frame.contains("Choose model · Picker Fixture")
            && frame.contains("Fixture Messages")
            && frame.contains("Fixture Messages Two")
            && frame.contains("Fixture OpenAI Chat Completions")
    });
    let mark = terminal.mark();
    terminal.send(b"j");
    terminal.wait_for_frame_after(mark, "next model row", |frame| {
        frame.contains("❯ 2. Fixture Messages Two")
            && frame.contains("Choose model · Picker Fixture")
    });
    let mark = terminal.mark();
    terminal.send(b"k");
    terminal.wait_for_frame_after(mark, "previous model row", |frame| {
        frame.contains("❯ 1. Fixture Messages") && frame.contains("Choose model · Picker Fixture")
    });
    let mark = terminal.mark();
    terminal.send(b"jj");
    terminal.wait_for_frame_after(mark, "OpenAI Chat Completions model selected", |frame| {
        frame.contains("❯ 3. Fixture OpenAI Chat Completions")
    });
    let mark = terminal.mark();
    terminal.send(b"l");
    terminal.wait_for_frame_after(mark, "Thinking next draft", |frame| {
        frame.contains("Choose model · Picker Fixture") && frame.contains("Thinking ‹ off ›")
    });
    terminal.send(b"\r");
    terminal.wait_for("Model selection:");
    terminal.wait_for("OpenAI Chat Completions");
    terminal.exit();

    let saved: Value =
        serde_json::from_slice(&fs::read(workdir.join("state/settings.json")).unwrap()).unwrap();
    assert_eq!(saved["defaultModel"]["provider"], "picker-fixture");
    assert_eq!(saved["defaultModel"]["model"], "picker-chat");
    assert_eq!(saved["effortLevel"], "off");
}

#[test]
fn failed_model_save_keeps_the_model_menu_and_allows_return_to_providers() {
    let directory = tempfile::tempdir().unwrap();
    let workdir = directory.path().canonicalize().unwrap();
    let provider = write_provider(&workdir, "127.0.0.1:1".parse().unwrap());
    write_saved_test_key(&workdir);
    let state = workdir.join("state");

    let mut terminal = Terminal::start(&workdir, Some(&provider));
    terminal.wait_for("picker-messages-service");
    terminal.send(b"/model\r");
    terminal.wait_for("Choose Provider");
    let mark = terminal.mark();
    terminal.send(b"\r");
    terminal.wait_for_frame_after(mark, "model before save failure", |frame| {
        frame.contains("Choose model · Picker Fixture")
            && frame.contains("Fixture Messages")
            && !frame.contains("Set API key")
    });

    let blocked_directory = state.join("providers");
    fs::write(&blocked_directory, "not a directory").unwrap();
    let mark = terminal.mark();
    terminal.send(b"\r");
    terminal.wait_for("Cannot apply selection:");
    terminal.wait_for_frame_after(mark, "model after failed save", |frame| {
        frame.contains("Choose model · Picker Fixture")
            && frame.contains("Fixture Messages")
            && frame.contains("Thinking")
    });
    let settings_path = state.join("settings.json");
    if settings_path.exists() {
        let settings: Value = serde_json::from_slice(&fs::read(&settings_path).unwrap()).unwrap();
        assert!(settings.get("defaultModel").is_none());
    }

    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "provider parent after failed save", |frame| {
        frame.contains("Choose Provider") && !frame.contains("Choose model ·")
    });
    fs::remove_file(&blocked_directory).unwrap();
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "prompt after failed save", |frame| {
        !frame.contains("Choose Provider") && frame.contains("picker-messages-service")
    });
    terminal.exit();
    assert!(!terminal.output.lock().unwrap().contains(TEST_KEY));
}

#[test]
fn provider_reauthentication_removes_only_its_saved_key_and_requires_entry_again() {
    let directory = tempfile::tempdir().unwrap();
    let workdir = directory.path().canonicalize().unwrap();
    let provider = write_provider(&workdir, "127.0.0.1:1".parse().unwrap());
    write_saved_test_key(&workdir);
    let env_file = workdir.join("state/.env");
    let existing = fs::read_to_string(&env_file).unwrap();
    fs::write(
        &env_file,
        format!("UNRELATED_FIXTURE_KEY=keep-me\n{existing}"),
    )
    .unwrap();

    let mut terminal = Terminal::start(&workdir, Some(&provider));
    terminal.wait_for("picker-messages-service");
    terminal.send(b"/model\r");
    terminal.wait_for("Choose Provider");
    let mark = terminal.mark();
    terminal.send(b"r");
    terminal.wait_for_frame_after(mark, "key entry after reauthentication", |frame| {
        frame.contains("API key · Picker Fixture") && frame.contains("Save API key")
    });
    let remaining = fs::read_to_string(&env_file).unwrap();
    assert!(remaining.contains("UNRELATED_FIXTURE_KEY=keep-me"));
    assert!(!remaining.contains(TEST_KEY_NAME));
    assert!(!remaining.contains(TEST_KEY));
    assert_eq!(
        fs::metadata(&env_file).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "secret field closed", |frame| {
        frame.contains("API key · Picker Fixture") && frame.contains("Tab API key")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "Provider asks for key again", |frame| {
        frame.contains("Choose Provider") && frame.contains("Picker Fixture · API key required")
    });
    let mark = terminal.mark();
    terminal.send(b"\r");
    terminal.wait_for_frame_after(mark, "key still required on Provider Enter", |frame| {
        frame.contains("API key · Picker Fixture") && frame.contains("Save API key")
    });
    terminal.send(TEST_KEY.as_bytes());
    terminal.wait_for("API key: ********");
    terminal.send(b"\r");
    terminal.wait_for("API key saved to Agent .env.");
    terminal.wait_for("Choose model · Picker Fixture");
    assert!(
        fs::read_to_string(&env_file)
            .unwrap()
            .contains(TEST_KEY_NAME)
    );
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "Provider after saved key", |frame| {
        frame.contains("Choose Provider") && frame.contains("API key saved")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "prompt after reauthentication", |frame| {
        !frame.contains("Choose Provider") && frame.contains("picker-messages-service")
    });
    terminal.exit();
    assert!(!terminal.output.lock().unwrap().contains(TEST_KEY));
}

fn receive_request(listener: &TcpListener) -> (String, Value) {
    let deadline = Instant::now() + TIMEOUT;
    let mut socket = loop {
        match listener.accept() {
            Ok((socket, _)) => break socket,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "no loopback model request");
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("accept failed: {error}"),
        }
    };
    socket.set_read_timeout(Some(TIMEOUT)).unwrap();
    let (headers, body) = read_http_request(&mut socket);
    let answer = [
        serde_json::json!({"type":"message_start","message":{"id":"fixture","type":"message","role":"assistant","content":[],"usage":{"input_tokens":3,"output_tokens":0}}}),
        serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Picker loopback answer"}}),
        serde_json::json!({"type":"content_block_stop","index":0}),
        serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":4}}),
        serde_json::json!({"type":"message_stop"}),
    ]
    .iter()
    .map(|event| format!("data: {event}\n\n"))
    .collect::<String>();
    socket
        .write_all(
            format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}", answer.len()).as_bytes(),
        )
        .unwrap();
    (headers, body)
}

fn receive_request_with_stream(listener: &TcpListener, answer: &str) -> Value {
    let deadline = Instant::now() + TIMEOUT;
    let mut socket = loop {
        match listener.accept() {
            Ok((socket, _)) => break socket,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "no loopback model request");
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("accept failed: {error}"),
        }
    };
    socket.set_nonblocking(false).unwrap();
    socket.set_read_timeout(Some(TIMEOUT)).unwrap();
    let (_, body) = read_http_request(&mut socket);
    socket
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}",
                answer.len()
            )
            .as_bytes(),
        )
        .unwrap();
    body
}

#[test]
fn explicit_ask_menu_offers_only_one_time_approval_or_denial() {
    let directory = tempfile::tempdir().unwrap();
    let workdir = directory.path().canonicalize().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let provider = write_provider(&workdir, listener.local_addr().unwrap());
    write_saved_test_key(&workdir);
    let report = workdir.join("report.txt");
    let tool_input = serde_json::json!({"file_path":"report.txt","content":"approved"});
    let tool_stream = [
        serde_json::json!({"type":"message_start","message":{"id":"fixture","type":"message","role":"assistant","content":[],"usage":{"input_tokens":7,"output_tokens":0}}}),
        serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tool-fixture","name":"Write","input":{}}}),
        serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":tool_input.to_string()}}),
        serde_json::json!({"type":"content_block_stop","index":0}),
        serde_json::json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":4}}),
        serde_json::json!({"type":"message_stop"}),
    ]
    .iter()
    .map(|event| format!("data: {event}\n\n"))
    .collect::<String>();
    let done_stream = [
        serde_json::json!({"type":"message_start","message":{"id":"fixture","type":"message","role":"assistant","content":[],"usage":{"input_tokens":3,"output_tokens":0}}}),
        serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Report complete."}}),
        serde_json::json!({"type":"content_block_stop","index":0}),
        serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":4}}),
        serde_json::json!({"type":"message_stop"}),
    ]
    .iter()
    .map(|event| format!("data: {event}\n\n"))
    .collect::<String>();
    let mut terminal = Terminal::start(&workdir, Some(&provider));
    terminal.wait_for("picker-messages-service");
    terminal.send(b"/rule ask Write\r");
    terminal.wait_for("Rule added:");
    let server = thread::spawn(move || {
        receive_request_with_stream(&listener, &tool_stream);
        receive_request_with_stream(&listener, &done_stream);
    });
    let mark = terminal.mark();
    terminal.send(b"Write the report\r");
    terminal.wait_for_frame_after(mark, "explicit Ask choices", |frame| {
        frame.contains("Permission required")
            && frame.contains("Yes")
            && frame.contains("No")
            && !frame.contains("for this session")
            && !frame.contains("don't ask again")
    });
    assert!(!report.exists());
    terminal.send(b"\r");
    terminal.wait_for("Report complete.");
    assert_eq!(fs::read_to_string(&report).unwrap(), "approved");
    terminal.exit();
    server.join().unwrap();
}

fn read_http_request(socket: &mut TcpStream) -> (String, Value) {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let count = socket.read(&mut buffer).unwrap();
        assert!(count > 0, "model request ended before its body");
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let headers = String::from_utf8(bytes[..end].to_vec()).unwrap();
            let length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(str::trim)
                        .and_then(|value| value.parse::<usize>().ok())
                })
                .unwrap();
            if bytes.len() >= end + 4 + length {
                let body = serde_json::from_slice(&bytes[end + 4..end + 4 + length]).unwrap();
                return (headers, body);
            }
        }
    }
}

#[test]
fn api_key_menu_saves_private_env_and_restarted_model_uses_it_on_loopback() {
    let directory = tempfile::tempdir().unwrap();
    let workdir = directory.path().canonicalize().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let provider = write_provider(&workdir, listener.local_addr().unwrap());
    let state = workdir.join("state");
    fs::create_dir(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(state.join(".env"), "UNRELATED_FIXTURE_KEY=keep-me\n").unwrap();
    fs::set_permissions(state.join(".env"), fs::Permissions::from_mode(0o600)).unwrap();
    let server = thread::spawn(move || [receive_request(&listener), receive_request(&listener)]);

    let mut terminal = Terminal::start(&workdir, Some(&provider));
    terminal.wait_for("picker-messages-service");
    terminal.send(b"/model\r");
    terminal.wait_for("Choose Provider");
    let mark = terminal.mark();
    terminal.send(b"\r");
    terminal.wait_for_frame_after(mark, "API key entry", |frame| {
        frame.contains("API key · Picker Fixture") && frame.contains("Save API key")
    });
    terminal.send(TEST_KEY.as_bytes());
    terminal.wait_for("API key: ********");
    assert!(!terminal.output.lock().unwrap().contains(TEST_KEY));
    terminal.send(b"\r");
    terminal.wait_for("API key saved to Agent .env.");
    terminal.wait_for("Choose model · Picker Fixture");
    terminal.send(b"\r");
    terminal.wait_for("Model selection:");
    terminal.send(b"Provider key first turn\r");
    terminal.wait_for("Picker loopback answer");
    terminal.exit();

    let env_file = state.join(".env");
    let env_contents = fs::read_to_string(&env_file).unwrap();
    assert!(env_contents.contains("UNRELATED_FIXTURE_KEY=keep-me"));
    assert!(env_contents.contains(TEST_KEY_NAME));
    assert!(env_contents.contains(TEST_KEY));
    assert_eq!(
        fs::metadata(&env_file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(!terminal.output.lock().unwrap().contains(TEST_KEY));
    let selection: Value =
        serde_json::from_slice(&fs::read(state.join("settings.json")).unwrap()).unwrap();
    assert_eq!(selection["defaultModel"]["provider"], "picker-fixture");
    assert_eq!(selection["defaultModel"]["model"], "picker-messages");
    let history = state.join("history.jsonl");
    assert!(history.exists());
    assert!(!fs::read_to_string(history).unwrap().contains(TEST_KEY));

    let mut restarted = Terminal::start(&workdir, None);
    restarted.wait_for("picker-messages-service");
    restarted.send(b"Provider key second turn\r");
    restarted.wait_for("Picker loopback answer");
    restarted.exit();
    assert!(!restarted.output.lock().unwrap().contains(TEST_KEY));

    let requests = server.join().unwrap();
    for (headers, body) in requests {
        assert!(
            headers
                .to_ascii_lowercase()
                .contains(&format!("x-api-key: {TEST_KEY}"))
        );
        assert_eq!(body["model"], "picker-messages-service");
    }
}

#[test]
fn model_judgment_settings_save_key_without_changing_model_mode_or_reviewer() {
    const JEV_TEST_KEY: &str = "model-menu-private-jev-fixture-key";
    let directory = tempfile::tempdir().unwrap();
    let workdir = directory.path().canonicalize().unwrap();
    let provider = write_provider(&workdir, "127.0.0.1:1".parse().unwrap());
    write_saved_test_key(&workdir);
    let env_file = workdir.join("state/.env");
    let original = fs::read_to_string(&env_file).unwrap();
    let mut terminal = Terminal::start(&workdir, Some(&provider));
    terminal.wait_for("picker-messages-service");

    terminal.send(b"/model\r");
    terminal.wait_for("Choose Provider");
    let mark = terminal.mark();
    terminal.send(b"jjjjjjjjk\r");
    terminal.wait_for_frame_after(mark, "Judgment settings from model picker", |frame| {
        frame.contains("Judgment settings · Jev (TypeSafe)")
            && frame.contains("Jev (TypeSafe) key: Missing")
            && frame.contains("Set Jev (TypeSafe) key")
    });
    let mark = terminal.mark();
    terminal.send(b"\r");
    terminal.wait_for_frame_after(mark, "key entry from model settings", |frame| {
        frame.contains("Jev (TypeSafe) key") && frame.contains("Save key")
    });
    terminal.send(b"cancelled-model-menu-key");
    terminal.wait_for("API key: ");
    let mark = terminal.mark();
    terminal.send(b"\x1b\x1b");
    terminal.wait_for_frame_after(mark, "cancelled key returns to model settings", |frame| {
        frame.contains("Jev (TypeSafe) key: Missing")
    });
    assert_eq!(fs::read_to_string(&env_file).unwrap(), original);

    let mark = terminal.mark();
    terminal.send(b"\r");
    terminal.wait_for_frame_after(mark, "key re-entry from model settings", |frame| {
        frame.contains("Jev (TypeSafe) key") && frame.contains("Save key")
    });
    terminal.send(JEV_TEST_KEY.as_bytes());
    let mark = terminal.mark();
    terminal.send(b"\r");
    terminal.wait_for_frame_after(mark, "saved key returns to Judgment settings", |frame| {
        frame.contains("Jev (TypeSafe) key: Saved in Agent settings")
            && frame.contains("Update Jev (TypeSafe) key")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "returns to Chat model providers", |frame| {
        frame.contains("Choose Provider") && frame.contains("Judgment settings · Jev (TypeSafe)")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "keeps Chat model and Ask mode", |frame| {
        frame.contains("picker-messages-service") && frame.contains("Ask")
    });
    assert!(!workdir.join("state/settings.json").exists());
    let saved = fs::read_to_string(&env_file).unwrap();
    assert!(saved.contains(&format!("TYPESAFE_API_KEY=\"{JEV_TEST_KEY}\"")));
    assert!(saved.contains(TEST_KEY));
    assert!(!terminal.output.lock().unwrap().contains(JEV_TEST_KEY));
    assert!(
        !terminal
            .output
            .lock()
            .unwrap()
            .contains("cancelled-model-menu-key")
    );
    terminal.exit();
}

#[test]
fn jev_key_editor_masks_saves_and_cancels_without_changing_the_chat_model() {
    const JEV_TEST_KEY: &str = "jev-hidden-fixture-key";
    const UPDATED_KEY: &str = "updated-jev-hidden-fixture-key";
    let directory = tempfile::tempdir().unwrap();
    let workdir = directory.path().canonicalize().unwrap();
    let provider = write_provider(&workdir, "127.0.0.1:1".parse().unwrap());
    write_saved_test_key(&workdir);
    let env_file = workdir.join("state/.env");
    let original = fs::read_to_string(&env_file).unwrap();
    let mut terminal = Terminal::start(&workdir, Some(&provider));
    terminal.wait_for("picker-messages-service");
    let mark = terminal.mark();
    terminal.send(b"/permissions\r");
    terminal.wait_for_frame_after(mark, "permission home after Ask status", |frame| {
        frame.contains("Permissions · Choose mode")
    });
    terminal.send(b"k\r");
    terminal.wait_for("[Auto mode]");
    terminal.wait_for("Reviewer     LLM");
    let mark = terminal.mark();
    terminal.send(b"\rj\r");
    terminal.wait_for_frame_after(mark, "missing key settings", |frame| {
        frame.contains("Jev (TypeSafe) key: Missing")
    });
    let mark = terminal.mark();
    terminal.send(b"\r");
    terminal.wait_for_frame_after(mark, "key entry from Auto reviewer", |frame| {
        frame.contains("Jev (TypeSafe) key") && frame.contains("Save key")
    });
    terminal.send(b"cancelled-jev-key");
    terminal.wait_for("API key: ");
    let mark = terminal.mark();
    terminal.send(b"\x1b\x1b");
    terminal.wait_for_frame_after(
        mark,
        "cancelled key returns to Judgment settings",
        |frame| frame.contains("Jev (TypeSafe) key: Missing"),
    );
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "cancelled key keeps LLM", |frame| {
        frame.contains("Reviewer     LLM") && frame.contains("Mode  Auto")
    });
    assert_eq!(fs::read_to_string(&env_file).unwrap(), original);
    let mark = terminal.mark();
    terminal.send(b"\rj\r");
    terminal.wait_for_frame_after(mark, "missing key settings after cancellation", |frame| {
        frame.contains("Jev (TypeSafe) key: Missing")
    });
    let mark = terminal.mark();
    terminal.send(b"\r");
    terminal.wait_for_frame_after(mark, "key re-entry from Auto reviewer", |frame| {
        frame.contains("Jev (TypeSafe) key") && frame.contains("Save key")
    });
    terminal.send(JEV_TEST_KEY.as_bytes());
    let mark = terminal.mark();
    terminal.send(b"\r");
    terminal.wait_for_frame_after(mark, "saved key selects Jev", |frame| {
        frame.contains("Reviewer     Judgment · Jev (TypeSafe)")
            && frame.contains("authentication has not been verified")
    });
    let mark = terminal.mark();
    terminal.send(b"\r");
    terminal.wait_for_frame_after(mark, "reviewer selector", |frame| {
        frame.contains("Choose Auto reviewer")
            && frame.contains("LLM")
            && frame.contains("This Chat model")
            && frame.contains("Judgment")
            && frame.contains("Jev (TypeSafe) · key saved")
    });
    let mark = terminal.mark();
    terminal.send(b"k\r");
    terminal.wait_for_frame_after(mark, "selected LLM", |frame| {
        frame.contains("Reviewer     LLM")
    });
    let mark = terminal.mark();
    terminal.send(b"\rj\r");
    terminal.wait_for_frame_after(mark, "reuse saved key", |frame| {
        frame.contains("Reviewer     Judgment · Jev (TypeSafe)")
    });
    assert!(
        fs::read_to_string(&env_file)
            .unwrap()
            .contains(JEV_TEST_KEY)
    );
    terminal.send(b"j\r");
    terminal.wait_for("Jev (TypeSafe) key: Saved in Agent settings");
    let mark = terminal.mark();
    terminal.send(b"\r");
    terminal.wait_for_frame_after(mark, "update key entry from Auto settings", |frame| {
        frame.contains("Jev (TypeSafe) key") && frame.contains("Save key")
    });
    terminal.send(UPDATED_KEY.as_bytes());
    let mark = terminal.mark();
    terminal.send(b"\r");
    terminal.wait_for_frame_after(mark, "updated key returns to Judgment settings", |frame| {
        frame.contains("Jev (TypeSafe) key: Saved in Agent settings")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "updated key keeps Jev", |frame| {
        frame.contains("Reviewer     Judgment · Jev (TypeSafe)") && frame.contains("Mode  Auto")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "back to permission mode", |frame| {
        frame.contains("Permissions · Choose mode")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "closed permissions", |frame| {
        !frame.contains("Permissions ·") && frame.contains("picker-messages-service")
    });
    let mark = terminal.mark();
    terminal.send(b"/permissions\r");
    terminal.wait_for("Permissions · Choose mode");
    terminal.send(b"\r");
    terminal.wait_for_frame_after(mark, "permissions opens Auto", |frame| {
        frame.contains("[Auto mode]") && frame.contains("Reviewer     Judgment · Jev (TypeSafe)")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b\x1b");
    terminal.wait_for_frame_after(mark, "closed permissions after reopening", |frame| {
        !frame.contains("Permissions ·") && frame.contains("picker-messages-service")
    });
    terminal.send(b"/settings\r");
    terminal.wait_for("Settings");
    let mark = terminal.mark();
    terminal.send(b"jjjjjjjjjjjjjjjjjjjj\r");
    terminal.wait_for_frame_after(mark, "permissions from Settings", |frame| {
        frame.contains("Permissions · Choose mode")
    });
    thread::sleep(Duration::from_millis(75));
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "returned to Settings", |frame| {
        frame.contains("Settings") && !frame.contains("Permissions ·")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "closed Settings", |frame| {
        !frame.contains("Settings") && frame.contains("picker-messages-service")
    });
    terminal.exit();
    let saved = fs::read_to_string(&env_file).unwrap();
    assert!(saved.contains(UPDATED_KEY));
    assert!(saved.contains(TEST_KEY));
    assert!(!saved.contains("TYPESAFE_API_KEY=jev-hidden-fixture-key"));
    assert_eq!(
        fs::metadata(&env_file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let output = terminal.output.lock().unwrap();
    for secret in [JEV_TEST_KEY, UPDATED_KEY, "cancelled-jev-key"] {
        assert!(!output.contains(secret));
        for path in ["state/settings.json", "state/history.jsonl"] {
            if let Ok(contents) = fs::read_to_string(workdir.join(path)) {
                assert!(!contents.contains(secret));
            }
        }
    }
}

#[test]
fn permission_home_separates_mode_changes_from_recent_activity_and_plan() {
    let directory = tempfile::tempdir().unwrap();
    let workdir = directory.path().canonicalize().unwrap();
    let provider = write_provider(&workdir, "127.0.0.1:1".parse().unwrap());
    write_saved_test_key(&workdir);
    let mut terminal = Terminal::start(&workdir, Some(&provider));
    terminal.wait_for("picker-messages-service");

    let mark = terminal.mark();
    terminal.send(b"/permissions\r");
    terminal.wait_for_frame_after(mark, "permission home", |frame| {
        frame.contains("Permissions · Choose mode")
            && frame.contains("Ask · Allow workspace edits; ask before protected actions · Current")
            && frame.contains("Auto · Use LLM or Judgment review")
            && frame.contains("Full Access")
            && frame.contains("View recent activity · keep current mode")
    });
    let mark = terminal.mark();
    terminal.send(b"4");
    terminal.wait_for_frame_after(mark, "activity without mode change", |frame| {
        frame.contains("[Recent activity]") && !frame.contains("Auto mode")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "mode home after activity", |frame| {
        frame.contains("Permissions · Choose mode")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "Chat after mode home", |frame| {
        !frame.contains("Permissions ·")
    });
    let mark = terminal.mark();
    terminal.send(b"/mode\r");
    terminal.wait_for_frame_after(mark, "activity kept Ask", |frame| {
        frame.contains("Execution mode: Ask")
    });

    let mark = terminal.mark();
    terminal.send(b"/permissions\r");
    terminal.wait_for_frame_after(mark, "permission home after Ask status", |frame| {
        frame.contains("Permissions · Choose mode")
    });
    let mark = terminal.mark();
    terminal.send(b"3");
    terminal.wait_for_frame_after(mark, "Full Access has only activity", |frame| {
        frame.contains("[Recent activity]") && !frame.contains("Auto mode")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "mode home after Full Access", |frame| {
        frame.contains("Permissions · Choose mode")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "Chat after Full Access", |frame| {
        !frame.contains("Permissions ·")
    });
    let mark = terminal.mark();
    terminal.send(b"/mode\r");
    terminal.wait_for_frame_after(mark, "Full Access applied", |frame| {
        frame.contains("Execution mode: Full Access")
    });

    let mark = terminal.mark();
    terminal.send(b"/permissions\r");
    terminal.wait_for_frame_after(mark, "permission home after Full Access status", |frame| {
        frame.contains("Permissions · Choose mode")
    });
    let mark = terminal.mark();
    terminal.send(b"1");
    terminal.wait_for_frame_after(mark, "Auto has settings before activity", |frame| {
        frame.contains("[Auto mode]  Recent activity")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "mode home after Auto", |frame| {
        frame.contains("Permissions · Choose mode")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "Chat after Auto", |frame| {
        !frame.contains("Permissions ·")
    });

    let mark = terminal.mark();
    terminal.send(b"/plan\r");
    terminal.wait_for_frame_after(mark, "Plan without a goal", |frame| {
        frame.contains("Plan mode is active. Enter a task to make a plan.")
    });
    let mark = terminal.mark();
    terminal.send(b"/permissions\r");
    terminal.wait_for_frame_after(mark, "Plan is outside permission modes", |frame| {
        frame.contains("Current: Plan") && frame.contains("❯ 1. Auto")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "Chat after Plan permission home", |frame| {
        !frame.contains("Permissions ·")
    });
    let mark = terminal.mark();
    terminal.send(b"/mode\r");
    terminal.wait_for_frame_after(mark, "cancelling permission home kept Plan", |frame| {
        frame.contains("Execution mode: Plan")
    });
    terminal.exit();
}

#[test]
fn permissions_panel_cancels_an_auto_environment_draft_opened_from_settings() {
    let directory = tempfile::tempdir().unwrap();
    let workdir = directory.path().canonicalize().unwrap();
    let provider = write_provider(&workdir, "127.0.0.1:1".parse().unwrap());
    write_saved_test_key(&workdir);
    fs::write(workdir.join("state/settings.json"), "{}").unwrap();
    let mut terminal = Terminal::start(&workdir, Some(&provider));
    terminal.wait_for("picker-messages-service");
    terminal.send(b"/settings\r");
    terminal.wait_for("Settings");
    terminal.send(b"jjjjjjjjjjjjjjjjjjjj\r");
    terminal.wait_for("Permissions · Choose mode");
    terminal.send(b"k\r");
    terminal.wait_for("[Auto mode]");
    terminal.send(b"jjjjj\r\r");
    terminal.wait_for("Replace Environment");
    let mark = terminal.mark();
    terminal.send(b"jk");
    terminal.wait_for_frame_after(mark, "draft keeps typed j and k", |frame| {
        frame.replace('▏', "").contains("jk")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "cancelled draft", |frame| {
        frame.contains("Replace Environment") && !frame.contains("Text  ")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "Auto mode after Environment", |frame| {
        frame.contains("[Auto mode]")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "mode choice after Auto page", |frame| {
        frame.contains("Permissions · Choose mode")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "Settings after Permissions", |frame| {
        frame.contains("Settings") && !frame.contains("Permissions ·")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "prompt after Settings", |frame| {
        frame.contains("picker-messages-service") && !frame.contains("Settings")
    });
    terminal.exit();
    let settings: Value =
        serde_json::from_slice(&fs::read(workdir.join("state/settings.json")).unwrap()).unwrap();
    assert!(settings["autoApproval"].is_null());
}

#[test]
fn permissions_panel_scrolls_auto_rules_and_full_details_in_a_narrow_terminal() {
    let directory = tempfile::tempdir().unwrap();
    let workdir = directory.path().canonicalize().unwrap();
    let provider = write_provider(&workdir, "127.0.0.1:1".parse().unwrap());
    write_saved_test_key(&workdir);
    let rules = (0..20)
        .map(|index| {
            serde_json::json!({
                "id": uuid::Uuid::new_v4(),
                "text": format!("{} detail_end_{index:02}", "wrapped detail ".repeat(40)),
            })
        })
        .collect::<Vec<_>>();
    let settings = serde_json::json!({
        "autoApproval":{"rules":{"softDeny":{"includeBuiltins":false,"custom":rules}}}
    });
    fs::write(workdir.join("state/settings.json"), settings.to_string()).unwrap();
    let mut terminal = Terminal::start(&workdir, Some(&provider));
    terminal.wait_for("picker-messages-service");
    terminal
        ._master
        .resize(PtySize {
            rows: 18,
            cols: 62,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    terminal.send(b"/permissions\r");
    terminal.wait_for("Permissions · Choose mode");
    terminal.send(b"k\r");
    terminal.wait_for("[Auto mode]");
    terminal.send(b"jjj\r");
    terminal.wait_for("Add custom rule");
    let mark = terminal.mark();
    terminal.send(b"\x1b[Fkk");
    terminal.wait_for_frame_after(mark, "last Auto rule after scrolling", |frame| {
        frame.contains("wrapped detail") && !frame.contains("+ Add custom rule")
    });
    let mark = terminal.mark();
    terminal.send(b"\r");
    terminal.wait_for_frame_after(mark, "wrapped rule detail", |frame| {
        frame.contains("Soft deny") && frame.contains("wrapped detail")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b[6~\x1b[6~");
    terminal.wait_for_frame_after(mark, "end of full rule detail", |frame| {
        frame.contains("detail_end_19") && frame.contains("Source")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "Auto rule list", |frame| {
        frame.contains("Soft deny") && frame.contains("Search  (none)")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "Auto settings", |frame| frame.contains("[Auto mode]"));
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "mode choice after narrow panel", |frame| {
        frame.contains("Permissions · Choose mode")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "prompt after narrow panel", |frame| {
        frame.contains("picker-messages-service") && !frame.contains("Permissions ·")
    });
    terminal.exit();
}

fn messages_tool_stream(id: &str, tool: &str, input: Value) -> String {
    [
        serde_json::json!({"type":"message_start","message":{"id":"fixture","type":"message","role":"assistant","content":[],"usage":{"input_tokens":7,"output_tokens":0}}}),
        serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":id,"name":tool,"input":{}}}),
        serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":input.to_string()}}),
        serde_json::json!({"type":"content_block_stop","index":0}),
        serde_json::json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":4}}),
        serde_json::json!({"type":"message_stop"}),
    ].iter().map(|event| format!("data: {event}\n\n")).collect()
}

fn messages_text_stream(text: &str) -> String {
    [
        serde_json::json!({"type":"message_start","message":{"id":"fixture","type":"message","role":"assistant","content":[],"usage":{"input_tokens":3,"output_tokens":0}}}),
        serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
        serde_json::json!({"type":"content_block_stop","index":0}),
        serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":4}}),
        serde_json::json!({"type":"message_stop"}),
    ].iter().map(|event| format!("data: {event}\n\n")).collect()
}

#[test]
fn recently_denied_restores_details_and_retry_obeys_the_current_ask_rule() {
    let directory = tempfile::tempdir().unwrap();
    let workdir = directory.path().canonicalize().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let provider = write_provider(&workdir, listener.local_addr().unwrap());
    write_saved_test_key(&workdir);
    let rule_id = uuid::Uuid::new_v4();
    let settings_path = workdir.join("state/settings.json");
    let mut settings = serde_json::json!({
        "projects": {workdir.to_string_lossy().as_ref(): {"permissions": {"rules": [{
            "id":rule_id,"decision":"deny","tool":"Write"
        }]}}}
    });
    fs::write(&settings_path, settings.to_string()).unwrap();
    let mut terminal = Terminal::start(&workdir, Some(&provider));
    terminal.wait_for("picker-messages-service");
    let server = thread::spawn(move || {
        receive_request_with_stream(
            &listener,
            &messages_tool_stream(
                "denied-file",
                "Write",
                serde_json::json!({"file_path":"denied-report.txt","content":"private report content"}),
            ),
        );
        receive_request_with_stream(&listener, &messages_text_stream("The report was denied."));
        listener
    });
    terminal.send(b"Prepare a report\r");
    terminal.wait_for("The report was denied.");
    let listener = server.join().unwrap();
    assert!(!workdir.join("denied-report.txt").exists());
    terminal.send(b"/mode ask\r");
    terminal.wait_for("Execution mode: Ask");
    terminal.send(b"/permissions\r");
    terminal.wait_for("Permissions · Choose mode");
    let mark = terminal.mark();
    terminal.send(b"\r");
    terminal.wait_for_frame_after(mark, "recent final denial", |frame| {
        frame.contains("[Recent activity]")
            && frame.contains("denied-report.txt")
            && !frame.contains("private report content")
    });
    let mark = terminal.mark();
    terminal.send(b"\r");
    terminal.wait_for_frame_after(mark, "historical denial details", |frame| {
        frame.contains("Rule denied")
            && frame.contains("Reason:")
            && frame.contains("denied-report.txt")
            && frame.contains("private report content")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b[6~");
    terminal.wait_for_frame_after(mark, "historical policy details", |frame| {
        frame.contains("Policy revision:")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "history list", |frame| {
        frame.contains("Enter details")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "mode choice after history", |frame| {
        frame.contains("Permissions · Choose mode")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "closed history", |frame| {
        !frame.contains("Permissions ·")
    });
    terminal.exit();

    settings["projects"][workdir.to_string_lossy().as_ref()]["permissions"]["rules"][0]["decision"] =
        serde_json::json!("ask");
    fs::write(&settings_path, settings.to_string()).unwrap();
    let mut restarted = Terminal::start_with_arguments(&workdir, Some(&provider), &["--continue"]);
    restarted.wait_for("picker-messages-service");
    restarted.send(b"/permissions\r");
    restarted.wait_for("Permissions · Choose mode");
    let mark = restarted.mark();
    restarted.send(b"\r");
    restarted.wait_for_frame_after(mark, "saved denial after restart", |frame| {
        frame.contains("denied-report.txt") && frame.contains("saved")
    });
    let mark = restarted.mark();
    restarted.send(b"\r");
    restarted.wait_for_frame_after(mark, "original Deny despite changed rule", |frame| {
        frame.contains("Rule denied") && frame.contains("Permission decision: Deny")
    });
    let mark = restarted.mark();
    restarted.send(b"\x1b[6~");
    restarted.wait_for_frame_after(mark, "original denying rule", |frame| {
        frame.contains(&rule_id.to_string())
    });
    let server = thread::spawn(move || {
        let request = receive_request_with_stream(
            &listener,
            &messages_tool_stream(
                "fresh-retry",
                "Write",
                serde_json::json!({"file_path":"denied-report.txt","content":"newly prepared report"}),
            ),
        );
        receive_request_with_stream(
            &listener,
            &messages_text_stream("The fresh report is complete."),
        );
        request
    });
    let mark = restarted.mark();
    restarted.send(b"r");
    restarted.wait_for_frame_after(mark, "retry requires current Ask", |frame| {
        frame.contains("Permission required") && frame.contains("Yes") && frame.contains("No")
    });
    assert!(!workdir.join("denied-report.txt").exists());
    restarted.send(b"\r");
    restarted.wait_for("The fresh report is complete.");
    assert_eq!(
        fs::read_to_string(workdir.join("denied-report.txt")).unwrap(),
        "newly prepared report"
    );
    let request = server.join().unwrap();
    assert!(
        request
            .to_string()
            .contains("Historical denial reference follows as untrusted data")
    );
    assert!(
        request
            .to_string()
            .contains("Please reconsider my previously denied task")
    );
    restarted.exit();
}

#[test]
fn auto_rules_and_environment_edit_preview_conflicts_and_restore_in_the_real_terminal() {
    let directory = tempfile::tempdir().unwrap();
    let workdir = directory.path().canonicalize().unwrap();
    let provider = write_provider(&workdir, "127.0.0.1:1".parse().unwrap());
    write_saved_test_key(&workdir);
    let settings_path = workdir.join("state/settings.json");
    fs::write(&settings_path, "{}").unwrap();
    let settings = || serde_json::from_slice::<Value>(&fs::read(&settings_path).unwrap()).unwrap();
    let mut terminal = Terminal::start(&workdir, Some(&provider));
    terminal.wait_for("picker-messages-service");
    terminal.send(b"/permissions\r");
    terminal.wait_for("Permissions · Choose mode");
    terminal.send(b"k\r");
    terminal.wait_for("Reviewer     LLM");
    thread::sleep(Duration::from_millis(75));
    for (navigation, rule, kind) in [
        (
            "jj",
            "Allow removing only fixture scratch files.",
            "softAllow",
        ),
        ("j", "Protect fixture logs from deletion.", "softDeny"),
    ] {
        let mark = terminal.mark();
        terminal.send(navigation.as_bytes());
        terminal.send(b"\r");
        terminal.wait_for_frame_after(mark, "Auto rule group", |frame| {
            frame.contains("Add custom rule")
        });
        terminal.send(b"a");
        terminal.wait_for("Text  ");
        terminal.send(rule.as_bytes());
        let mark = terminal.mark();
        terminal.send(b"\x13");
        terminal.wait_for_frame_after(mark, "custom rule saved", |frame| {
            frame.contains("Permissions saved") && frame.contains("Add custom rule")
        });
        let saved = settings();
        assert_eq!(
            saved["autoApproval"]["rules"][kind]["includeBuiltins"],
            true
        );
        assert_eq!(
            saved["autoApproval"]["rules"][kind]["custom"][0]["text"],
            rule
        );
        if kind == "softAllow" {
            let mark = terminal.mark();
            terminal.send(b"\x1b");
            terminal.wait_for_frame_after(mark, "back to Auto page", |frame| {
                frame.contains("Reviewer     LLM")
            });
        }
    }
    let mark = terminal.mark();
    terminal.send(b"\x1b[Fk\r");
    terminal.wait_for_frame_after(mark, "replace group editor", |frame| {
        frame.contains("One rule per line")
    });
    terminal.send(b"Protect only the replacement fixture.\x13");
    terminal.wait_for("Will remove 9 item(s)");
    let mut externally_changed = settings();
    externally_changed["autoApproval"]["rules"]["softDeny"]["custom"]
        .as_array_mut()
        .unwrap()
        .push(
            serde_json::json!({"id":uuid::Uuid::new_v4(),"text":"Concurrent fixture protection."}),
        );
    fs::write(&settings_path, externally_changed.to_string()).unwrap();
    let mark = terminal.mark();
    terminal.send(b"\r");
    terminal.wait_for_frame_after(mark, "replacement conflict refreshes preview", |frame| {
        frame.contains("preview refreshed") && frame.contains("10 item")
    });
    assert_eq!(
        settings()["autoApproval"]["rules"]["softDeny"]["includeBuiltins"],
        true
    );
    let mark = terminal.mark();
    terminal.send(b"\r");
    terminal.wait_for_frame_after(mark, "confirmed replacement", |frame| {
        frame.contains("Permissions saved") && frame.contains("Add custom rule")
    });
    assert_eq!(
        settings()["autoApproval"]["rules"]["softDeny"]["includeBuiltins"],
        false
    );
    assert_eq!(
        settings()["autoApproval"]["rules"]["softDeny"]["custom"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let mark = terminal.mark();
    terminal.send(b"\x1b[F\r");
    terminal.wait_for_frame_after(mark, "restore selected group preview", |frame| {
        frame.contains("Will remove 1 item")
            && frame.contains("Protect only the replacement fixture")
    });
    let mark = terminal.mark();
    terminal.send(b"\r");
    terminal.wait_for_frame_after(mark, "restored Soft deny", |frame| {
        frame.contains("Permissions saved") && frame.contains("Add custom rule")
    });
    assert_eq!(
        settings()["autoApproval"]["rules"]["softDeny"]["includeBuiltins"],
        true
    );
    assert_eq!(
        settings()["autoApproval"]["rules"]["softDeny"]["custom"],
        serde_json::json!([])
    );
    assert_eq!(
        settings()["autoApproval"]["rules"]["softAllow"]["custom"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "Auto page after restore", |frame| {
        frame.contains("Reviewer     LLM")
    });
    let mark = terminal.mark();
    terminal.send(b"j\ra");
    terminal.wait_for_frame_after(mark, "Hard deny editor", |frame| {
        frame.contains("Add Auto rule") && frame.contains("Text  ")
    });
    let mark = terminal.mark();
    terminal.send(b"Never delete the protected fixture.\x13");
    terminal.wait_for_frame_after(mark, "saved Hard deny", |frame| {
        frame.contains("Permissions saved") && frame.contains("Add custom rule")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "Auto page before Environment", |frame| {
        frame.contains("Reviewer     LLM")
    });
    terminal.send(b"j\r\r");
    terminal.wait_for("Context only; grants no permission");
    let mark = terminal.mark();
    terminal.send(b"\x01\x0bFixture environment: only this temporary workspace.\x13");
    terminal.wait_for_frame_after(mark, "Environment replacement preview", |frame| {
        frame.contains("Will remove 1 item")
    });
    let mark = terminal.mark();
    terminal.send(b"\r");
    terminal.wait_for_frame_after(mark, "saved Environment", |frame| {
        frame.contains("Fixture environment:") && frame.contains("Permissions saved")
    });
    assert_eq!(
        settings()["autoApproval"]["rules"]["environmentReplacement"],
        "Fixture environment: only this temporary workspace."
    );
    let mark = terminal.mark();
    terminal.send(b"j\r");
    terminal.wait_for_frame_after(mark, "Environment restore preview", |frame| {
        frame.contains("Will remove 1 item") && frame.contains("Fixture environment:")
    });
    let mark = terminal.mark();
    terminal.send(b"\r");
    terminal.wait_for_frame_after(mark, "Environment default restored", |frame| {
        frame.contains("Permissions saved") && !frame.contains("Fixture environment:")
    });
    assert!(settings()["autoApproval"]["rules"]["environmentReplacement"].is_null());
    assert_eq!(
        settings()["autoApproval"]["rules"]["hardDeny"]["custom"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    for label in ["Reviewer     LLM", "Permissions · Choose mode"] {
        let mark = terminal.mark();
        terminal.send(b"\x1b");
        terminal.wait_for_frame_after(mark, "close Auto layer", |frame| frame.contains(label));
    }
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "closed Permissions", |frame| {
        frame.contains("picker-messages-service") && !frame.contains("Permissions ·")
    });
    terminal.exit();
}

#[test]
fn auto_jev_failure_shows_llm_fallback_and_preserves_a_custom_hard_denial_in_details() {
    let directory = tempfile::tempdir().unwrap();
    let workdir = directory.path().canonicalize().unwrap();
    let model_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    model_listener.set_nonblocking(true).unwrap();
    let jev_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let provider = write_provider(&workdir, model_listener.local_addr().unwrap());
    write_saved_test_key(&workdir);
    let settings_path = workdir.join("state/settings.json");
    fs::write(&settings_path, serde_json::json!({"jev":{"endpoint":format!("http://{}/v1/systemone",jev_listener.local_addr().unwrap()),"model":"jev-fixture"}}).to_string()).unwrap();
    let mut terminal = Terminal::start(&workdir, Some(&provider));
    terminal.wait_for("picker-messages-service");
    terminal.send(b"/mode auto\r");
    terminal.wait_for("Auto");
    let mark = terminal.mark();
    terminal.send(b"/permissions\r");
    terminal.wait_for_frame_after(mark, "permission home before fallback", |frame| {
        frame.contains("Permissions · Choose mode")
    });
    let mark = terminal.mark();
    terminal.send(b"\r");
    terminal.wait_for_frame_after(mark, "Auto page before fallback", |frame| {
        frame.contains("[Auto mode]")
    });
    terminal.send(b"\rj\r");
    terminal.wait_for("Jev (TypeSafe) key: Missing");
    terminal.send(b"\r");
    terminal.wait_for("Save key");
    terminal.send(b"fixture-rejected-jev-key");
    let mark = terminal.mark();
    terminal.send(b"\r");
    terminal.wait_for_frame_after(mark, "Jev configured for fallback journey", |frame| {
        frame.contains("Reviewer     Judgment · Jev (TypeSafe)")
    });
    terminal.send(b"jjjj\ra");
    terminal.wait_for("Add Auto rule");
    let mark = terminal.mark();
    terminal.send(b"Never create symlinks in this fixture.\x13");
    terminal.wait_for_frame_after(mark, "Hard deny saved before review", |frame| {
        frame.contains("Permissions saved") && frame.contains("Add custom rule")
    });
    let configured: Value = serde_json::from_slice(&fs::read(&settings_path).unwrap()).unwrap();
    let hard_id = configured["autoApproval"]["rules"]["hardDeny"]["custom"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut findings = support::auto_findings(false);
    findings["rules"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({"id":hard_id,"applicability":"present","clearance":null}));
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "Auto page before request", |frame| {
        frame.contains("Reviewer     Judgment · Jev (TypeSafe)")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "mode choice after Auto panel", |frame| {
        frame.contains("Permissions · Choose mode")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "closed Auto panel", |frame| {
        !frame.contains("Permissions ·")
    });
    let jev_server = thread::spawn(move || {
        let (mut socket, _) = jev_listener.accept().unwrap();
        socket.set_read_timeout(Some(TIMEOUT)).unwrap();
        let (_, request) = read_http_request(&mut socket);
        socket
            .write_all(
                b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
        request
    });
    let model_server = thread::spawn(move || {
        let first = receive_request_with_stream(
            &model_listener,
            &messages_tool_stream(
                "protected-link",
                "Bash",
                serde_json::json!({"command":"ln -s missing-target protected-link"}),
            ),
        );
        let review = receive_request_with_stream(
            &model_listener,
            &messages_text_stream(&findings.to_string()),
        );
        let last = receive_request_with_stream(
            &model_listener,
            &messages_text_stream("Fallback respected the custom restriction."),
        );
        [first, review, last]
    });
    terminal.send(b"Create the fixture symlink if permitted.\r");
    terminal.wait_for("using this Chat's LLM for Auto review");
    terminal.wait_for("Fallback respected the custom restriction.");
    assert!(workdir.join("protected-link").symlink_metadata().is_err());
    let requests = model_server.join().unwrap();
    assert!(
        requests[1]
            .to_string()
            .contains("Never create symlinks in this fixture.")
    );
    assert_eq!(requests[0]["model"], requests[1]["model"]);
    let jev_request = jev_server.join().unwrap();
    assert!(
        jev_request
            .to_string()
            .contains("Never create symlinks in this fixture.")
    );
    let mark = terminal.mark();
    terminal.send(b"/permissions\r");
    terminal.wait_for_frame_after(mark, "permission home after fallback", |frame| {
        frame.contains("Permissions · Choose mode")
    });
    let mark = terminal.mark();
    terminal.send(b"\r");
    terminal.wait_for_frame_after(mark, "Auto page after fallback", |frame| {
        frame.contains("[Auto mode]")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b[C");
    terminal.wait_for_frame_after(mark, "automatic denial list", |frame| {
        frame.contains("[Recent activity]") && frame.contains("Deny · LLM")
    });
    let mark = terminal.mark();
    terminal.send(b"\r");
    terminal.wait_for_frame_after(mark, "fallback denial request", |frame| {
        frame.contains("ln -s missing-target protected-link")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b[6~");
    terminal.wait_for_frame_after(mark, "fallback denial details", |frame| {
        frame.contains("Actual reviewer: LLM") && frame.contains("Jev → LLM fallback")
    });
    assert!(
        !terminal
            .output
            .lock()
            .unwrap()
            .contains("fixture-rejected-jev-key")
    );
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "back to denied list", |frame| {
        frame.contains("Enter details")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "mode choice after denial", |frame| {
        frame.contains("Permissions · Choose mode")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "closed denial panel", |frame| {
        !frame.contains("Permissions ·")
    });
    terminal.exit();
}

#[test]
#[ignore = "waits for the real 60-second Jev cooldown in an Agent PTY"]
fn auto_jev_recovery_notice_appears_after_real_cooldown_in_agent_pty() {
    let directory = tempfile::tempdir().unwrap();
    let workdir = directory.path().canonicalize().unwrap();
    let model_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    model_listener.set_nonblocking(true).unwrap();
    let jev_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let provider = write_provider(&workdir, model_listener.local_addr().unwrap());
    write_saved_test_key(&workdir);
    let key_file = workdir.join("state/.env");
    let mut keys = fs::read_to_string(&key_file).unwrap();
    keys.push_str("TYPESAFE_API_KEY=fixture-jev-recovery-key\n");
    fs::write(&key_file, keys).unwrap();
    let hard_id = uuid::Uuid::new_v4().to_string();
    fs::write(
        workdir.join("state/settings.json"),
        serde_json::json!({
            "jev":{"endpoint":format!("http://{}/v1/systemone",jev_listener.local_addr().unwrap()),"model":"jev-fixture"},
            "autoApproval":{"reviewer":"jev","rules":{
                "hardDeny":{"includeBuiltins":true,"custom":[{
                    "id":hard_id,"text":"Never create fixture symlinks"
                }]}
            }}
        })
        .to_string(),
    )
    .unwrap();
    let mut findings = support::auto_findings(false);
    findings["rules"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({"id":hard_id,"applicability":"present","clearance":null}));
    let model_server = thread::spawn(move || {
        let first_operation = receive_request_with_stream(
            &model_listener,
            &messages_tool_stream(
                "first-link",
                "Bash",
                serde_json::json!({"command":"ln -s missing-target first-link"}),
            ),
        );
        let first_review = receive_request_with_stream(
            &model_listener,
            &messages_text_stream(&findings.to_string()),
        );
        receive_request_with_stream(
            &model_listener,
            &messages_text_stream("The first link was denied."),
        );
        model_listener.set_nonblocking(false).unwrap();
        let second_operation = receive_request_with_stream(
            &model_listener,
            &messages_tool_stream(
                "second-link",
                "Bash",
                serde_json::json!({"command":"ln -s missing-target second-link"}),
            ),
        );
        receive_request_with_stream(
            &model_listener,
            &messages_text_stream("The second link was denied."),
        );
        [first_operation, first_review, second_operation]
    });
    let jev_server = thread::spawn(move || {
        let (mut first, _) = jev_listener.accept().unwrap();
        first.set_read_timeout(Some(TIMEOUT)).unwrap();
        let (_, first_request) = read_http_request(&mut first);
        first
            .write_all(
                b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
        let (mut second, _) = jev_listener.accept().unwrap();
        second.set_read_timeout(Some(TIMEOUT)).unwrap();
        let (_, second_request) = read_http_request(&mut second);
        let mut answers = serde_json::Map::new();
        for (id, question) in second_request["questions"].as_object().unwrap() {
            let criteria = question["criteria"].as_object().unwrap();
            let choice = if id == "task_scope" {
                "within_task"
            } else if id == &format!("app:{hard_id}") {
                "present"
            } else if id.starts_with("clear:") {
                "missing"
            } else {
                "absent"
            };
            assert!(
                criteria.contains_key(choice),
                "invalid fixture choice for {id}"
            );
            let probabilities: serde_json::Map<String, Value> = criteria
                .keys()
                .map(|option| {
                    (
                        option.clone(),
                        serde_json::json!(f64::from(option == choice)),
                    )
                })
                .collect();
            answers.insert(
                id.clone(),
                serde_json::json!({
                    "type":"choice","choice":choice,"confidence":1.0,"probabilities":probabilities
                }),
            );
        }
        let response = serde_json::json!({
            "model":"jev-fixture","answers":answers,"usage":{"input_tokens":50,"output_tokens":10}
        })
        .to_string();
        write!(second, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
        (first_request, second_request)
    });

    let mut terminal = Terminal::start(&workdir, Some(&provider));
    terminal.wait_for("picker-messages-service");
    terminal.send(b"/mode auto\r");
    terminal.wait_for("Auto");
    terminal.send(b"Create the first fixture symlink if permitted.\r");
    terminal
        .wait_for("Jev review unavailable (Authentication); using this Chat's LLM for Auto review");
    terminal.wait_for("The first link was denied.");
    assert!(workdir.join("first-link").symlink_metadata().is_err());
    thread::sleep(Duration::from_secs(61));
    let mark = terminal.mark();
    terminal.send(b"Create the second fixture symlink if permitted.\r");
    terminal.wait_for_frame_after(mark, "readable Jev recovery notice", |frame| {
        frame.contains("Jev Auto review recovered")
    });
    terminal.wait_for("The second link was denied.");
    assert!(workdir.join("second-link").symlink_metadata().is_err());
    let model_requests = model_server.join().unwrap();
    assert_eq!(model_requests[0]["model"], model_requests[1]["model"]);
    assert!(
        model_requests[2]
            .to_string()
            .contains("Create the second fixture symlink")
    );
    let (first_jev, second_jev) = jev_server.join().unwrap();
    assert_eq!(first_jev["model"], second_jev["model"]);
    let latest = latest_frame(&terminal.output.lock().unwrap())
        .map(strip_ansi)
        .unwrap_or_default();
    assert_eq!(latest.matches("Jev Auto review recovered").count(), 1);
    let mark = terminal.mark();
    terminal.send(b"/permissions\r");
    terminal.wait_for_frame_after(mark, "permission home after recovery", |frame| {
        frame.contains("Permissions · Choose mode")
    });
    let mark = terminal.mark();
    terminal.send(b"\r");
    terminal.wait_for_frame_after(mark, "Auto page after recovery", |frame| {
        frame.contains("[Auto mode]")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b[C");
    terminal.wait_for_frame_after(mark, "recent Jev denial", |frame| {
        frame.contains("[Recent activity]") && frame.contains("Deny · Jev")
    });
    let mark = terminal.mark();
    terminal.send(b"\r");
    terminal.wait_for_frame_after(mark, "Jev reviewer in newest denial", |frame| {
        frame.contains("Actual reviewer: Jev")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "back to recent denials", |frame| {
        frame.contains("Enter details")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "mode choice after recent activity", |frame| {
        frame.contains("Permissions · Choose mode")
    });
    let mark = terminal.mark();
    terminal.send(b"\x1b");
    terminal.wait_for_frame_after(mark, "closed recent denials", |frame| {
        !frame.contains("Permissions ·")
    });
    terminal.exit();
}
