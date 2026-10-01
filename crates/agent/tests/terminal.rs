mod support;

use std::io::{Read, Write};
use std::process::Command;
use std::{process::Stdio, thread, time::Duration};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde_json::{Value, json};
use tickloop_agent::settings::UserSettings;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

const INTERACTION_TIMEOUT: Duration = Duration::from_secs(10);

fn trust_working_directory(config_root: &std::path::Path, workdir: &std::path::Path) {
    UserSettings::in_root(config_root)
        .trust_working_directory(workdir)
        .unwrap();
}

fn trusted_agent_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tickloop"));
    command.args(["--working-directory-is-trusted", "--mode", "acceptEdits"]);
    command
}

fn trusted_async_agent_command() -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_tickloop"));
    command.args(["--working-directory-is-trusted", "--mode", "acceptEdits"]);
    command
}

fn product_async_agent_command() -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_tickloop"));
    command.arg("--working-directory-is-trusted");
    command
}

#[test]
fn malformed_auto_settings_do_not_exit_the_terminal_on_permissions_commands() {
    let directory = tempfile::tempdir().unwrap();
    let config_root = directory.path().join("config-root");
    std::fs::create_dir_all(&config_root).unwrap();
    std::fs::write(config_root.join("settings.json"), r#"{"autoApproval":{"rules":{"softDeny":{"includeBuiltins":false,"custom":[{"id":"invalid","text":"rule"}]}}}}"#).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_tickloop"))
        .args(["--working-directory-is-trusted", "--no-session-persistence"])
        .env("TICKLOOP_CONFIG_DIR", &config_root)
        .current_dir(directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(b"/permissions\n/rules\n/jev\n/exit\n")?;
            child.wait_with_output()
        })
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Permissions view requires a terminal"),
        "{stdout}"
    );
    assert!(stdout.contains("Cannot list permission rules"), "{stdout}");
    assert!(
        stdout.contains("Jev (TypeSafe) key settings require the interactive /model picker"),
        "{stdout}"
    );
}

#[test]
fn interactive_pty_trusts_the_directory_and_cycles_all_execution_modes() {
    let directory = tempfile::tempdir().unwrap();
    let config_root = directory.path().join("config-root");
    let pty = native_pty_system()
        .openpty(PtySize {
            rows: 30,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut reader = pty.master.try_clone_reader().unwrap();
    let mut writer = pty.master.take_writer().unwrap();
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_tickloop"));
    command.cwd(directory.path());
    command.env("TICKLOOP_CONFIG_DIR", &config_root);
    command.env("TICKLOOP_AGENT_TUI", "inline");
    command.arg("--no-session-persistence");
    let mut child = pty.slave.spawn_command(command).unwrap();
    drop(pty.slave);
    let output = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let reader_output = output.clone();
    let output_reader = thread::spawn(move || {
        let mut buffer = [0_u8; 4096];
        while let Ok(length) = reader.read(&mut buffer) {
            if length == 0 {
                break;
            }
            reader_output
                .lock()
                .unwrap()
                .push_str(&String::from_utf8_lossy(&buffer[..length]));
        }
    });
    let wait_for_output = |needle: &str, after: usize| {
        let deadline = std::time::Instant::now() + INTERACTION_TIMEOUT;
        let ansi = regex::Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").unwrap();
        loop {
            let captured = output.lock().unwrap();
            if captured
                .get(after..)
                .is_some_and(|suffix| ansi.replace_all(suffix, "").contains(needle))
            {
                return;
            }
            if std::time::Instant::now() >= deadline {
                panic!("PTY output did not contain {needle:?}: {captured}");
            }
            drop(captured);
            thread::sleep(Duration::from_millis(20));
        }
    };

    wait_for_output("Trust working directory", 0);
    writer.write_all(b"y\r").unwrap();
    writer.flush().unwrap();
    wait_for_output("⏵⏵ Auto · deepseek-flash", 0);
    for mode in ["Ask", "Full Access", "Auto"] {
        let output_start = output.lock().unwrap().len();
        writer.write_all(b"\x1b[Z").unwrap();
        writer.flush().unwrap();
        wait_for_output(&format!("⏵⏵ {mode} · deepseek-flash"), output_start);
    }
    writer.write_all(b"/exit\r\r").unwrap();
    writer.flush().unwrap();

    let deadline = std::time::Instant::now() + INTERACTION_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            panic!("interactive Agent did not exit after /exit");
        }
        thread::sleep(Duration::from_millis(50));
    };
    drop(writer);
    drop(pty.master);
    output_reader.join().unwrap();
    let output = output.lock().unwrap().replace('\r', "");

    assert!(status.success(), "{status:?}\n{output}");
    assert!(output.contains("Trust working directory"), "{output}");
    assert!(
        regex::Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]")
            .unwrap()
            .replace_all(&output, "")
            .contains("⏵⏵ Auto · deepseek-flash"),
        "{output}"
    );
    assert!(UserSettings::in_root(&config_root).is_working_directory_trusted(directory.path()));
}

#[test]
fn interactive_pty_opens_the_subscription_view_and_closes_it_with_escape() {
    let directory = tempfile::tempdir().unwrap();
    let config_root = directory.path().join("config-root");
    trust_working_directory(&config_root, &directory.path().canonicalize().unwrap());
    let pty = native_pty_system()
        .openpty(PtySize {
            rows: 30,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut reader = pty.master.try_clone_reader().unwrap();
    let mut writer = pty.master.take_writer().unwrap();
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_tickloop"));
    command.cwd(directory.path());
    command.env("TICKLOOP_CONFIG_DIR", &config_root);
    command.env("TICKLOOP_AGENT_TUI", "inline");
    command.arg("--no-session-persistence");
    let mut child = pty.slave.spawn_command(command).unwrap();
    drop(pty.slave);
    let output = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let reader_output = output.clone();
    let output_reader = thread::spawn(move || {
        let mut buffer = [0_u8; 4096];
        while let Ok(length) = reader.read(&mut buffer) {
            if length == 0 {
                break;
            }
            reader_output
                .lock()
                .unwrap()
                .push_str(&String::from_utf8_lossy(&buffer[..length]));
        }
    });
    let ansi = regex::Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").unwrap();
    let wait_for_output = |needle: &str, after: usize| {
        let deadline = std::time::Instant::now() + INTERACTION_TIMEOUT;
        loop {
            let captured = output.lock().unwrap();
            if captured
                .get(after..)
                .is_some_and(|suffix| ansi.replace_all(suffix, "").contains(needle))
            {
                return;
            }
            if std::time::Instant::now() >= deadline {
                panic!("PTY output did not contain {needle:?}: {captured}");
            }
            drop(captured);
            thread::sleep(Duration::from_millis(20));
        }
    };

    wait_for_output("⏵⏵ Auto · deepseek-flash", 0);
    let opened_at = output.lock().unwrap().len();
    writer.write_all(b"/subscriptions\r").unwrap();
    writer.flush().unwrap();
    wait_for_output("Tick Subscriptions", opened_at);
    wait_for_output("No Tick Subscription yet", opened_at);
    wait_for_output("Enter cadence · d cancel · Esc close", opened_at);

    let closed_at = output.lock().unwrap().len();
    writer.write_all(b"jkd\x1b").unwrap();
    writer.flush().unwrap();
    wait_for_output("⏵⏵ Auto · deepseek-flash", closed_at);
    assert!(
        !ansi
            .replace_all(&output.lock().unwrap()[closed_at..], "")
            .contains("❯ jkd"),
        "the open view owns its keys instead of typing them into the editor"
    );

    writer.write_all(b"/exit\r\r").unwrap();
    writer.flush().unwrap();
    let deadline = std::time::Instant::now() + INTERACTION_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            panic!("interactive Agent did not exit after /exit");
        }
        thread::sleep(Duration::from_millis(50));
    };
    drop(writer);
    drop(pty.master);
    output_reader.join().unwrap();
    let transcript = output.lock().unwrap().replace('\r', "");
    assert!(status.success(), "{status:?}\n{transcript}");
}

#[test]
fn startup_rejects_invalid_working_directories_before_creating_storage() {
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("missing");
    let file = directory.path().join("file.txt");
    std::fs::write(&file, "not a directory").unwrap();
    for workdir in [&missing, &file] {
        for without_persistence in [false, true] {
            let mut command = trusted_agent_command();
            command.env("TICKLOOP_CONFIG_DIR", directory.path().join("config-root"));
            command.arg("--cwd").arg(workdir).stdin(Stdio::null());
            if without_persistence {
                command.arg("--no-session-persistence");
            }
            let output = command.output().unwrap();
            let error = String::from_utf8(output.stderr).unwrap();
            assert!(!output.status.success(), "invalid --cwd was accepted");
            assert!(error.contains("working directory"), "{error}");
            assert!(error.contains(&workdir.display().to_string()), "{error}");
            assert!(error.contains("omit --cwd"), "{error}");
            assert!(!missing.exists(), "startup created an invalid --cwd");
        }
    }
}

#[test]
fn noninteractive_startup_rejects_an_untrusted_current_directory() {
    let directory = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_tickloop"))
        .env("TICKLOOP_CONFIG_DIR", directory.path().join("config-root"))
        .current_dir(directory.path())
        .env_remove("DEEPSEEK_API_KEY")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(error.contains("not trusted"), "{error}");
    assert!(error.contains("interactively"), "{error}");
    assert!(!directory.path().join(".tickloop").exists());
    let config_root = directory.path().join("config-root");
    assert!(!config_root.exists());
}

#[tokio::test]
async fn product_terminal_defaults_to_auto_and_explains_legacy_mode_names() {
    let directory = tempfile::tempdir().unwrap();
    let mut child = product_async_agent_command()
        .env("TICKLOOP_CONFIG_DIR", directory.path().join("config-root"))
        .arg("--cwd")
        .arg(directory.path())
        .arg("--no-session-persistence")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(
            b"/mode\n/mode ask\n/mode fullAccess\n/mode plan\n/mode default\n/mode manual\n/mode acceptEdits\n/mode dontAsk\n/mode bypass\n",
        )
        .await
        .unwrap();
    child.stdin.take();
    let output = child.wait_with_output().await.unwrap();
    assert!(output.status.success());
    let transcript = String::from_utf8(output.stdout).unwrap();
    assert!(transcript.contains("Execution mode: Auto"), "{transcript}");
    assert!(transcript.contains("Execution mode: Ask"), "{transcript}");
    assert!(
        transcript.contains("Execution mode: Full Access"),
        "{transcript}"
    );
    assert!(transcript.contains("Execution mode: Plan"), "{transcript}");
    assert!(
        transcript.contains("`default` is deprecated"),
        "{transcript}"
    );
    assert!(transcript.contains("replaced by Ask"), "{transcript}");
    assert!(transcript.contains("--deny-on-prompt"), "{transcript}");
    assert!(transcript.contains("external isolation"), "{transcript}");
}

#[test]
fn advanced_prompt_policies_are_explicit_mutually_exclusive_and_warn_when_unrestricted() {
    let directory = tempfile::tempdir().unwrap();
    let conflict = trusted_agent_command()
        .env("TICKLOOP_CONFIG_DIR", directory.path().join("config-root"))
        .arg("--cwd")
        .arg(directory.path())
        .args(["--deny-on-prompt", "--dangerously-skip-permissions"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!conflict.status.success());
    assert!(
        String::from_utf8(conflict.stderr)
            .unwrap()
            .contains("cannot be combined")
    );

    let unrestricted = trusted_agent_command()
        .env("TICKLOOP_CONFIG_DIR", directory.path().join("config-root"))
        .arg("--cwd")
        .arg(directory.path())
        .arg("--dangerously-skip-permissions")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(unrestricted.status.success());
    let warning = String::from_utf8(unrestricted.stderr).unwrap();
    assert!(
        warning.contains("externally isolated environment"),
        "{warning}"
    );
}

#[test]
fn startup_storage_errors_identify_the_config_root_and_available_options() {
    let directory = tempfile::tempdir().unwrap();
    let state = directory.path().join("state");
    std::fs::write(&state, "not a directory").unwrap();
    let output = trusted_agent_command()
        .env("TICKLOOP_CONFIG_DIR", &state)
        .arg("--cwd")
        .arg(directory.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(!output.status.success());
    assert!(error.contains(&state.display().to_string()), "{error}");
    assert!(error.contains("TICKLOOP_CONFIG_DIR"), "{error}");
    assert!(error.contains("--no-session-persistence"), "{error}");
    assert_eq!(std::fs::read_to_string(&state).unwrap(), "not a directory");

    let without_persistence = trusted_agent_command()
        .env("TICKLOOP_CONFIG_DIR", directory.path().join("config-root"))
        .arg("--cwd")
        .arg(directory.path())
        .arg("--no-session-persistence")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        without_persistence.status.success(),
        "{}",
        String::from_utf8_lossy(&without_persistence.stderr)
    );
    assert!(
        !directory
            .path()
            .join("config-root")
            .join("projects")
            .exists()
    );
    assert!(!directory.path().join(".tickloop").exists());
}

#[test]
fn terminal_explains_how_to_start_without_credentials() {
    let output = trusted_agent_command()
        .arg("--help")
        .env_remove("DEEPSEEK_API_KEY")
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("--config"));
    assert!(text.contains("--cwd"));
    assert!(text.contains("/new"));
    assert!(
        !text.contains("/ack-"),
        "removed acknowledgement commands must not be documented: {text}"
    );
    for current in [
        "--continue",
        "--resume [CHAT]",
        "--chat-id CHAT",
        "--fork-session",
        "--no-session-persistence",
        "TICKLOOP_CONFIG_DIR",
        "/clear",
        "/resume CHAT",
    ] {
        assert!(text.contains(current), "missing {current}: {text}");
    }
}

#[test]
fn chat_id_opens_the_exact_chat_identity() {
    let directory = tempfile::tempdir().unwrap();
    let chat_id = "00000000-0000-4000-8000-000000000123";
    let output = Command::new(env!("CARGO_BIN_EXE_tickloop"))
        .env("TICKLOOP_CONFIG_DIR", directory.path().join("config-root"))
        .arg("--working-directory-is-trusted")
        .arg("--cwd")
        .arg(directory.path())
        .arg("--chat-id")
        .arg(chat_id)
        .stdin(Stdio::null())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains(&format!("Chat: {chat_id}")));
}

#[tokio::test]
async fn piped_provider_model_and_settings_menus_apply_numbered_choices() {
    let directory = tempfile::tempdir().unwrap();
    let mut child = trusted_async_agent_command()
        .env("TICKLOOP_CONFIG_DIR", directory.path().join("config-root"))
        .arg("--cwd")
        .arg(directory.path())
        .arg("--no-session-persistence")
        .env("DEEPSEEK_API_KEY", "fixture-menu-key")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            b"/model\n1\n1\n/model\n1\ncancel\ncancel\n/settings\n4\n2\n/config\ncancel\n/exit\n",
        )
        .await
        .unwrap();
    let output = tokio::time::timeout(INTERACTION_TIMEOUT, child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    let transcript = String::from_utf8(output.stdout).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        transcript.contains("DeepSeek-V4.1-Flash    Thinking"),
        "{transcript}"
    );
    assert!(
        transcript.contains(
            "Current model: DeepSeek API · DeepSeek-V4.1-Flash (deepseek-flash) · Messages"
        ),
        "{transcript}"
    );
    assert!(
        transcript.contains("Execution mode: Ask. Explicit deny rules remain effective."),
        "{transcript}"
    );
    assert!(
        transcript.contains("Selection cancelled; configuration unchanged."),
        "{transcript}"
    );
    assert!(!transcript.contains("Cannot"), "{transcript}");
    assert!(
        !transcript.contains('\u{1b}'),
        "pipe output must not use terminal controls"
    );
}

#[tokio::test]
async fn effort_picker_saves_a_supported_level_without_model_credentials() {
    let directory = tempfile::tempdir().unwrap();
    let config_root = directory.path().join("config-root");
    let mut child = trusted_async_agent_command()
        .env("TICKLOOP_CONFIG_DIR", &config_root)
        .arg("--cwd")
        .arg(directory.path())
        .arg("--no-session-persistence")
        .env_remove("DEEPSEEK_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"/thinking medium\n/effort\n4\n/thinking\ncancel\n/exit\n")
        .await
        .unwrap();
    let output = tokio::time::timeout(INTERACTION_TIMEOUT, child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    let transcript = String::from_utf8(output.stdout).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        transcript.contains("Thinking levels for deepseek-"),
        "{transcript}"
    );
    assert!(transcript.contains("low, high, max"), "{transcript}");
    assert!(
        !transcript.contains("off · disable thinking"),
        "{transcript}"
    );
    assert!(!transcript.contains("medium · current"), "{transcript}");
    assert!(transcript.contains("Thinking: max"), "{transcript}");
    assert!(transcript.contains("max · current"), "{transcript}");
    let saved: Value =
        serde_json::from_slice(&std::fs::read(config_root.join("settings.json")).unwrap()).unwrap();
    assert_eq!(saved["effortLevel"], "max");
}

#[tokio::test]
async fn thinking_toggle_is_separate_from_effort() {
    let directory = tempfile::tempdir().unwrap();
    let config_root = directory.path().join("config-root");
    let mut child = trusted_async_agent_command()
        .env("TICKLOOP_CONFIG_DIR", &config_root)
        .arg("--cwd")
        .arg(directory.path())
        .arg("--no-session-persistence")
        .env_remove("DEEPSEEK_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"/settings\n2\n/exit\n")
        .await
        .unwrap();
    let output = tokio::time::timeout(INTERACTION_TIMEOUT, child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let transcript = String::from_utf8(output.stdout).unwrap();
    assert!(transcript.contains("Thinking: Off"), "{transcript}");
    let saved: Value =
        serde_json::from_slice(&std::fs::read(config_root.join("settings.json")).unwrap()).unwrap();
    assert_eq!(saved["alwaysThinkingEnabled"], false);
    assert!(saved.get("effortLevel").is_none());
}

#[tokio::test]
async fn piped_effort_controls_request_and_prints_returned_thinking() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_request(&mut socket).await;
        let body = [
            json!({"type":"message_start","message":{"id":"thinking-id","type":"message","role":"assistant","content":[],"usage":{"input_tokens":7,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Compare both values."}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"The second value is larger."}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":9}}),
            json!({"type":"message_stop"}),
        ]
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>();
        socket
            .write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes())
            .await
            .unwrap();
        socket.write_all(body.as_bytes()).await.unwrap();
        request
    });
    let directory = tempfile::tempdir().unwrap();
    let mut config = tickloop_agent::model::cards::ModelCatalog::defaults()
        .get("deepseek-api", "deepseek-flash-messages")
        .unwrap();
    config.base_url = format!("http://{address}");
    config.api_key_env.clear();
    config.max_output_tokens = 256;
    config.card.as_mut().unwrap().max_output_tokens = 256;
    std::fs::write(
        directory.path().join("model.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();

    let output = run_with_config(
        directory.path(),
        &["--no-session-persistence"],
        "/effort max\nCompare\n",
    )
    .await;
    let transcript = String::from_utf8(output.stdout).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        transcript.contains("Thinking: Compare both values."),
        "{transcript}"
    );
    assert!(
        transcript.contains("Assistant: The second value is larger."),
        "{transcript}"
    );
    let request = server.await.unwrap();
    assert_eq!(request["thinking"], json!({"type":"enabled"}));
    assert_eq!(request["output_config"]["effort"], "max");
}

#[tokio::test]
async fn settings_preview_requires_confirmation_before_saving_nerd_font_glyphs() {
    let directory = tempfile::tempdir().unwrap();
    let config_root = directory.path().join("config-root");
    let mut child = trusted_async_agent_command()
        .env("TICKLOOP_CONFIG_DIR", &config_root)
        .arg("--cwd")
        .arg(directory.path())
        .arg("--no-session-persistence")
        .env_remove("DEEPSEEK_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"/settings\n9\n2\n1\n/exit\n")
        .await
        .unwrap();
    let output = tokio::time::timeout(INTERACTION_TIMEOUT, child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let transcript = String::from_utf8(output.stdout).unwrap();
    assert!(transcript.contains("Unicode preview:"), "{transcript}");
    assert!(transcript.contains("Nerd Font preview:"), "{transcript}");
    assert!(transcript.contains("Confirm glyph set"), "{transcript}");
    let saved: Value =
        serde_json::from_slice(&std::fs::read(config_root.join("settings.json")).unwrap()).unwrap();
    assert_eq!(saved["glyphSet"], "nerd-font");
}

#[tokio::test]
async fn piped_bang_input_stays_a_model_prompt_and_cannot_enter_direct_shell_mode() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_request(&mut socket).await;
        let body = messages_stream("The literal prompt was received.");
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        socket.write_all(header.as_bytes()).await.unwrap();
        socket.write_all(body.as_bytes()).await.unwrap();
        request
    });
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("must-not-exist");
    let config_path = directory.path().join("model.json");
    std::fs::write(
        &config_path,
        serde_json::to_vec(&json!({
            "name":"local fixture", "protocol":"messages", "base_url":format!("http://{address}"),
            "model":"fixture", "api_key_env":"", "max_output_tokens":256
        }))
        .unwrap(),
    )
    .unwrap();
    let mut child = trusted_async_agent_command()
        .env("TICKLOOP_CONFIG_DIR", directory.path().join("config-root"))
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "--cwd",
            directory.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"!touch must-not-exist\n")
        .await
        .unwrap();
    let output = tokio::time::timeout(INTERACTION_TIMEOUT, child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    let request = tokio::time::timeout(INTERACTION_TIMEOUT, server)
        .await
        .unwrap()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !marker.exists(),
        "piped input executed as a direct shell command"
    );
    assert!(
        request["messages"]
            .to_string()
            .contains("!touch must-not-exist")
    );
}

#[tokio::test]
async fn piped_follow_up_keeps_context_and_waits_for_both_replies_before_eof_exit() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for answer in ["Remembered violet.", "The word is violet."] {
            let (mut socket, _) = listener.accept().await.unwrap();
            requests.push(read_request(&mut socket).await);
            let body = messages_stream(answer);
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(header.as_bytes()).await.unwrap();
            socket.write_all(body.as_bytes()).await.unwrap();
        }
        requests
    });
    let directory = tempfile::tempdir().unwrap();
    let config_path = directory.path().join("model.json");
    std::fs::write(
        &config_path,
        serde_json::to_vec(&json!({
            "name":"local fixture", "protocol":"messages", "base_url":format!("http://{address}"),
            "model":"fixture", "api_key_env":"", "max_output_tokens":256
        }))
        .unwrap(),
    )
    .unwrap();
    let mut child = trusted_async_agent_command()
        .env("TICKLOOP_CONFIG_DIR", directory.path().join("config-root"))
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "--cwd",
            directory.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    input
        .write_all(b"Remember the word violet.\nWhat was the word?\n")
        .await
        .unwrap();
    drop(input);
    let output = tokio::time::timeout(INTERACTION_TIMEOUT, child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("Messages"));
    assert!(text.contains("Remembered violet."));
    assert!(text.contains("The word is violet."));
    assert_eq!(text.matches("Reply completed").count(), 2);
    let requests = tokio::time::timeout(INTERACTION_TIMEOUT, server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(requests[1]["messages"].as_array().unwrap().len(), 3);
    assert_eq!(
        requests[1]["messages"][0]["content"][0]["text"],
        "Remember the word violet."
    );
    assert_eq!(
        requests[1]["messages"][1]["content"][0]["text"],
        "Remembered violet."
    );
}

#[tokio::test]
async fn piped_branch_switches_to_an_independent_child_with_the_source_context() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for answer in ["Source answer.", "Child answer."] {
            let (mut socket, _) = listener.accept().await.unwrap();
            requests.push(read_request(&mut socket).await);
            let body = messages_stream(answer);
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(header.as_bytes()).await.unwrap();
            socket.write_all(body.as_bytes()).await.unwrap();
        }
        requests
    });
    let directory = tempfile::tempdir().unwrap();
    let config_path = directory.path().join("model.json");
    std::fs::write(
        &config_path,
        serde_json::to_vec(&json!({
            "name":"local fixture", "protocol":"messages", "base_url":format!("http://{address}"),
            "model":"fixture", "api_key_env":"", "max_output_tokens":256
        }))
        .unwrap(),
    )
    .unwrap();
    let mut child = trusted_async_agent_command()
        .env("TICKLOOP_CONFIG_DIR", directory.path().join("config-root"))
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "--cwd",
            directory.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    let mut stderr = child.stderr.take().unwrap();
    let mut transcript = Vec::new();

    input.write_all(b"Source prompt.\n").await.unwrap();
    tokio::time::timeout(INTERACTION_TIMEOUT, async {
        loop {
            let line = output.next_line().await.unwrap().unwrap();
            let completed = line.starts_with("Reply completed");
            transcript.push(line);
            if completed {
                break;
            }
        }
    })
    .await
    .expect("source reply did not finish");
    let source_chat_id = transcript
        .iter()
        .find_map(|line| line.strip_prefix("Chat: "))
        .expect("terminal did not print the source Chat ID")
        .to_owned();

    input
        .write_all("/branch 研究 β\n".as_bytes())
        .await
        .unwrap();
    let branch_confirmation = tokio::time::timeout(INTERACTION_TIMEOUT, async {
        loop {
            let line = output.next_line().await.unwrap().unwrap();
            transcript.push(line.clone());
            if line.starts_with("Branched conversation") {
                break line;
            }
        }
    })
    .await
    .expect("branch confirmation did not arrive");
    assert!(branch_confirmation.contains("研究 β (Branch)"));
    assert!(branch_confirmation.contains(&source_chat_id));
    let child_chat_id = branch_confirmation
        .split_once("new Chat ")
        .and_then(|(_, suffix)| suffix.split_once('.'))
        .map(|(id, _)| id)
        .expect("branch confirmation did not include the child Chat ID");
    assert_ne!(child_chat_id, source_chat_id);

    input
        .write_all(b"/resume\ncancel\nChild prompt.\n")
        .await
        .unwrap();
    drop(input);
    while let Some(line) = output.next_line().await.unwrap() {
        transcript.push(line);
    }
    let status = tokio::time::timeout(INTERACTION_TIMEOUT, child.wait())
        .await
        .expect("terminal did not exit after EOF")
        .unwrap();
    let mut error = String::new();
    stderr.read_to_string(&mut error).await.unwrap();
    assert!(status.success(), "{error}");
    assert!(transcript.iter().any(|line| line.contains("Child answer.")));
    assert!(
        transcript.iter().any(|line| {
            line.starts_with("  1. Source prompt.") && line.contains(&source_chat_id)
        }),
        "/resume must offer the source Chat: {transcript:?}"
    );
    assert!(
        !transcript
            .iter()
            .any(|line| line.starts_with("  ") && line.contains(child_chat_id)),
        "/resume must not offer the Chat that is already open"
    );

    let requests = tokio::time::timeout(INTERACTION_TIMEOUT, server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(requests.len(), 2);
    let child_messages = requests[1]["messages"].to_string();
    assert!(child_messages.contains("Source prompt."));
    assert!(child_messages.contains("Source answer."));
    assert!(child_messages.contains("Child prompt."));
    assert!(!child_messages.contains("/branch"));
    assert!(!child_messages.contains("研究 β"));
}

#[tokio::test]
async fn terminal_reports_failed_model_work_as_a_failed_process() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        socket.write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 14\r\nConnection: close\r\n\r\nserver-private").await.unwrap();
    });
    let directory = tempfile::tempdir().unwrap();
    let config_path = directory.path().join("model.json");
    std::fs::write(
        &config_path,
        serde_json::to_vec(&json!({
            "name":"local fixture", "protocol":"messages", "base_url":format!("http://{address}"),
            "model":"fixture", "api_key_env":"", "max_output_tokens":256
        }))
        .unwrap(),
    )
    .unwrap();
    let mut child = trusted_async_agent_command()
        .env("TICKLOOP_CONFIG_DIR", directory.path().join("config-root"))
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "--cwd",
            directory.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"Hello\n")
        .await
        .unwrap();
    let output = tokio::time::timeout(INTERACTION_TIMEOUT, child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(!output.status.success());
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(text.contains("Failed:"));
    assert!(!text.contains("Reply completed"));
    assert!(!text.contains("server-private"));
    server.await.unwrap();
}

#[test]
fn malformed_configuration_is_actionable_without_echoing_its_contents() {
    let directory = tempfile::tempdir().unwrap();
    let config_path = directory.path().join("broken.json");
    std::fs::write(&config_path, "{\"api_key\":\"private-value\"").unwrap();
    let output = trusted_agent_command()
        .env("TICKLOOP_CONFIG_DIR", directory.path().join("config-root"))
        .args(["--config", config_path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(error.contains("model configuration JSON"));
    assert!(error.contains("examples/providers/deepseek-api.yaml"));
    assert!(!error.contains("private-value"));
}

#[tokio::test]
async fn restart_lists_saved_chats_without_running_until_manual_resume_and_input() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for answer in ["Saved the word cedar.", "The word was cedar."] {
            let (mut socket, _) = listener.accept().await.unwrap();
            requests.push(read_request(&mut socket).await);
            let body = messages_stream(answer);
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
            socket.write_all(body.as_bytes()).await.unwrap();
        }
        requests
    });
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("model.json"),
        serde_json::to_vec(&json!({
            "name":"local fixture", "protocol":"messages", "base_url":format!("http://{address}"),
            "model":"fixture", "api_key_env":"", "max_output_tokens":256
        }))
        .unwrap(),
    )
    .unwrap();
    let first = run_with_config(directory.path(), &[], "Remember cedar.\n").await;
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first_text = String::from_utf8(first.stdout).unwrap();
    let chat = first_text
        .lines()
        .find_map(|line| line.strip_prefix("Chat: "))
        .unwrap();
    let goal = first_text
        .lines()
        .find_map(|line| line.strip_prefix("Goal: "))
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap();
    let second = run_with_config(directory.path(), &[], "/resume\ncancel\n").await;
    assert!(second.status.success());
    let second_text = String::from_utf8(second.stdout).unwrap();
    assert!(second_text.contains("Resume Chat"), "{second_text}");
    assert!(
        second_text
            .lines()
            .any(|line| line.starts_with("  1. Remember cedar.") && line.contains(chat)),
        "{second_text}"
    );
    assert!(!second_text.contains("Running"));
    let rejected =
        run_with_config(directory.path(), &[], &format!("/resume {chat} {goal}\n")).await;
    assert!(rejected.status.success());
    assert!(
        String::from_utf8(rejected.stdout)
            .unwrap()
            .contains("use /resume [CHAT]")
    );
    let third = run_with_config(
        directory.path(),
        &[],
        &format!("/resume {chat}\nWhat was the word?\n"),
    )
    .await;
    assert!(
        third.status.success(),
        "{}",
        String::from_utf8_lossy(&third.stderr)
    );
    let third_text = String::from_utf8(third.stdout).unwrap();
    assert!(third_text.contains("The word was cedar."));
    assert!(third_text.contains(&format!("Goal: {goal}")));
    let requests = tokio::time::timeout(INTERACTION_TIMEOUT, server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        requests[1]["messages"][0]["content"][0]["text"],
        "Remember cedar."
    );
    assert_eq!(
        requests[1]["messages"][1]["content"][0]["text"],
        "Saved the word cedar."
    );
}

#[tokio::test]
async fn resuming_history_does_not_restore_a_failed_partial_reply() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        let mut body = messages_stream("保留下来的失败片段")
            .split("\n\n")
            .take(3)
            .map(|event| format!("{event}\n\n"))
            .collect::<String>();
        let delta = json!({"type":"content_block_delta", "index":0, "delta":{"type":"text_delta", "text":"."}});
        body.push_str(&format!("data: {delta}\n\n").repeat(1100));
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
        socket.write_all(body.as_bytes()).await.unwrap();
    });
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("model.json"),
        serde_json::to_vec(&json!({
            "name":"fixture", "protocol":"messages", "base_url":format!("http://{address}"),
            "model":"fixture", "api_key_env":"", "max_output_tokens":256
        }))
        .unwrap(),
    )
    .unwrap();
    let first = run_with_config(directory.path(), &[], "Please answer\n").await;
    assert!(!first.status.success());
    let first_text = String::from_utf8(first.stdout).unwrap();
    assert!(first_text.contains("Failed:"), "{first_text}");
    let chat = first_text
        .lines()
        .find_map(|line| line.strip_prefix("Chat: "))
        .unwrap();
    server.await.unwrap();
    for flags in [vec!["--resume", chat], Vec::new()] {
        let command = if flags.is_empty() {
            format!("/resume {chat}\n")
        } else {
            String::new()
        };
        let output = run_with_config(directory.path(), &flags, &command).await;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(!text.contains("保留下来的失败片段"), "{text}");
        assert!(!text.contains("Assistant (incomplete):"), "{text}");
        assert!(
            !text.contains("Running"),
            "history must not start a model request: {text}"
        );
        assert!(text.contains(&format!("Chat: {chat}")), "{text}");
    }
}

#[tokio::test]
async fn terminal_accepts_a_follow_up_while_the_first_reply_is_still_running() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (release, wait) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let mut wait = Some(wait);
        for answer in ["first reply", "second reply"] {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request(&mut socket).await;
            if let Some(wait) = wait.take() {
                wait.await.unwrap();
            }
            let body = messages_stream(answer);
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
            socket.write_all(body.as_bytes()).await.unwrap();
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("model.json");
    std::fs::write(&config, serde_json::to_vec(&json!({"name":"fixture","protocol":"messages","base_url":format!("http://{address}"),"model":"fixture","api_key_env":"","max_output_tokens":256})).unwrap()).unwrap();
    let mut child = trusted_async_agent_command()
        .env("TICKLOOP_CONFIG_DIR", directory.path().join("config-root"))
        .arg("--config")
        .arg(config)
        .arg("--no-session-persistence")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    input.write_all(b"First task\nFollow up\n").await.unwrap();
    let mut accepted = 0;
    tokio::time::timeout(INTERACTION_TIMEOUT, async {
        while accepted < 2 {
            let line = output.next_line().await.unwrap().unwrap();
            if line.starts_with("Accepted:") {
                accepted += 1;
            }
        }
    })
    .await
    .expect("both inputs must be accepted before the first model reply completes");
    release.send(()).unwrap();
    drop(input);
    let mut completed = 0;
    tokio::time::timeout(INTERACTION_TIMEOUT, async {
        while let Some(line) = output.next_line().await.unwrap() {
            if line.starts_with("Reply completed") {
                completed += 1;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(completed, 2);
    assert!(child.wait().await.unwrap().success());
    server.await.unwrap();
}

#[tokio::test]
async fn terminal_waits_for_explicit_permission_before_reading_an_external_file() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let external_path = outside.path().join("note.txt");
    std::fs::write(&external_path, "approved external note").unwrap();
    let tool_input = json!({"file_path":external_path});
    let server = tokio::spawn(async move {
        let mut final_request = Value::Null;
        for body in [
            tool_stream("Read", tool_input),
            messages_stream("Read the approved external note."),
        ] {
            let (mut socket, _) = listener.accept().await.unwrap();
            final_request = read_request(&mut socket).await;
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
            socket.write_all(body.as_bytes()).await.unwrap();
        }
        final_request
    });
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("model.json");
    std::fs::write(&config, serde_json::to_vec(&json!({"name":"fixture","protocol":"messages","base_url":format!("http://{address}"),"model":"fixture","api_key_env":"","max_output_tokens":256})).unwrap()).unwrap();
    let mut child = trusted_async_agent_command()
        .env("TICKLOOP_CONFIG_DIR", directory.path().join("config-root"))
        .arg("--config")
        .arg(config)
        .arg("--cwd")
        .arg(directory.path())
        .arg("--no-session-persistence")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    input.write_all(b"Read the external note\n").await.unwrap();
    let request_id = tokio::time::timeout(INTERACTION_TIMEOUT, async {
        loop {
            let line = output.next_line().await.unwrap().unwrap();
            if let Some(request) = line.strip_prefix("Permission request: ") {
                break request.split_whitespace().next().unwrap().to_owned();
            }
        }
    })
    .await
    .expect("terminal must show an actionable permission request");
    input
        .write_all(format!("/allow {request_id}\n").as_bytes())
        .await
        .unwrap();
    drop(input);
    let mut transcript = String::new();
    tokio::time::timeout(INTERACTION_TIMEOUT, async {
        while let Some(line) = output.next_line().await.unwrap() {
            transcript.push_str(&line);
        }
    })
    .await
    .unwrap();
    assert!(child.wait().await.unwrap().success());
    assert!(transcript.contains("Read the approved external note."));
    let request = server.await.unwrap();
    assert!(
        request["messages"]
            .to_string()
            .contains("approved external note")
    );
}

#[tokio::test]
async fn terminal_can_stop_a_running_goal_and_show_its_stopped_state() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (started, started_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        started.send(()).unwrap();
        std::future::pending::<()>().await;
    });
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("model.json");
    std::fs::write(&config, serde_json::to_vec(&json!({"name":"fixture","protocol":"messages","base_url":format!("http://{address}"),"model":"fixture","api_key_env":"","max_output_tokens":256})).unwrap()).unwrap();
    let mut child = trusted_async_agent_command()
        .env("TICKLOOP_CONFIG_DIR", directory.path().join("config-root"))
        .arg("--config")
        .arg(config)
        .arg("--no-session-persistence")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    input.write_all(b"Work until I stop you\n").await.unwrap();
    tokio::time::timeout(INTERACTION_TIMEOUT, started_rx)
        .await
        .unwrap()
        .unwrap();
    input.write_all(b"/stop\n/loops\n").await.unwrap();
    drop(input);
    let output = tokio::time::timeout(INTERACTION_TIMEOUT, child.wait_with_output())
        .await
        .expect("stop must cancel the request and allow EOF to exit")
        .unwrap();
    server.abort();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("Stopped goal"));
    assert!(text.contains("Stopped"));
    assert!(!text.contains("Reply completed"));
}

#[tokio::test]
async fn terminal_can_choose_accept_edits_with_an_explicit_write_rule_before_the_first_task() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for body in [
            tool_stream(
                "Write",
                json!({"file_path":"result.txt","content":"terminal permission mode"}),
            ),
            messages_stream("File saved."),
        ] {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request(&mut socket).await;
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
            socket.write_all(body.as_bytes()).await.unwrap();
        }
    });
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("model.json"),serde_json::to_vec(&json!({"name":"fixture","protocol":"messages","base_url":format!("http://{address}"),"model":"fixture","api_key_env":"","max_output_tokens":256})).unwrap()).unwrap();
    let output = run_with_config(
        directory.path(),
        &["--no-session-persistence"],
        "/rule allow Write\n/mode acceptEdits\nWrite the file\n",
    )
    .await;
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("result.txt")).unwrap(),
        "terminal permission mode"
    );
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("Execution mode: Ask")
    );
    server.await.unwrap();
}

#[tokio::test]
async fn terminal_directory_controls_apply_to_subsequent_file_operations() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("data")).unwrap();
    std::fs::write(
        directory.path().join("data/note.txt"),
        "main directory marker",
    )
    .unwrap();
    let outside = tempfile::tempdir().unwrap();
    let outside_file = outside.path().join("note.txt");
    std::fs::write(&outside_file, "external directory marker").unwrap();
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for (index, mut body) in [
            tool_stream("Read", json!({"file_path":"note.txt"})),
            tool_stream("Read", json!({"file_path":outside_file})),
            messages_stream("Both directories were read."),
        ]
        .into_iter()
        .enumerate()
        {
            body = body.replace("tool-fixture", &format!("tool-{index}"));
            let (mut socket, _) = listener.accept().await.unwrap();
            requests.push(read_request(&mut socket).await);
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
            socket.write_all(body.as_bytes()).await.unwrap();
        }
        requests
    });
    std::fs::write(directory.path().join("model.json"),serde_json::to_vec(&json!({"name":"fixture","protocol":"messages","base_url":format!("http://{address}"),"model":"fixture","api_key_env":"","max_output_tokens":256})).unwrap()).unwrap();
    let commands = format!(
        "/cwd data\n/add-dir {}\nRead both notes\n",
        outside.path().display()
    );
    let output = run_with_config(directory.path(), &["--no-session-persistence"], &commands).await;
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let requests = server.await.unwrap();
    let final_history = requests[2]["messages"].to_string();
    assert!(final_history.contains("main directory marker"));
    assert!(final_history.contains("external directory marker"));
}

#[tokio::test]
async fn no_session_persistence_writes_no_files_into_the_working_directory() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for (index, mut body) in [
            tool_stream(
                "Write",
                json!({"file_path":"report.txt","content":"quarterly report\n"}),
            ),
            tool_stream("Bash", json!({"command":"printf shell-output"})),
            messages_stream("Report written and checked."),
        ]
        .into_iter()
        .enumerate()
        {
            body = body.replace("tool-fixture", &format!("tool-{index}"));
            let (mut socket, _) = listener.accept().await.unwrap();
            requests.push(read_request(&mut socket).await);
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
            socket.write_all(body.as_bytes()).await.unwrap();
        }
        requests
    });
    let directory = tempfile::tempdir().unwrap();
    let workdir = directory.path().join("work");
    std::fs::create_dir(&workdir).unwrap();
    let config_path = directory.path().join("model.json");
    std::fs::write(&config_path,serde_json::to_vec(&json!({"name":"fixture","protocol":"messages","base_url":format!("http://{address}"),"model":"fixture","api_key_env":"","max_output_tokens":256})).unwrap()).unwrap();
    let mut child = trusted_async_agent_command()
        .env("TICKLOOP_CONFIG_DIR", directory.path().join("config-root"))
        .arg("--config")
        .arg(&config_path)
        .arg("--cwd")
        .arg(&workdir)
        .arg("--no-session-persistence")
        .arg("--dangerously-skip-permissions")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"/rule allow *\n/mode acceptEdits\nWrite the report and check the shell\n")
        .await
        .unwrap();
    let output = tokio::time::timeout(INTERACTION_TIMEOUT, child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let requests = server.await.unwrap();
    let final_history = requests[2]["messages"].to_string();
    assert!(final_history.contains("shell-output"), "{final_history}");
    let entries = std::fs::read_dir(&workdir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(entries, ["report.txt"]);
    assert!(!directory.path().join("config-root/projects").exists());
}

#[tokio::test]
async fn terminal_deny_rule_still_wins_over_an_explicit_wildcard_allow() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        let body = tool_stream(
            "Write",
            json!({"file_path":"denied.txt","content":"must not be written"}),
        );
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
        socket.write_all(body.as_bytes()).await.unwrap();
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        socket
            .write_all(
                b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
    });
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("model.json"),serde_json::to_vec(&json!({"name":"fixture","protocol":"messages","base_url":format!("http://{address}"),"model":"fixture","api_key_env":"","max_output_tokens":256})).unwrap()).unwrap();
    let output = run_with_config(
        directory.path(),
        &["--no-session-persistence"],
        "/rule deny Write\n/rule allow *\n/rules\n/mode acceptEdits\nWrite the file\n",
    )
    .await;
    assert!(!output.status.success());
    assert!(
        !directory.path().join("denied.txt").exists(),
        "explicit deny rule must prevent the actual write"
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("Rule added:"));
    assert!(text.contains("authorization rule denied"));
    server.await.unwrap();
}

#[tokio::test]
async fn terminal_auto_mode_reviews_with_the_current_chat_model_by_default() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let external = tempfile::tempdir().unwrap();
    let checked_path = external.path().join("checked.txt");
    let requested_path = checked_path.clone();
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for body in [
            tool_stream(
                "Write",
                json!({"file_path":requested_path,"content":"checked output"}),
            ),
            messages_stream(&support::auto_findings(false).to_string()),
            messages_stream("Checked output saved."),
        ] {
            let (mut socket, _) = listener.accept().await.unwrap();
            requests.push(read_request(&mut socket).await);
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
            socket.write_all(body.as_bytes()).await.unwrap();
        }
        requests
    });
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("model.json");
    std::fs::write(&config,serde_json::to_vec(&json!({"name":"fixture","protocol":"messages","base_url":format!("http://{address}"),"model":"fixture","api_key_env":"","max_output_tokens":256})).unwrap()).unwrap();
    let output = run_with_config(
        directory.path(),
        &["--no-session-persistence"],
        "/mode auto\nWrite checked.txt\n",
    )
    .await;
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&checked_path).unwrap(),
        "checked output"
    );
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("Auto review: Allow")
    );
    let requests = server.await.unwrap();
    assert!(
        requests[1]
            .get("tools")
            .is_none_or(|tools| tools.as_array().is_some_and(Vec::is_empty))
    );
    assert!(
        requests[1]["system"]
            .to_string()
            .contains("complete Auto policy")
    );
}

#[test]
fn legacy_independent_reviewer_flags_report_the_permissions_migration() {
    for flag in ["--auto-reviewer-config", "--permission-checker-config"] {
        let output = Command::new(env!("CARGO_BIN_EXE_tickloop"))
            .args([flag, "legacy.json"])
            .output()
            .unwrap();
        assert!(!output.status.success(), "{flag} should be rejected");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("/permissions") && stderr.contains("Auto mode"),
            "{stderr}"
        );
    }
}

#[tokio::test]
async fn terminal_can_switch_model_configuration_for_follow_up_input() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for answer in ["first model answer", "second model answer"] {
            let (mut socket, _) = listener.accept().await.unwrap();
            requests.push(read_request(&mut socket).await);
            let body = messages_stream(answer);
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
            socket.write_all(body.as_bytes()).await.unwrap();
        }
        requests
    });
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("model.json");
    let second_config = directory.path().join("second-model.json");
    for (path, model) in [(&config, "first-model"), (&second_config, "second-model")] {
        std::fs::write(path,serde_json::to_vec(&json!({"name":model,"protocol":"messages","base_url":format!("http://{address}"),"model":model,"api_key_env":"","max_output_tokens":256})).unwrap()).unwrap();
    }
    let mut child = trusted_async_agent_command()
        .env("TICKLOOP_CONFIG_DIR", directory.path().join("config-root"))
        .arg("--config")
        .arg(config)
        .arg("--no-session-persistence")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    input.write_all(b"First task\n").await.unwrap();
    tokio::time::timeout(INTERACTION_TIMEOUT, async {
        while !output
            .next_line()
            .await
            .unwrap()
            .unwrap()
            .starts_with("Reply completed")
        {}
    })
    .await
    .unwrap();
    input
        .write_all(format!("/model {}\nFollow up\n", second_config.display()).as_bytes())
        .await
        .unwrap();
    drop(input);
    tokio::time::timeout(INTERACTION_TIMEOUT, async {
        while output.next_line().await.unwrap().is_some() {}
    })
    .await
    .unwrap();
    assert!(child.wait().await.unwrap().success());
    let requests = server.await.unwrap();
    assert_eq!(requests[0]["model"], "first-model");
    assert_eq!(requests[1]["model"], "second-model");
    assert!(
        requests[1]["messages"]
            .to_string()
            .contains("first model answer")
    );
}

/// 照 Claude Code：`/model` 与持久规则写入 `<root>/settings.json`，
/// 重启后不带 `--config` 时生效；带 `--config` 时启动参数优先。
#[tokio::test]
async fn saved_model_and_rules_apply_after_restart_unless_config_is_given() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(answer_requests(
        listener,
        vec![
            "Answered by the saved model.",
            "Answered by the explicit model.",
        ],
    ));
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("model.json");
    let second_config = directory.path().join("second-model.json");
    for (path, model) in [(&config, "first-model"), (&second_config, "second-model")] {
        std::fs::write(path,serde_json::to_vec(&json!({"name":model,"protocol":"messages","base_url":format!("http://{address}"),"model":model,"api_key_env":"","max_output_tokens":256})).unwrap()).unwrap();
    }
    let run = |flags: Vec<String>, text: String| {
        let directory = directory.path().to_path_buf();
        async move {
            let mut child = trusted_async_agent_command()
                .env("TICKLOOP_CONFIG_DIR", directory.join("config-root"))
                .arg("--cwd")
                .arg(&directory)
                .args(flags)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(text.as_bytes())
                .await
                .unwrap();
            tokio::time::timeout(INTERACTION_TIMEOUT, child.wait_with_output())
                .await
                .unwrap()
                .unwrap()
        }
    };

    let selecting = run(
        vec!["--config".into(), config.display().to_string()],
        format!(
            "/model {}\n/rule allow Read persistent\n/rule allow Glob session\n/exit\n",
            second_config.display()
        ),
    )
    .await;
    assert!(
        selecting.status.success(),
        "{}",
        String::from_utf8_lossy(&selecting.stderr)
    );
    let settings: Value = serde_json::from_slice(
        &std::fs::read(directory.path().join("config-root").join("settings.json")).unwrap(),
    )
    .unwrap();
    assert!(settings.get("model").is_none(), "{settings}");
    assert_eq!(settings["defaultModel"]["model"], "second-model");
    let selected_provider = settings["defaultModel"]["provider"].as_str().unwrap();
    assert!(
        directory
            .path()
            .join("config-root/providers")
            .join(format!("{selected_provider}.yaml"))
            .is_file()
    );
    assert!(settings.get("contextWindowTokens").is_none());

    let restarted = run(Vec::new(), "/rules\nHello after restart\n".into()).await;
    assert!(
        restarted.status.success(),
        "{}",
        String::from_utf8_lossy(&restarted.stderr)
    );
    let text = String::from_utf8(restarted.stdout).unwrap();
    assert!(text.contains("\"Read\""), "{text}");
    assert!(
        !text.contains("\"Glob\""),
        "session rules must not be saved: {text}"
    );
    assert!(text.contains("Answered by the saved model."), "{text}");

    let explicit = run(
        vec!["--config".into(), config.display().to_string()],
        "Hello with an explicit configuration\n".into(),
    )
    .await;
    assert!(
        explicit.status.success(),
        "{}",
        String::from_utf8_lossy(&explicit.stderr)
    );
    let requests = tokio::time::timeout(INTERACTION_TIMEOUT, server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(requests[0]["model"], "second-model");
    assert_eq!(requests[1]["model"], "first-model");
}

#[tokio::test]
async fn explicit_provider_yaml_starts_with_an_invalid_saved_model_selection() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(answer_requests(listener, vec!["Explicit model answer."]));
    let directory = tempfile::tempdir().unwrap();
    let config_root = directory.path().join("config-root");
    std::fs::create_dir_all(&config_root).unwrap();
    std::fs::write(
        config_root.join("settings.json"),
        r#"{"defaultModel":{"provider":"removed-provider","model":"removed-model"}}"#,
    )
    .unwrap();
    let config = directory.path().join("explicit.yaml");
    std::fs::write(
        &config,
        format!(
            "id: explicit-provider\ndisplay_name: Explicit provider\nauth: api_key_env\napi_key_env: EXPLICIT_TEST_KEY\nendpoints:\n  messages: http://{address}\nmodels:\n  - id: explicit-model\n    service_model_id: explicit-model\n    display_name: Explicit model\n    adapter: messages\n"
        ),
    )
    .unwrap();
    let mut child = trusted_async_agent_command()
        .env("TICKLOOP_CONFIG_DIR", &config_root)
        .env("EXPLICIT_TEST_KEY", "test-key")
        .arg("--no-session-persistence")
        .arg("--cwd")
        .arg(directory.path())
        .arg("--config")
        .arg(&config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"Hello from explicit configuration\n")
        .await
        .unwrap();
    let output = tokio::time::timeout(INTERACTION_TIMEOUT, child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("Explicit model answer."), "{text}");
    assert!(text.contains("unknown provider/model"), "{text}");
    let requests = tokio::time::timeout(INTERACTION_TIMEOUT, server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(requests[0]["model"], "explicit-model");
}

#[tokio::test]
async fn selected_external_provider_yaml_is_saved_as_a_provider_and_reloaded() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(answer_requests(listener, vec!["Reloaded model answer."]));
    let directory = tempfile::tempdir().unwrap();
    let config_root = directory.path().join("config-root");
    let external = directory.path().join("external.yaml");
    std::fs::write(
        &external,
        format!(
            "id: external-provider\ndefault_model: chosen-model\ndisplay_name: External provider\nauth: api_key_env\napi_key_env: EXTERNAL_TEST_KEY\nendpoints:\n  messages: http://{address}\nmodels:\n  - id: chosen-model\n    service_model_id: original-service-model\n    display_name: Chosen model\n    adapter: messages\n    max_output_tokens: 512\n  - id: sibling-model\n    service_model_id: sibling-service-model\n    display_name: Sibling model\n    adapter: messages\n"
        ),
    )
    .unwrap();
    let mut first = trusted_async_agent_command()
        .env("TICKLOOP_CONFIG_DIR", &config_root)
        .env("EXTERNAL_TEST_KEY", "test-key")
        .arg("--cwd")
        .arg(directory.path())
        .arg("--config")
        .arg(&external)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    first
        .stdin
        .take()
        .unwrap()
        .write_all(format!("/model {}\n/exit\n", external.display()).as_bytes())
        .await
        .unwrap();
    let output = tokio::time::timeout(INTERACTION_TIMEOUT, first.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let selection: Value =
        serde_json::from_slice(&std::fs::read(config_root.join("settings.json")).unwrap()).unwrap();
    assert!(selection.get("model").is_none(), "{selection}");
    assert_eq!(selection["defaultModel"]["provider"], "external-provider");
    assert_eq!(selection["defaultModel"]["model"], "chosen-model");
    let saved_provider_path = config_root.join("providers/external-provider.yaml");
    let mut saved_provider: serde_yaml_ng::Value =
        serde_yaml_ng::from_slice(&std::fs::read(&saved_provider_path).unwrap()).unwrap();
    assert_eq!(saved_provider["default_model"], "chosen-model");
    assert_eq!(saved_provider["models"].as_sequence().unwrap().len(), 2);
    saved_provider["models"][0]["service_model_id"] =
        serde_yaml_ng::Value::String("edited-service-model".into());
    std::fs::write(
        &saved_provider_path,
        serde_yaml_ng::to_string(&saved_provider).unwrap(),
    )
    .unwrap();

    let mut restarted = trusted_async_agent_command()
        .env("TICKLOOP_CONFIG_DIR", &config_root)
        .env("EXTERNAL_TEST_KEY", "test-key")
        .arg("--cwd")
        .arg(directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    restarted
        .stdin
        .take()
        .unwrap()
        .write_all(b"Hello after Provider YAML edit\n")
        .await
        .unwrap();
    let output = tokio::time::timeout(INTERACTION_TIMEOUT, restarted.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let requests = tokio::time::timeout(INTERACTION_TIMEOUT, server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(requests[0]["model"], "edited-service-model");
}

#[tokio::test]
async fn explicit_multimodel_yaml_shows_siblings_without_saving_on_load() {
    let directory = tempfile::tempdir().unwrap();
    let config_root = directory.path().join("config-root");
    let config = directory.path().join("multimodel.yaml");
    std::fs::write(
        &config,
        "id: external-provider\ndefault_model: first\ndisplay_name: External provider\nauth: api_key_env\napi_key_env: EXTERNAL_TEST_KEY\nendpoints:\n  messages: https://gateway.example/messages\nmodels:\n  - id: first\n    service_model_id: first-service\n    display_name: First choice\n    adapter: messages\n  - id: second\n    service_model_id: second-service\n    display_name: Second choice\n    adapter: messages\n",
    )
    .unwrap();
    let mut child = trusted_async_agent_command()
        .env("TICKLOOP_CONFIG_DIR", &config_root)
        .env("EXTERNAL_TEST_KEY", "fixture-external-key")
        .arg("--no-session-persistence")
        .arg("--cwd")
        .arg(directory.path())
        .arg("--config")
        .arg(&config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"/model\n1\ncancel\ncancel\n/exit\n")
        .await
        .unwrap();
    let output = tokio::time::timeout(INTERACTION_TIMEOUT, child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("First choice"), "{text}");
    assert!(text.contains("Second choice"), "{text}");
    assert!(
        !config_root
            .join("providers/external-provider.yaml")
            .exists()
    );
}

#[tokio::test]
async fn partial_explicit_yaml_inherits_saved_endpoint_without_freezing_defaults() {
    let directory = tempfile::tempdir().unwrap();
    let config_root = directory.path().join("config-root");
    std::fs::create_dir_all(config_root.join("providers")).unwrap();
    let saved_endpoint = "http://127.0.0.1:21987/anthropic";
    std::fs::write(
        config_root.join("providers/deepseek-api.yaml"),
        format!("id: deepseek-api\nendpoints:\n  deepseek_messages: {saved_endpoint}\n"),
    )
    .unwrap();
    let explicit = directory.path().join("partial.yaml");
    std::fs::write(
        &explicit,
        "id: deepseek-api\ndefault_model: deepseek-flash-messages\nmodels:\n  - id: deepseek-flash-messages\n    max_output_tokens: 8192\n",
    )
    .unwrap();
    let mut child = trusted_async_agent_command()
        .env("TICKLOOP_CONFIG_DIR", &config_root)
        .arg("--no-session-persistence")
        .arg("--cwd")
        .arg(directory.path())
        .arg("--config")
        .arg(&explicit)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("/model {}\n/exit\n", explicit.display()).as_bytes())
        .await
        .unwrap();
    let output = tokio::time::timeout(INTERACTION_TIMEOUT, child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains(saved_endpoint));
    let saved: tickloop_agent::model::cards::ProviderPatch = serde_yaml_ng::from_slice(
        &std::fs::read(config_root.join("providers/deepseek-api.yaml")).unwrap(),
    )
    .unwrap();
    assert_eq!(saved.display_name, None);
    assert_eq!(saved.models.as_ref().unwrap().len(), 1);
    assert_eq!(
        saved.models.as_ref().unwrap()[0].max_output_tokens,
        Some(8192)
    );
    assert_eq!(saved.models.as_ref().unwrap()[0].service_model_id, None);
    let restarted = UserSettings::in_root(&config_root)
        .load()
        .configured_model()
        .unwrap()
        .unwrap();
    assert_eq!(restarted.base_url, saved_endpoint);
    assert_eq!(restarted.max_output_tokens, 8192);
}

#[test]
fn invalid_settings_are_reported_once_without_starting_a_default_model() {
    let directory = tempfile::tempdir().unwrap();
    let config_root = directory.path().join("config-root");
    std::fs::create_dir_all(&config_root).unwrap();
    std::fs::write(
        config_root.join("settings.json"),
        "{\"contextWindowTokens\": 64000,}",
    )
    .unwrap();
    let output = trusted_agent_command()
        .env("TICKLOOP_CONFIG_DIR", &config_root)
        .arg("--cwd")
        .arg(directory.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("Chat:"));
    let text = String::from_utf8(output.stderr).unwrap();
    assert_eq!(text.matches("not valid JSON").count(), 1, "{text}");
    assert_eq!(
        std::fs::read_to_string(config_root.join("settings.json")).unwrap(),
        "{\"contextWindowTokens\": 64000,}"
    );
}

#[tokio::test]
async fn terminal_reviews_and_approves_a_specific_plan_version_before_writing() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for turn in 0..5 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_request(&mut socket).await.to_string();
            let body = match turn {
                0 => {
                    let plan_file = request
                        .split_once("create your plan at ")
                        .and_then(|(_, rest)| rest.split_once(" using the Write tool."))
                        .expect("the planning request names the plan file")
                        .0;
                    tool_stream(
                        "Write",
                        json!({"file_path":plan_file,"content":"Create reviewed.txt with the approved result."}),
                    )
                    .replace("tool-fixture", "plan-file-tool")
                }
                1 => tool_stream("ExitPlanMode", json!({})).replace("tool-fixture", "plan-tool"),
                2 => messages_stream("The plan is ready for review."),
                3 => tool_stream(
                    "Write",
                    json!({"file_path":"reviewed.txt","content":"approved plan result"}),
                )
                .replace("tool-fixture", "write-tool"),
                _ => messages_stream("The approved file is ready."),
            };
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
            socket.write_all(body.as_bytes()).await.unwrap();
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("model.json");
    std::fs::write(&config,serde_json::to_vec(&json!({"name":"fixture","protocol":"messages","base_url":format!("http://{address}"),"model":"fixture","api_key_env":"","max_output_tokens":256})).unwrap()).unwrap();
    let mut child = trusted_async_agent_command()
        .env("TICKLOOP_CONFIG_DIR", directory.path().join("config-root"))
        .arg("--config")
        .arg(config)
        .arg("--cwd")
        .arg(directory.path())
        .arg("--no-session-persistence")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    input
        .write_all(b"/rule allow Write\n/mode plan\nPlan a new result file\n")
        .await
        .unwrap();
    let mut transcript = String::new();
    let mut goal = String::new();
    tokio::time::timeout(INTERACTION_TIMEOUT, async {
        loop {
            let line = output.next_line().await.unwrap().unwrap();
            if let Some(value) = line.strip_prefix("Goal: ") {
                goal = value.split_whitespace().next().unwrap().to_owned();
            }
            transcript.push_str(&line);
            if line.starts_with("Reply completed") {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert!(transcript.contains("Create reviewed.txt with the approved result."));
    assert!(!directory.path().join("reviewed.txt").exists());
    input
        .write_all(
            format!("/approve-plan {goal} 1 acceptEdits\nProceed with the approved plan\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    drop(input);
    tokio::time::timeout(INTERACTION_TIMEOUT, async {
        while let Some(line) = output.next_line().await.unwrap() {
            transcript.push_str(&line);
        }
    })
    .await
    .unwrap();
    assert!(child.wait().await.unwrap().success(), "{transcript}");
    assert_eq!(
        std::fs::read_to_string(directory.path().join("reviewed.txt")).unwrap(),
        "approved plan result"
    );
    server.await.unwrap();
}

#[tokio::test]
async fn terminal_can_background_read_and_stop_the_same_shell_process() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for body in [
            tool_stream(
                "Bash",
                json!({"command":"printf 'background marker'; sleep 30"}),
            ),
            messages_stream("The command is in the background."),
        ] {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request(&mut socket).await;
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
            socket.write_all(body.as_bytes()).await.unwrap();
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("model.json");
    std::fs::write(&config,serde_json::to_vec(&json!({"name":"fixture","protocol":"messages","base_url":format!("http://{address}"),"model":"fixture","api_key_env":"","max_output_tokens":256})).unwrap()).unwrap();
    let mut child = trusted_async_agent_command()
        .env("TICKLOOP_CONFIG_DIR", directory.path().join("config-root"))
        .arg("--config")
        .arg(config)
        .arg("--cwd")
        .arg(directory.path())
        .arg("--dangerously-skip-permissions")
        .arg("--no-session-persistence")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    input
        .write_all(b"/rule allow *\n/mode acceptEdits\nRun the long command\n")
        .await
        .unwrap();
    let operation = tokio::time::timeout(INTERACTION_TIMEOUT, async {
        loop {
            let line = output.next_line().await.unwrap().unwrap();
            if let Some(value) = line.strip_prefix("Command: ") {
                break value.split_whitespace().next().unwrap().to_owned();
            }
        }
    })
    .await
    .expect("shell command must expose its operation identity and status");
    input
        .write_all(format!("/background {operation}\n").as_bytes())
        .await
        .unwrap();
    let mut transcript = String::new();
    tokio::time::timeout(INTERACTION_TIMEOUT, async {
        loop {
            let line = output.next_line().await.unwrap().unwrap();
            transcript.push_str(&line);
            if line.starts_with("Reply completed") {
                break;
            }
        }
    })
    .await
    .unwrap();
    input
        .write_all(
            format!("/commands\n/command-output {operation}\n/stop-command {operation}\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    drop(input);
    tokio::time::timeout(INTERACTION_TIMEOUT, async {
        while let Some(line) = output.next_line().await.unwrap() {
            transcript.push_str(&line);
        }
    })
    .await
    .unwrap();
    assert!(child.wait().await.unwrap().success(), "{transcript}");
    assert!(transcript.contains("Background"));
    assert!(transcript.contains("background marker"));
    assert!(transcript.contains("Cancelling") || transcript.contains("Failed"));
    server.await.unwrap();
}

#[tokio::test]
async fn terminal_can_enable_and_deliver_an_explicit_goal_trigger() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for answer in ["The goal is ready.", "Event processed."] {
            let (mut socket, _) = listener.accept().await.unwrap();
            requests.push(read_request(&mut socket).await);
            let body = messages_stream(answer);
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
            socket.write_all(body.as_bytes()).await.unwrap();
        }
        requests
    });
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("model.json");
    std::fs::write(&config,serde_json::to_vec(&json!({"name":"fixture","protocol":"messages","base_url":format!("http://{address}"),"model":"fixture","api_key_env":"","max_output_tokens":256})).unwrap()).unwrap();
    let mut child = trusted_async_agent_command()
        .env("TICKLOOP_CONFIG_DIR", directory.path().join("config-root"))
        .arg("--config")
        .arg(config)
        .arg("--no-session-persistence")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    input.write_all(b"Observe a generic task\n").await.unwrap();
    tokio::time::timeout(INTERACTION_TIMEOUT, async {
        while !output
            .next_line()
            .await
            .unwrap()
            .unwrap()
            .starts_with("Reply completed")
        {}
    })
    .await
    .unwrap();
    input.write_all(b"/events Check the task when an explicit event arrives\n/trigger terminal {\"update\":42}\n").await.unwrap();
    drop(input);
    let mut transcript = String::new();
    tokio::time::timeout(INTERACTION_TIMEOUT, async {
        while let Some(line) = output.next_line().await.unwrap() {
            transcript.push_str(&line);
        }
    })
    .await
    .unwrap();
    assert!(child.wait().await.unwrap().success());
    assert!(transcript.contains("Event processed."));
    let requests = server.await.unwrap();
    assert!(requests[1]["messages"].to_string().contains("terminal"));
    assert!(requests[1]["messages"].to_string().contains("42"));
}

#[tokio::test]
async fn terminal_can_configure_a_search_service_and_return_source_links() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let model_server = tokio::spawn(async move {
        let mut request = Value::Null;
        for body in [
            tool_stream("WebSearch", json!({"query":"controlled query"})),
            messages_stream("Found the controlled source."),
        ] {
            let (mut socket, _) = listener.accept().await.unwrap();
            request = read_request(&mut socket).await;
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).as_bytes()).await.unwrap();
            socket.write_all(body.as_bytes()).await.unwrap();
        }
        request
    });
    let search_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let search_address = search_listener.local_addr().unwrap();
    let search_server = tokio::spawn(async move {
        let (mut socket, _) = search_listener.accept().await.unwrap();
        let mut header = Vec::new();
        loop {
            let byte = socket.read_u8().await.unwrap();
            header.push(byte);
            if header.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        assert!(String::from_utf8(header).unwrap().contains("controlled"));
        let body=json!({"results":[{"title":"Controlled source","url":"https://example.org/item","content":"Evidence summary"}]}).to_string();
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).as_bytes()).await.unwrap();
        socket.write_all(body.as_bytes()).await.unwrap();
    });
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("model.json"),serde_json::to_vec(&json!({"name":"fixture","protocol":"messages","base_url":format!("http://{address}"),"model":"fixture","api_key_env":"","max_output_tokens":256})).unwrap()).unwrap();
    let search_config = directory.path().join("search.json");
    std::fs::write(
        &search_config,
        serde_json::to_vec(
            &json!({"endpoint":format!("http://{search_address}/search"),"api_key_env":null}),
        )
        .unwrap(),
    )
    .unwrap();
    let output = run_with_config(
        directory.path(),
        &["--no-session-persistence"],
        &format!(
            "/search-config {}\n/rule allow *\n/mode acceptEdits\nFind a source\n",
            search_config.display()
        ),
    )
    .await;
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("https://example.org/item")
    );
    let request = model_server.await.unwrap();
    assert!(request["messages"].to_string().contains("Evidence summary"));
    search_server.await.unwrap();
}

#[tokio::test]
async fn terminal_can_compact_history_and_continue_using_the_summary() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for answer in [
            "Remembered cedar.",
            "The user wants to retain cedar; continue the existing goal.",
            "Cedar remains in context.",
        ] {
            let (mut socket, _) = listener.accept().await.unwrap();
            requests.push(read_request(&mut socket).await);
            let body = messages_stream(answer);
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).as_bytes()).await.unwrap();
            socket.write_all(body.as_bytes()).await.unwrap();
        }
        requests
    });
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("model.json");
    std::fs::write(&config,serde_json::to_vec(&json!({"name":"fixture","protocol":"messages","base_url":format!("http://{address}"),"model":"fixture","api_key_env":"","max_output_tokens":256})).unwrap()).unwrap();
    let mut child = trusted_async_agent_command()
        .env("TICKLOOP_CONFIG_DIR", directory.path().join("config-root"))
        .arg("--config")
        .arg(config)
        .arg("--no-session-persistence")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    input.write_all(b"Remember cedar\n").await.unwrap();
    tokio::time::timeout(INTERACTION_TIMEOUT, async {
        while !output
            .next_line()
            .await
            .unwrap()
            .unwrap()
            .starts_with("Reply completed")
        {}
    })
    .await
    .unwrap();
    input
        .write_all(b"/compact keep the remembered word\n")
        .await
        .unwrap();
    tokio::time::timeout(INTERACTION_TIMEOUT, async {
        while !output
            .next_line()
            .await
            .unwrap()
            .unwrap()
            .starts_with("Context compacted")
        {}
    })
    .await
    .expect("manual compaction must complete without blocking terminal control");
    input.write_all(b"What word remains?\n").await.unwrap();
    drop(input);
    let mut transcript = String::new();
    tokio::time::timeout(INTERACTION_TIMEOUT, async {
        while let Some(line) = output.next_line().await.unwrap() {
            transcript.push_str(&line);
        }
    })
    .await
    .unwrap();
    assert!(child.wait().await.unwrap().success());
    assert!(transcript.contains("Cedar remains in context."));
    let requests = server.await.unwrap();
    assert!(
        requests[1]["messages"]
            .to_string()
            .contains("Additional user instructions for this summary: keep the remembered word")
    );
    assert!(
        requests[2]["messages"]
            .to_string()
            .contains("The user wants to retain cedar")
    );
}

#[tokio::test]
async fn a_chat_cleared_away_keeps_running_without_writing_into_the_foreground() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (release, wait) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let mut release_wait = Some(wait);
        let mut handlers = Vec::new();
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_request(&mut socket).await;
            let is_background = request["messages"].to_string().contains("Finish this task");
            let wait = is_background.then(|| release_wait.take().unwrap());
            handlers.push(tokio::spawn(async move {
                let answer = if let Some(wait) = wait {
                    wait.await.unwrap();
                    "background answer preserved"
                } else {
                    "Foreground answer."
                };
                let body = messages_stream(answer);
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).as_bytes()).await.unwrap();
                socket.write_all(body.as_bytes()).await.unwrap();
            }));
        }
        for handler in handlers {
            handler.await.unwrap();
        }
    });
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("model.json"),serde_json::to_vec(&json!({"name":"fixture","protocol":"messages","base_url":format!("http://{address}"),"model":"fixture","api_key_env":"","max_output_tokens":256})).unwrap()).unwrap();
    let mut child = trusted_async_agent_command()
        .env("TICKLOOP_CONFIG_DIR", directory.path().join("config-root"))
        .arg("--config")
        .arg(directory.path().join("model.json"))
        .arg("--cwd")
        .arg(directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    input
        .write_all(b"Finish this task\n/clear\nForeground question\n")
        .await
        .unwrap();
    let mut transcript = Vec::new();
    tokio::time::timeout(INTERACTION_TIMEOUT, async {
        loop {
            let line = output.next_line().await.unwrap().unwrap();
            let answered = line.contains("Foreground answer.");
            transcript.push(line);
            if answered {
                break;
            }
        }
    })
    .await
    .expect("the new foreground Chat must answer while the cleared Chat is still running");
    release.send(()).unwrap();
    drop(input);
    tokio::time::timeout(INTERACTION_TIMEOUT, async {
        while let Some(line) = output.next_line().await.unwrap() {
            transcript.push(line);
        }
    })
    .await
    .unwrap();
    let status = tokio::time::timeout(INTERACTION_TIMEOUT, child.wait())
        .await
        .expect("the process must wait for the background Chat and then exit")
        .unwrap();
    assert!(status.success(), "{transcript:?}");
    tokio::time::timeout(INTERACTION_TIMEOUT, server)
        .await
        .unwrap()
        .unwrap();
    let chats = transcript
        .iter()
        .filter_map(|line| line.strip_prefix("Chat: "))
        .collect::<Vec<_>>();
    assert_eq!(chats.len(), 2, "{transcript:?}");
    assert!(
        !transcript
            .iter()
            .any(|line| line.contains("background answer preserved")),
        "the cleared Chat's reply must not appear in the foreground: {transcript:?}"
    );

    let resumed = run_with_config(directory.path(), &["--resume", chats[0]], "").await;
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    let resumed_text = String::from_utf8(resumed.stdout).unwrap();
    assert!(
        resumed_text.contains("background answer preserved"),
        "{resumed_text}"
    );
    assert!(
        !resumed_text.contains("Foreground answer."),
        "{resumed_text}"
    );
}

#[tokio::test]
async fn resuming_recovered_queued_input_starts_no_work_and_exits_at_eof() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("model.json");
    std::fs::write(
        &config,
        serde_json::to_vec(&json!({
            "name":"fixture", "protocol":"messages", "base_url":format!("http://{address}"),
            "model":"fixture", "api_key_env":"", "max_output_tokens":256
        }))
        .unwrap(),
    )
    .unwrap();
    let mut child = trusted_async_agent_command()
        .env("TICKLOOP_CONFIG_DIR", directory.path().join("config-root"))
        .arg("--config")
        .arg(config)
        .arg("--cwd")
        .arg(directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    input
        .write_all(b"Start a report\nAdd a conclusion after the report\n")
        .await
        .unwrap();
    let (mut socket, _) = tokio::time::timeout(INTERACTION_TIMEOUT, listener.accept())
        .await
        .unwrap()
        .unwrap();
    read_request(&mut socket).await;
    let mut chat = String::new();
    let mut goal = String::new();
    tokio::time::timeout(INTERACTION_TIMEOUT, async {
        let mut accepted = 0;
        while accepted < 2 {
            let line = output.next_line().await.unwrap().unwrap();
            if let Some(value) = line.strip_prefix("Chat: ") {
                chat = value.to_owned();
            }
            if let Some(value) = line.strip_prefix("Accepted: Goal ") {
                goal = value.split_whitespace().next().unwrap().to_owned();
                accepted += 1;
            }
        }
    })
    .await
    .expect("the second input must be accepted before the crash");
    // Transcript 每 100ms 批量追加；等待一次冲刷后再强制结束进程。
    tokio::time::sleep(Duration::from_millis(300)).await;
    child.kill().await.unwrap();
    drop(input);
    drop(socket);

    let result = run_with_config(directory.path(), &[], &format!("/resume {chat}\n")).await;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let text = String::from_utf8(result.stdout).unwrap();
    assert!(text.contains(&format!("Chat: {chat}")));
    assert!(text.contains("Start a report"), "{text}");
    assert!(text.contains("History opened"), "{text}");
    assert!(!goal.is_empty());
    assert!(!text.contains("Accepted:"));
    assert!(
        tokio::time::timeout(Duration::from_millis(50), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn operations_from_a_crashed_process_are_not_listed_after_resume() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        let body = tool_stream("Bash", json!({"command":"sleep 1"}));
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).as_bytes()).await.unwrap();
        socket.write_all(body.as_bytes()).await.unwrap();
    });
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("model.json");
    std::fs::write(&config,serde_json::to_vec(&json!({"name":"fixture","protocol":"messages","base_url":format!("http://{address}"),"model":"fixture","api_key_env":"","max_output_tokens":256})).unwrap()).unwrap();
    let mut child = trusted_async_agent_command()
        .env("TICKLOOP_CONFIG_DIR", directory.path().join("config-root"))
        .arg("--config")
        .arg(config)
        .arg("--cwd")
        .arg(directory.path())
        .arg("--dangerously-skip-permissions")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    input
        .write_all(b"/rule allow *\n/mode acceptEdits\nRun the short command\n")
        .await
        .unwrap();
    let mut chat = String::new();
    let mut goal = String::new();
    let operation = tokio::time::timeout(INTERACTION_TIMEOUT, async {
        loop {
            let line = output.next_line().await.unwrap().unwrap();
            if let Some(value) = line.strip_prefix("Chat: ") {
                chat = value.to_owned();
            }
            if let Some(value) = line.strip_prefix("Goal: ") {
                goal = value.split_whitespace().next().unwrap().to_owned();
            }
            if let Some(value) = line.strip_prefix("Command: ") {
                break value.split_whitespace().next().unwrap().to_owned();
            }
        }
    })
    .await
    .unwrap();
    // Transcript 每 100ms 批量追加；等待一次冲刷后再强制结束进程。
    tokio::time::sleep(Duration::from_millis(300)).await;
    child.kill().await.unwrap();
    drop(input);
    server.await.unwrap();
    assert!(!goal.is_empty());
    let result = run_with_config(
        directory.path(),
        &[],
        &format!("/resume {chat}\n/operations\n"),
    )
    .await;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let text = String::from_utf8(result.stdout).unwrap();
    assert!(
        text.contains("Operations recorded in this process: 0"),
        "{text}"
    );
    assert!(!text.contains(&format!("Operation: {operation}")), "{text}");
    assert!(!text.contains("Accepted:"));
}

#[tokio::test]
async fn ordinary_terminal_input_answers_the_visible_questions_one_line_each() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut request = Value::Null;
        for body in [
            tool_stream(
                "AskUserQuestion",
                json!({"questions":[
                    {"question":"Which color should the report use?","header":"Color","options":[
                        {"label":"Blue","description":"Calm"},
                        {"label":"Green","description":"Fresh"}
                    ]},
                    {"question":"Which sections should it include?","header":"Sections","multiSelect":true,"options":[
                        {"label":"Summary","description":"Opening"},
                        {"label":"Risks","description":"Open risks"},
                        {"label":"Charts","description":"Every chart"}
                    ]}
                ]}),
            ),
            messages_stream("The report will use Blue."),
        ] {
            let (mut socket, _) = listener.accept().await.unwrap();
            request = read_request(&mut socket).await;
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).as_bytes()).await.unwrap();
            socket.write_all(body.as_bytes()).await.unwrap();
        }
        request
    });
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("model.json");
    std::fs::write(&config,serde_json::to_vec(&json!({"name":"fixture","protocol":"messages","base_url":format!("http://{address}"),"model":"fixture","api_key_env":"","max_output_tokens":256})).unwrap()).unwrap();
    let mut child = trusted_async_agent_command()
        .env("TICKLOOP_CONFIG_DIR", directory.path().join("config-root"))
        .arg("--config")
        .arg(config)
        .arg("--no-session-persistence")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    input.write_all(b"Choose a report color\n").await.unwrap();
    tokio::time::timeout(INTERACTION_TIMEOUT, async {
        while !output
            .next_line()
            .await
            .unwrap()
            .unwrap()
            .starts_with("Question:")
        {}
    })
    .await
    .expect("a pending clarification must be visible");
    input.write_all(b"Blue\n1, 3\n").await.unwrap();
    drop(input);
    let mut transcript = String::new();
    tokio::time::timeout(INTERACTION_TIMEOUT, async {
        while let Some(line) = output.next_line().await.unwrap() {
            transcript.push_str(&line);
        }
    })
    .await
    .unwrap();
    assert!(child.wait().await.unwrap().success(), "{transcript}");
    assert!(transcript.contains("1/2 [Color] Which color should the report use?"));
    assert!(transcript.contains("  3. Other — type your own answer"));
    assert!(
        transcript
            .contains("2/2 [Sections] Which sections should it include? (choose one or more)")
    );
    assert!(transcript.contains("Answer recorded. Next question:"));
    assert!(transcript.contains("Answer accepted."));
    assert!(transcript.contains("The report will use Blue."));
    assert!(
        !transcript.contains("Accepted:"),
        "an answer must not become a new queued run"
    );
    let request = server.await.unwrap();
    let result = serde_json::to_string(
        "User has answered your questions: \"Which color should the report use?\"=\"Blue\", \"Which sections should it include?\"=\"Summary, Charts\". You can now continue with the user's answers in mind.",
    )
    .unwrap();
    let messages = request["messages"].to_string();
    assert!(messages.contains(result.trim_matches('"')), "{messages}");
}

async fn run_with_config(
    directory: &std::path::Path,
    flags: &[&str],
    text: &str,
) -> std::process::Output {
    trust_working_directory(&directory.join("config-root"), directory);
    let mut child = trusted_async_agent_command()
        .env("TICKLOOP_CONFIG_DIR", directory.join("config-root"))
        .arg("--config")
        .arg(directory.join("model.json"))
        .arg("--cwd")
        .arg(directory)
        .args(flags)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(text.as_bytes())
        .await
        .unwrap();
    tokio::time::timeout(INTERACTION_TIMEOUT, child.wait_with_output())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn the_removed_rewind_acknowledgement_is_unknown_without_calling_the_model() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("model.json"),
        serde_json::to_vec(&json!({
            "name": "fixture",
            "protocol": "messages",
            "base_url": "http://127.0.0.1:1",
            "model": "fixture",
            "api_key_env": "",
            "max_output_tokens": 256
        }))
        .unwrap(),
    )
    .unwrap();

    let result = run_with_config(
        directory.path(),
        &["--no-session-persistence"],
        "/ack-rewind\n/exit\n",
    )
    .await;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let transcript = String::from_utf8(result.stdout).unwrap();
    assert!(
        transcript.contains("Unknown command; use /help."),
        "the removed /ack-rewind command must not be accepted: {transcript}"
    );
    assert!(!transcript.contains("Accepted:"));
}

#[tokio::test]
async fn model_control_sequences_cannot_change_terminal_state() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        let body =
            messages_stream("safe\u{1b}]52;c;Y2xpcGJvYXJk\u{7}\u{1b}[2J\u{9b}2J\r\u{8} text");
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        socket.write_all(header.as_bytes()).await.unwrap();
        socket.write_all(body.as_bytes()).await.unwrap();
    });
    let directory = tempfile::tempdir().unwrap();
    let config_path = directory.path().join("model.json");
    std::fs::write(&config_path, serde_json::to_vec(&json!({
        "name":"local\u{1b}[2J fixture", "protocol":"messages", "base_url":format!("http://{address}"),
        "model":"fixture", "api_key_env":"", "max_output_tokens":256
    })).unwrap()).unwrap();
    let mut child = trusted_async_agent_command()
        .env("TICKLOOP_CONFIG_DIR", directory.path().join("config-root"))
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "--cwd",
            directory.path().to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"Hello\n")
        .await
        .unwrap();
    let output = tokio::time::timeout(INTERACTION_TIMEOUT, child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("safe"));
    assert!(
        text.chars()
            .all(|character| !character.is_control() || character == '\n' || character == '\t')
    );
    server.await.unwrap();
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> Value {
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        let length = socket.read(&mut buffer).await.unwrap();
        assert!(length > 0, "request ended before its body");
        bytes.extend_from_slice(&buffer[..length]);
        if let Some(end) = bytes.windows(4).position(|value| value == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..end]);
            let body_length: usize = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(str::trim)
                        .map(str::to_owned)
                })
                .unwrap()
                .parse()
                .unwrap();
            if bytes.len() >= end + 4 + body_length {
                return serde_json::from_slice(&bytes[end + 4..end + 4 + body_length]).unwrap();
            }
        }
    }
}

fn messages_stream(answer: &str) -> String {
    [
        json!({"type":"message_start","message":{"id":"fixture-id","type":"message","role":"assistant","content":[],"usage":{"input_tokens":7,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":answer}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":4}}),
        json!({"type":"message_stop"}),
    ].iter().map(|event| format!("data: {event}\n\n")).collect()
}

fn tool_stream(name: &str, input: Value) -> String {
    [
        json!({"type":"message_start","message":{"id":"fixture-id","type":"message","role":"assistant","content":[],"usage":{"input_tokens":7,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tool-fixture","name":name,"input":{}}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":input.to_string()}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":4}}),
        json!({"type":"message_stop"}),
    ].iter().map(|event| format!("data: {event}\n\n")).collect()
}

#[tokio::test]
async fn two_terminal_processes_share_a_config_dir_without_lock_errors() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut prompts = Vec::new();
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_request(&mut socket).await;
            prompts.push(request["messages"].to_string());
            let body = messages_stream("Answered concurrently.");
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
            socket.write_all(body.as_bytes()).await.unwrap();
        }
        prompts
    });
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("model.json"),
        serde_json::to_vec(&json!({
            "name":"local fixture", "protocol":"messages", "base_url":format!("http://{address}"),
            "model":"fixture", "api_key_env":"", "max_output_tokens":256
        }))
        .unwrap(),
    )
    .unwrap();
    let (first, second) = tokio::join!(
        run_with_config(directory.path(), &[], "Prompt from the first process.\n"),
        run_with_config(directory.path(), &[], "Prompt from the second process.\n"),
    );
    for output in [&first, &second] {
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{error}");
        assert!(!error.contains("database is locked"), "{error}");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("Answered concurrently."),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
    let prompts = tokio::time::timeout(INTERACTION_TIMEOUT, server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(prompts.len(), 2);

    let listing = run_with_config(directory.path(), &[], "/resume\ncancel\n").await;
    assert!(listing.status.success());
    let listing = String::from_utf8(listing.stdout).unwrap();
    for prompt in [
        "Prompt from the first process.",
        "Prompt from the second process.",
    ] {
        assert!(
            listing
                .lines()
                .any(|line| line.starts_with("  ") && line.contains(prompt)),
            "{listing}"
        );
    }
}

fn local_model_config(directory: &std::path::Path, address: std::net::SocketAddr) {
    std::fs::write(
        directory.join("model.json"),
        serde_json::to_vec(&json!({
            "name":"local fixture", "protocol":"messages", "base_url":format!("http://{address}"),
            "model":"fixture", "api_key_env":"", "max_output_tokens":256
        }))
        .unwrap(),
    )
    .unwrap();
}

async fn answer_requests(
    listener: tokio::net::TcpListener,
    answers: Vec<&'static str>,
) -> Vec<Value> {
    let mut requests = Vec::new();
    for answer in answers {
        let (mut socket, _) = listener.accept().await.unwrap();
        requests.push(read_request(&mut socket).await);
        let body = messages_stream(answer);
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
        socket.write_all(body.as_bytes()).await.unwrap();
    }
    requests
}

fn chat_ids(output: &std::process::Output) -> Vec<String> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix("Chat: "))
        .map(str::to_owned)
        .collect()
}

fn saved_transcript(directory: &std::path::Path, chat: &str) -> std::path::PathBuf {
    std::fs::read_dir(directory.join("config-root").join("projects"))
        .unwrap()
        .map(|project| project.unwrap().path().join(format!("{chat}.jsonl")))
        .find(|path| path.is_file())
        .unwrap_or_else(|| panic!("Transcript for Chat {chat} does not exist"))
}

#[tokio::test]
async fn continue_resumes_the_most_recent_chat_and_fork_session_needs_a_resume_flag() {
    let directory = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    local_model_config(directory.path(), listener.local_addr().unwrap());
    let server = tokio::spawn(answer_requests(
        listener,
        vec![
            "Old answer.",
            "Saved the word cedar.",
            "The word was cedar.",
        ],
    ));

    let missing = run_with_config(directory.path(), &["--continue"], "").await;
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("No conversation found to continue"));
    for flags in [
        vec!["--fork-session"],
        vec!["--fork-session", "--no-session-persistence"],
    ] {
        let rejected = run_with_config(directory.path(), &flags, "").await;
        assert!(!rejected.status.success());
        assert!(
            String::from_utf8_lossy(&rejected.stderr)
                .contains("--fork-session can only be used with --continue or --resume"),
            "{}",
            String::from_utf8_lossy(&rejected.stderr)
        );
    }

    let older = run_with_config(directory.path(), &[], "An older conversation.\n").await;
    assert!(older.status.success());
    tokio::time::sleep(Duration::from_millis(20)).await;
    let recent = run_with_config(directory.path(), &[], "Remember cedar.\n").await;
    assert!(recent.status.success());
    let recent_chat = chat_ids(&recent).remove(0);
    let continued = run_with_config(directory.path(), &["-c"], "What was the word?\n").await;
    assert!(
        continued.status.success(),
        "{}",
        String::from_utf8_lossy(&continued.stderr)
    );
    assert_eq!(chat_ids(&continued), vec![recent_chat]);
    assert!(String::from_utf8_lossy(&continued.stdout).contains("The word was cedar."));
    let requests = tokio::time::timeout(INTERACTION_TIMEOUT, server)
        .await
        .unwrap()
        .unwrap();
    let continued_request = requests[2]["messages"].to_string();
    assert!(continued_request.contains("Remember cedar."));
    assert!(!continued_request.contains("An older conversation."));
}

#[tokio::test]
async fn resume_picker_and_fork_session_open_a_chat_created_by_another_process() {
    let directory = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    local_model_config(directory.path(), listener.local_addr().unwrap());
    let server = tokio::spawn(answer_requests(
        listener,
        vec!["Source answer.", "Picked answer.", "Forked answer."],
    ));

    let source = run_with_config(directory.path(), &[], "Source prompt.\n").await;
    assert!(
        source.status.success(),
        "{}",
        String::from_utf8_lossy(&source.stderr)
    );
    let source_chat = chat_ids(&source).remove(0);
    tokio::time::sleep(Duration::from_millis(20)).await;
    let branched = run_with_config(directory.path(), &["-r", &source_chat], "/branch\n").await;
    assert!(
        String::from_utf8_lossy(&branched.stdout).contains("Branched conversation"),
        "{}",
        String::from_utf8_lossy(&branched.stdout)
    );
    std::fs::File::options()
        .write(true)
        .open(saved_transcript(directory.path(), &source_chat))
        .unwrap()
        .set_modified(std::time::SystemTime::now() - Duration::from_secs(120))
        .unwrap();

    let picked = run_with_config(directory.path(), &["-r"], "2\nPicked prompt.\n").await;
    assert!(
        picked.status.success(),
        "{}",
        String::from_utf8_lossy(&picked.stderr)
    );
    let picked_text = String::from_utf8(picked.stdout.clone()).unwrap();
    let rows = picked_text
        .lines()
        .filter(|line| line.starts_with("  ") && line.contains(". "))
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 3, "{picked_text}");
    assert!(rows[0].starts_with("  1. Source prompt. (Branch) · just now"));
    assert!(rows[0].contains("Branch of Source prompt."));
    assert!(rows[1].starts_with("  2. Source prompt. · 2m ago"));
    assert!(rows[1].contains(&source_chat));
    assert!(picked_text.contains(&format!("Opened Chat: Source prompt. · {source_chat}")));
    assert!(picked_text.contains("Picked answer."));
    let source_file = saved_transcript(directory.path(), &source_chat);
    let source_bytes = std::fs::read(&source_file).unwrap();

    let copied = run_with_config(
        directory.path(),
        &["--resume", &source_chat, "--fork-session"],
        "Forked prompt.\n",
    )
    .await;
    assert!(
        copied.status.success(),
        "{}",
        String::from_utf8_lossy(&copied.stderr)
    );
    let copied_chat = chat_ids(&copied).remove(0);
    assert_ne!(copied_chat, source_chat);
    assert_eq!(std::fs::read(&source_file).unwrap(), source_bytes);
    let copied_file =
        std::fs::read_to_string(saved_transcript(directory.path(), &copied_chat)).unwrap();
    assert!(copied_file.contains("Picked prompt."));
    assert!(copied_file.contains("Forked prompt."));

    let missing = uuid::Uuid::new_v4().to_string();
    let not_found = run_with_config(directory.path(), &["--resume", &missing], "").await;
    assert!(!not_found.status.success());
    assert!(
        String::from_utf8_lossy(&not_found.stderr)
            .contains(&format!("No conversation found with Chat ID: {missing}"))
    );
    let requests = tokio::time::timeout(INTERACTION_TIMEOUT, server)
        .await
        .unwrap()
        .unwrap();
    assert!(
        requests[1]["messages"]
            .to_string()
            .contains("Source answer.")
    );
    let copied_request = requests[2]["messages"].to_string();
    assert!(
        copied_request.contains("Picked answer."),
        "{copied_request}"
    );
}

#[tokio::test]
async fn clear_starts_a_new_chat_and_stops_the_foreground_shell_command() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let directory = tempfile::tempdir().unwrap();
    local_model_config(directory.path(), address);
    let started = std::time::Instant::now();
    let mut child = trusted_async_agent_command()
        .env("TICKLOOP_CONFIG_DIR", directory.path().join("config-root"))
        .arg("--config")
        .arg(directory.path().join("model.json"))
        .arg("--cwd")
        .arg(directory.path())
        .arg("--dangerously-skip-permissions")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    input
        .write_all(b"/rule allow *\n/mode acceptEdits\nRun a long command\n")
        .await
        .unwrap();
    let (mut socket, _) = tokio::time::timeout(INTERACTION_TIMEOUT, listener.accept())
        .await
        .unwrap()
        .unwrap();
    read_request(&mut socket).await;
    let body = tool_stream("Bash", json!({"command":"sleep 30"}));
    socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).as_bytes()).await.unwrap();
    socket.write_all(body.as_bytes()).await.unwrap();
    drop(socket);
    let mut transcript = Vec::new();
    tokio::time::timeout(INTERACTION_TIMEOUT, async {
        loop {
            let line = output.next_line().await.unwrap().unwrap();
            let running = line.starts_with("Command: ");
            transcript.push(line);
            if running {
                break;
            }
        }
    })
    .await
    .expect("the foreground command did not start");
    input.write_all(b"/clear\n").await.unwrap();
    drop(input);
    tokio::time::timeout(INTERACTION_TIMEOUT, async {
        while let Some(line) = output.next_line().await.unwrap() {
            transcript.push(line);
        }
    })
    .await
    .expect("the cleared Chat's foreground command kept the process alive");
    tokio::time::timeout(INTERACTION_TIMEOUT, child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(25));
    let chats = transcript
        .iter()
        .filter_map(|line| line.strip_prefix("Chat: "))
        .collect::<Vec<_>>();
    assert_eq!(chats.len(), 2, "{transcript:?}");
    assert_ne!(chats[0], chats[1]);
    let short_source = chats[0].replace('-', "")[..8].to_owned();
    assert!(
        transcript
            .iter()
            .any(|line| { *line == format!("Background Chat {short_source} · interrupted") }),
        "the stopped command must be reported only as a background notice: {transcript:?}"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), listener.accept())
            .await
            .is_err(),
        "the stopped command must not continue the cleared Chat"
    );
}

#[tokio::test]
async fn process_registration_follows_the_foreground_chat_and_is_removed_at_exit() {
    let directory = tempfile::tempdir().unwrap();
    let config_root = directory.path().join("config-root");
    let sessions = config_root.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let mut exited = Command::new("true").spawn().unwrap();
    let exited_pid = exited.id();
    exited.wait().unwrap();
    let stale = sessions.join(format!("{exited_pid}.json"));
    std::fs::write(&stale, r#"{"pid":1}"#).unwrap();

    let mut child = trusted_async_agent_command()
        .env("TICKLOOP_CONFIG_DIR", &config_root)
        .arg("--cwd")
        .arg(directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let registration = sessions.join(format!("{}.json", child.id().unwrap()));
    let mut input = child.stdin.take().unwrap();
    let mut output = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    let registered_chat = |path: &std::path::Path| {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|record| record["chatId"].as_str().map(str::to_owned))
    };
    let first_chat = next_chat_line(&mut output).await;
    tokio::time::timeout(INTERACTION_TIMEOUT, async {
        while registered_chat(&registration).as_deref() != Some(first_chat.as_str()) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("startup did not register the process with its Chat");
    let record: Value = serde_json::from_slice(&std::fs::read(&registration).unwrap()).unwrap();
    assert_eq!(record["pid"], child.id().unwrap());
    assert_eq!(record["kind"], "interactive");
    assert_eq!(
        record["cwd"],
        directory.path().canonicalize().unwrap().to_str().unwrap()
    );
    assert!(
        !stale.exists(),
        "the exited process's registration was not removed"
    );

    input.write_all(b"/clear\n").await.unwrap();
    let second_chat = next_chat_line(&mut output).await;
    assert_ne!(second_chat, first_chat);
    tokio::time::timeout(INTERACTION_TIMEOUT, async {
        while registered_chat(&registration).as_deref() != Some(second_chat.as_str()) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("switching Chats did not update the registration");

    drop(input);
    let status = tokio::time::timeout(INTERACTION_TIMEOUT, child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success());
    assert!(!registration.exists());
}

async fn next_chat_line(
    output: &mut tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>,
) -> String {
    tokio::time::timeout(INTERACTION_TIMEOUT, async {
        loop {
            let line = output.next_line().await.unwrap().unwrap();
            if let Some(chat) = line.strip_prefix("Chat: ") {
                return chat.to_owned();
            }
        }
    })
    .await
    .expect("the terminal did not report its Chat")
}

async fn respond_with(socket: &mut tokio::net::TcpStream, body: &str) {
    socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
    socket.write_all(body.as_bytes()).await.unwrap();
}

async fn wait_for_line(
    output: &mut tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>,
    transcript: &mut Vec<String>,
    matches: impl Fn(&str) -> bool,
) -> String {
    tokio::time::timeout(INTERACTION_TIMEOUT, async {
        loop {
            let line = output.next_line().await.unwrap().unwrap();
            transcript.push(line.clone());
            if matches(&line) {
                return line;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("expected output did not appear: {transcript:?}"))
}

async fn finish_process(
    mut child: tokio::process::Child,
    mut output: tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>,
    transcript: &mut Vec<String>,
) -> std::process::ExitStatus {
    tokio::time::timeout(INTERACTION_TIMEOUT, async {
        while let Some(line) = output.next_line().await.unwrap() {
            transcript.push(line);
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the process kept its output open: {transcript:?}"));
    tokio::time::timeout(INTERACTION_TIMEOUT, child.wait())
        .await
        .unwrap_or_else(|_| panic!("the process did not exit after EOF: {transcript:?}"))
        .unwrap()
}

fn spawn_piped_terminal(directory: &std::path::Path) -> tokio::process::Child {
    trusted_async_agent_command()
        .env("TICKLOOP_CONFIG_DIR", directory.join("config-root"))
        .arg("--config")
        .arg(directory.join("model.json"))
        .arg("--cwd")
        .arg(directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}

#[tokio::test]
async fn a_background_chat_permission_request_is_announced_and_answered_by_request_id() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let external_path = outside.path().join("note.txt");
    std::fs::write(&external_path, "approved background note").unwrap();
    let tool_input = json!({"file_path":external_path});
    let (release, wait) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        wait.await.unwrap();
        respond_with(&mut socket, &tool_stream("Read", tool_input)).await;
        let (mut socket, _) = listener.accept().await.unwrap();
        let final_request = read_request(&mut socket).await;
        respond_with(&mut socket, &messages_stream("Background note read.")).await;
        final_request
    });
    let directory = tempfile::tempdir().unwrap();
    local_model_config(directory.path(), address);
    let mut child = spawn_piped_terminal(directory.path());
    let mut input = child.stdin.take().unwrap();
    let mut output = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    let mut transcript = Vec::new();
    let original_chat = next_chat_line(&mut output).await;
    input
        .write_all(b"Read the external note\n/clear\n")
        .await
        .unwrap();
    wait_for_line(&mut output, &mut transcript, |line| {
        line.strip_prefix("Chat: ")
            .is_some_and(|chat| chat != original_chat)
    })
    .await;
    release.send(()).unwrap();
    let original_label = format!("Background Chat {}", &original_chat.replace('-', "")[..8]);
    let notice = wait_for_line(&mut output, &mut transcript, |line| {
        line.starts_with(&format!("{original_label} · permission request "))
    })
    .await;
    let request_id = notice
        .split(" · permission request ")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned();
    input
        .write_all(format!("/allow {request_id}\n").as_bytes())
        .await
        .unwrap();
    wait_for_line(&mut output, &mut transcript, |line| {
        line == format!("{original_label} · reply completed")
    })
    .await;
    drop(input);
    let status = finish_process(child, output, &mut transcript).await;
    assert!(status.success(), "{transcript:?}");
    assert!(
        transcript
            .iter()
            .any(|line| line == "Permission decision accepted."),
        "{transcript:?}"
    );
    let final_request = tokio::time::timeout(INTERACTION_TIMEOUT, server)
        .await
        .unwrap()
        .unwrap();
    assert!(
        final_request["messages"]
            .to_string()
            .contains("approved background note")
    );
}

#[tokio::test]
async fn queued_input_merged_after_its_chat_was_cleared_away_lets_the_process_exit() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("note.txt"), "local note").unwrap();
    let tool_input = json!({"file_path":directory.path().join("note.txt")});
    let (release, wait) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        wait.await.unwrap();
        respond_with(&mut socket, &tool_stream("Read", tool_input)).await;
        let (mut socket, _) = listener.accept().await.unwrap();
        let merged_request = read_request(&mut socket).await;
        respond_with(&mut socket, &messages_stream("Correction applied.")).await;
        merged_request
    });
    local_model_config(directory.path(), address);
    let mut child = spawn_piped_terminal(directory.path());
    let mut input = child.stdin.take().unwrap();
    let mut output = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    let mut transcript = Vec::new();
    let original_chat = next_chat_line(&mut output).await;
    input
        .write_all(b"Read the note\nAlso apply this correction\n/clear\n")
        .await
        .unwrap();
    wait_for_line(&mut output, &mut transcript, |line| {
        line.strip_prefix("Chat: ")
            .is_some_and(|chat| chat != original_chat)
    })
    .await;
    assert_eq!(
        transcript
            .iter()
            .filter(|line| line.starts_with("Accepted:"))
            .count(),
        2,
        "{transcript:?}"
    );
    release.send(()).unwrap();
    drop(input);
    let status = finish_process(child, output, &mut transcript).await;
    assert!(status.success(), "{transcript:?}");
    let merged_request = tokio::time::timeout(INTERACTION_TIMEOUT, server)
        .await
        .unwrap()
        .unwrap();
    assert!(
        merged_request["messages"]
            .to_string()
            .contains("Also apply this correction"),
        "the queued input must join the running reply: {merged_request}"
    );
}

/// 清理天数为 0 时不保存 Chat：`-c` 说明原因，帮助文本介绍清理天数。
#[test]
fn a_zero_cleanup_period_explains_why_chats_cannot_be_resumed() {
    let directory = tempfile::tempdir().unwrap();
    let config_root = directory.path().join("config-root");
    std::fs::create_dir_all(&config_root).unwrap();
    std::fs::write(
        config_root.join("settings.json"),
        r#"{"cleanupPeriodDays": 0}"#,
    )
    .unwrap();
    let output = trusted_agent_command()
        .env("TICKLOOP_CONFIG_DIR", &config_root)
        .arg("--cwd")
        .arg(directory.path())
        .arg("--continue")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(!output.status.success());
    assert!(error.contains("cleanupPeriodDays is 0"), "{error}");
    assert!(!config_root.join("projects").exists());

    let help = trusted_agent_command().arg("--help").output().unwrap();
    let text = String::from_utf8(help.stdout).unwrap();
    assert!(text.contains("cleanupPeriodDays"), "{text}");
}
