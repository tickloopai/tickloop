use tickloop_agent_test_support::screen;

use crate::support;

use std::io::Write;
use std::process::Command;
use std::{process::Stdio, thread, time::Duration};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde_json::{Value, json};
use tickloop_agent_config::UserSettings;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

/// Every wait here expects success; a real CLI with its own daemon, worker and Shells needed more
/// than ten seconds on a machine busy with parallel test runs. Only a failure waits this long.
const INTERACTION_TIMEOUT: Duration = Duration::from_secs(60);

fn trust_working_directory(config_root: &std::path::Path, workdir: &std::path::Path) {
    UserSettings::in_root(config_root)
        .trust_working_directory(workdir)
        .unwrap();
}

fn trusted_agent_command() -> Command {
    let mut command = support::fixture_cli::command();
    command.env(
        "TICKLOOP_DAEMON_RUNTIME_DIR",
        support::cli_runtime::directory(),
    );
    command.args(["--working-directory-is-trusted", "--mode", "acceptEdits"]);
    command
}

fn trusted_async_agent_command() -> tokio::process::Command {
    let mut command = support::fixture_cli::tokio_command();
    command.env(
        "TICKLOOP_DAEMON_RUNTIME_DIR",
        support::cli_runtime::directory(),
    );
    command.args(["--working-directory-is-trusted", "--mode", "acceptEdits"]);
    command
}

fn product_async_agent_command() -> tokio::process::Command {
    let mut command = support::fixture_cli::tokio_command();
    command.env(
        "TICKLOOP_DAEMON_RUNTIME_DIR",
        support::cli_runtime::directory(),
    );
    command.arg("--working-directory-is-trusted");
    command
}

#[test]
fn malformed_auto_settings_do_not_exit_the_terminal_on_permissions_commands() {
    let directory = tempfile::tempdir().unwrap();
    let config_root = directory.path().join("config-root");
    std::fs::create_dir_all(&config_root).unwrap();
    std::fs::write(config_root.join("settings.json"), r#"{"autoApproval":{"rules":{"softDeny":{"includeBuiltins":false,"custom":[{"id":"invalid","text":"rule"}]}}}}"#).unwrap();
    let output = support::fixture_cli::command()
        .env(
            "TICKLOOP_DAEMON_RUNTIME_DIR",
            support::cli_runtime::directory(),
        )
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
                .write_all(b"/permissions\n/rules\n!true\n/rules\n/exit\n")?;
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
    let (before_first_input, after_first_input) = stdout
        .split_once("Chat: ")
        .expect("the first input creates the Agent");
    assert!(
        before_first_input.contains("Permission rules for the New Agent"),
        "configuration before the first input reads no saved Auto policy: {stdout}"
    );
    assert!(
        after_first_input.contains("Cannot list permission rules"),
        "the created Agent still reports the malformed saved policy without exiting: {stdout}"
    );
}

#[test]
fn interactive_pty_trusts_the_directory_and_cycles_all_execution_modes() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let config_root = directory.path().join("config-root");
    let home = directory.path().join("home");
    let scratch = directory.path().join("scratch");
    std::fs::create_dir(&home).unwrap();
    std::fs::create_dir(&scratch).unwrap();
    for path in [&home, &scratch] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    std::fs::create_dir(&config_root).unwrap();
    std::fs::write(
        config_root.join("settings.json"),
        r#"{"shiftTabModes":["auto","acceptEdits","plan","fullAccess"]}"#,
    )
    .unwrap();
    let pty = native_pty_system()
        .openpty(PtySize {
            rows: 30,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let reader = pty.master.try_clone_reader().unwrap();
    let mut writer = pty.master.take_writer().unwrap();
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_tickloop"));
    command.env(
        "TICKLOOP_DAEMON_RUNTIME_DIR",
        support::cli_runtime::directory(),
    );
    command.cwd(directory.path());
    command.env("TICKLOOP_CONFIG_DIR", &config_root);
    command.env("HOME", &home);
    command.env("TICKLOOP_TMPDIR", &scratch);
    command.env("TICKLOOP_AGENT_TUI", "inline");
    command.arg("--no-session-persistence");
    let mut child = pty.slave.spawn_command(command).unwrap();
    drop(pty.slave);
    let output = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let reader_output = output.clone();
    // Status glyphs are multi-byte; the shared capture keeps a character split across reads.
    let output_reader = thread::spawn(move || screen::capture_output(reader, reader_output));
    let wait_for_output = |needle: &str, after: usize| {
        let deadline = std::time::Instant::now() + INTERACTION_TIMEOUT;
        let ansi = regex::Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").unwrap();
        loop {
            let captured = output.lock().unwrap();
            if captured.get(after..).is_some_and(|suffix| {
                let editor_ready = !needle.starts_with("⏵⏵")
                    || suffix.rfind("\x1b[?25h").is_some_and(|shown| {
                        suffix
                            .rfind("\x1b[?25l")
                            .is_none_or(|hidden| shown > hidden)
                    });
                editor_ready && ansi.replace_all(suffix, "").contains(needle)
            }) {
                return;
            }
            if std::time::Instant::now() >= deadline {
                // Release the output lock first, so the reader thread is not poisoned.
                let text = captured.clone();
                drop(captured);
                panic!("PTY output did not contain {needle:?}: {text}");
            }
            drop(captured);
            thread::sleep(Duration::from_millis(20));
        }
    };

    wait_for_output("Accessing workspace:", 0);
    writer.write_all(b"j\r").unwrap();
    writer.flush().unwrap();
    wait_for_output("⏵⏵ Auto · DeepSeek Flash", 0);
    for mode in ["Ask", "Plan", "Full Access", "Auto"] {
        let output_start = output.lock().unwrap().len();
        writer.write_all(b"\x1b[Z").unwrap();
        writer.flush().unwrap();
        wait_for_output(&format!("⏵⏵ {mode} · DeepSeek Flash"), output_start);
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
    assert!(output.contains("Accessing workspace:"), "{output}");
    assert!(
        regex::Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]")
            .unwrap()
            .replace_all(&output, "")
            .contains("⏵⏵ Auto · DeepSeek Flash"),
        "{output}"
    );
    assert!(UserSettings::in_root(&config_root).is_working_directory_trusted(directory.path()));
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
        .env(
            "TICKLOOP_DAEMON_RUNTIME_DIR",
            support::cli_runtime::directory(),
        )
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
    assert!(!text.contains("--config"));
    assert!(!text.contains("/model FILE"));
    assert!(text.contains("--cwd"));
    assert!(text.contains("/stop interrupts the current turn"));
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
    let output = support::fixture_cli::command()
        .env(
            "TICKLOOP_DAEMON_RUNTIME_DIR",
            support::cli_runtime::directory(),
        )
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
            b"/provider\n1\n2\n/model\n2\n/model\ncancel\n/settings\n4\n2\n/settings\ncancel\n/exit\n",
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
        transcript.contains("DeepSeek Flash (deepseek-flash)    Thinking"),
        "{transcript}"
    );
    assert!(
        transcript.contains(
            "Current model: DeepSeek API · DeepSeek-V4.1-Flash (deepseek-flash) · OpenAI Chat Completions"
        ),
        "{transcript}"
    );
    assert!(
        transcript.contains("Execution mode: Ask. Explicit deny rules remain effective."),
        "{transcript}"
    );
    assert!(
        transcript.contains("Choose Provider · All")
            && transcript.contains("Choose model · Signed-in Providers")
            && transcript.contains("Model selection: Applied · DeepSeek-V4.1-Flash · Effort auto"),
        "{transcript}"
    );
    assert!(!transcript.contains("Accepted: Run"), "{transcript}");
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
        .write_all(b"/thinking medium\n/thinking\n4\n/thinking\ncancel\n/exit\n")
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
        transcript.contains("Effort levels for deepseek-"),
        "{transcript}"
    );
    assert!(transcript.contains("low, high, max"), "{transcript}");
    assert!(
        !transcript.contains("off · disable thinking"),
        "{transcript}"
    );
    assert!(!transcript.contains("medium · current"), "{transcript}");
    assert!(transcript.contains("Effort: max"), "{transcript}");
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
            json!({"id":"thinking-id","choices":[{"index":0,"delta":{"role":"assistant","reasoning_content":"Compare both values."},"finish_reason":null}]}),
            json!({"id":"thinking-id","choices":[{"index":0,"delta":{"content":"The second value is larger."},"finish_reason":null}]}),
            json!({"id":"thinking-id","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":7,"completion_tokens":9}}),
        ].iter().map(|event| format!("data: {event}\n\n")).collect::<String>() + "data: [DONE]\n\n";
        socket
            .write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes())
            .await
            .unwrap();
        socket.write_all(body.as_bytes()).await.unwrap();
        request
    });
    let directory = tempfile::tempdir().unwrap();
    let mut config = tickloop_agent_model::cards::ModelCatalog::defaults()
        .get("deepseek-api", "deepseek-flash")
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
        "/thinking max\nCompare\n",
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
    assert_eq!(request["reasoning_effort"], "max");
}

/// A `glyphSet` an earlier version saved, when Tickloop still offered a second glyph set, neither
/// stops the CLI nor makes it warn.
#[tokio::test]
async fn a_glyph_set_an_earlier_version_saved_starts_without_a_warning() {
    let directory = tempfile::tempdir().unwrap();
    let config_root = directory.path().join("config-root");
    std::fs::create_dir(&config_root).unwrap();
    std::fs::write(
        config_root.join("settings.json"),
        r#"{"glyphSet": "unicode"}"#,
    )
    .unwrap();
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
        .write_all(b"/settings\n/exit\n")
        .await
        .unwrap();
    let output = tokio::time::timeout(INTERACTION_TIMEOUT, child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    let (stdout, stderr) = (
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(output.status.success(), "{stderr}");
    assert!(stdout.contains("Reduce motion"), "{stdout}");
    assert!(
        !stdout.contains("glyphSet") && !stderr.contains("glyphSet"),
        "no warning names the old setting: {stdout}{stderr}"
    );
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
            "--transport-fixture",
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
            "--transport-fixture",
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
    assert!(text.contains("deepseek-flash"));
    assert!(text.contains("Remembered violet."));
    assert!(text.contains("The word is violet."));
    assert_eq!(text.matches("Reply completed").count(), 2);
    let requests = tokio::time::timeout(INTERACTION_TIMEOUT, server)
        .await
        .unwrap()
        .unwrap();
    assert_tickloop_reference_and_messages(
        &requests[1],
        &[
            ("user", "Remember the word violet."),
            ("assistant", "Remembered violet."),
            ("user", "What was the word?"),
        ],
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
            "--transport-fixture",
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
        let body = "Access refused; Authorization: Bearer server-private";
        socket.write_all(format!("HTTP/1.1 401 Unauthorized\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
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
            "--transport-fixture",
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
    assert!(text.contains("Access refused"));
    server.await.unwrap();
}

#[tokio::test]
async fn piped_agent_creation_failure_is_reported_on_stderr_and_fails_the_process() {
    for input in ["Hello\n", "Hello\n/exit\n"] {
        assert_piped_creation_refused(input).await;
    }
}

async fn assert_piped_creation_refused(piped_input: &str) {
    let directory = tempfile::tempdir().unwrap();
    let config_path = directory.path().join("model.json");
    std::fs::write(
        &config_path,
        serde_json::to_vec(&json!({
            "name":"local fixture", "protocol":"messages", "base_url":"http://127.0.0.1:1",
            "model":"fixture", "api_key_env":"", "max_output_tokens":256
        }))
        .unwrap(),
    )
    .unwrap();
    let workdir = directory.path().join("work");
    std::fs::create_dir(&workdir).unwrap();
    let mut child = trusted_async_agent_command()
        .env("TICKLOOP_CONFIG_DIR", directory.path().join("config-root"))
        .args([
            "--transport-fixture",
            config_path.to_str().unwrap(),
            "--cwd",
            workdir.to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    let banner = tokio::time::timeout(INTERACTION_TIMEOUT, output.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(banner.starts_with("Tickloop"), "{banner}");
    // The daemon cannot start a worker in a directory that no longer exists, so it refuses Create.
    std::fs::remove_dir(&workdir).unwrap();
    input.write_all(piped_input.as_bytes()).await.unwrap();
    drop(input);
    let mut transcript = String::new();
    while let Some(line) = tokio::time::timeout(INTERACTION_TIMEOUT, output.next_line())
        .await
        .unwrap()
        .unwrap()
    {
        transcript.push_str(&line);
        transcript.push('\n');
    }
    let result = tokio::time::timeout(INTERACTION_TIMEOUT, child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        !result.status.success(),
        "{piped_input:?}: {transcript}\n{stderr}"
    );
    assert!(
        stderr.contains("Failed: New Agent was not created"),
        "{piped_input:?}: {transcript}\n{stderr}"
    );
    assert!(!transcript.contains("Chat: "), "{transcript}");
    assert!(!transcript.contains("Accepted: Run"), "{transcript}");
}

#[test]
fn removed_configuration_flag_is_actionable_without_echoing_its_contents() {
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
    assert!(error.contains("not supported"));
    assert!(error.contains("built-in"));
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
    let rejected = run_with_config(directory.path(), &[], &format!("/resume {chat} extra\n")).await;
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
    let requests = tokio::time::timeout(INTERACTION_TIMEOUT, server)
        .await
        .unwrap()
        .unwrap();
    assert_tickloop_reference_and_messages(
        &requests[1],
        &[
            ("user", "Remember cedar."),
            ("assistant", "Saved the word cedar."),
            ("user", "What was the word?"),
            (
                "user",
                concat!(
                    "Host notice: host tools available in this conversation now: ArtifactLogin, TickCall, TickDescribe, TickManage, TickRead. ",
                    "Earlier statements in this conversation about tool availability may be outdated. ",
                    "This notice comes from the host, not the user, and does not authorize any new work."
                ),
            ),
        ],
    );
}

#[tokio::test]
async fn a_run_whose_worker_is_lost_is_reported_and_the_cli_exits_at_end_of_input() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    // The model takes the request and never answers, so the run stays in progress.
    let (arrived, request) = tokio::sync::oneshot::channel::<tokio::net::TcpStream>();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        let _ = arrived.send(socket);
    });
    let directory = tempfile::tempdir().unwrap();
    local_model_config(directory.path(), address);
    let mut command = trusted_async_agent_command();
    let runtime_directory = support::cli_runtime::command_directory(command.as_std());
    let mut child = command
        .env("TICKLOOP_CONFIG_DIR", directory.path().join("config-root"))
        .arg("--transport-fixture")
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
    let mut transcript = Vec::new();
    input
        .write_all(b"Keep working until the worker is lost\n")
        .await
        .unwrap();
    wait_for_line(&mut output, &mut transcript, |line| {
        line.starts_with("Accepted: Run")
    })
    .await;
    let _held = tokio::time::timeout(INTERACTION_TIMEOUT, request)
        .await
        .unwrap()
        .unwrap();
    // The peer of the unique socket in this fixture's private directory is its worker.
    let workers = std::fs::read_dir(&runtime_directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("w-")
                && path
                    .extension()
                    .is_some_and(|extension| extension == "sock")
        })
        .collect::<Vec<_>>();
    assert_eq!(workers.len(), 1, "{workers:?}");
    let worker = tokio::net::UnixStream::connect(&workers[0]).await.unwrap();
    let credentials = worker.peer_cred().unwrap();
    assert_eq!(credentials.uid(), unsafe { libc::geteuid() });
    let worker_pid = credentials
        .pid()
        .expect("the worker socket identifies its peer");
    assert!(worker_pid > 0);
    assert_ne!(Some(worker_pid as u32), child.id());
    let mut errors = child.stderr.take().unwrap();
    let errors = tokio::spawn(async move {
        let mut text = String::new();
        let _ = errors.read_to_string(&mut text).await;
        text
    });
    assert_eq!(unsafe { libc::kill(worker_pid, libc::SIGKILL) }, 0);
    drop(input);
    let status = finish_process(child, output, &mut transcript).await;
    let errors = errors.await.unwrap();
    assert!(!status.success(), "{transcript:?}\n{errors}");
    assert!(
        transcript.iter().any(|line| {
            line.contains("unknown outcome") && line.contains("execution was lost")
        }),
        "the run's lost execution is reported instead of awaited: {transcript:?}\n{errors}"
    );
    assert!(
        transcript
            .iter()
            .filter(|line| line.starts_with("Accepted: Run"))
            .nth(1)
            .is_none(),
        "nothing is resent: {transcript:?}"
    );
    use tickloop_agent_protocol::AgentAccess as _;
    let client =
        tickloop_agent_client::LocalAgentClient::connect(runtime_directory.join("daemon.sock"))
            .await
            .unwrap();
    let catalog = client
        .observe(tickloop_agent_protocol::ObservationRequest::Overview {
            filter: Default::default(),
            page: tickloop_agent_protocol::Page {
                offset: 0,
                limit: 50,
            },
            after: None,
        })
        .await
        .unwrap();
    let tickloop_agent_protocol::ObservationValue::Overview(rows) = catalog.initial else {
        panic!("expected the daemon catalog");
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].availability,
        tickloop_agent_protocol::ExecutionAvailability::Lost,
        "the daemon does not present the lost worker as live"
    );
    server.abort();
    client.shutdown_daemon().await.unwrap();
    tokio::time::timeout(INTERACTION_TIMEOUT, async {
        while runtime_directory.join("daemon.sock").exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the test's private daemon removes its socket after shutdown");
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
        .arg("--transport-fixture")
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
        .arg("--transport-fixture")
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
async fn terminal_can_stop_a_running_turn_and_show_the_paused_loop() {
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
        .arg("--transport-fixture")
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
    input.write_all(b"/stop\n/status\n").await.unwrap();
    drop(input);
    let output = tokio::time::timeout(INTERACTION_TIMEOUT, child.wait_with_output())
        .await
        .expect("stop must cancel the request and allow EOF to exit")
        .unwrap();
    server.abort();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.contains("Stopped. Queued messages were discarded"),
        "{text}"
    );
    assert!(text.contains("Interrupted run"), "{text}");
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
        .arg("--transport-fixture")
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
            messages_stream(&tickloop_agent_test_support::auto_classification(false).to_string()),
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
        !String::from_utf8(output.stdout)
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
            .contains("Decide whether the proposed operation should be blocked")
    );
}

#[test]
fn legacy_independent_reviewer_flags_report_the_permissions_migration() {
    for flag in ["--auto-reviewer-config", "--permission-checker-config"] {
        let output = support::fixture_cli::command()
            .env(
                "TICKLOOP_DAEMON_RUNTIME_DIR",
                support::cli_runtime::directory(),
            )
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
        for turn in 0..4 {
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
                2 => tool_stream(
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
        .arg("--transport-fixture")
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
    tokio::time::timeout(INTERACTION_TIMEOUT, async {
        loop {
            let line = output.next_line().await.unwrap().unwrap();
            transcript.push_str(&line);
            if line.contains("No, keep planning") {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert!(transcript.contains("Create reviewed.txt with the approved result."));
    assert!(!directory.path().join("reviewed.txt").exists());
    input.write_all(b"2\n").await.unwrap();
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
        for (index, body) in [
            tool_stream(
                "Bash",
                json!({"command":"printf 'background marker'; while [ ! -f background-release ]; do sleep 0.01; done", "description":"Run the controlled background command"}),
            ),
            messages_stream("The command is in the background."),
            messages_stream("The stopped background command is accounted for."),
        ].into_iter().enumerate() {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_request(&mut socket).await;
            if index == 2 {
                let request = serde_json::to_string(&request).unwrap();
                assert!(request.contains("<task-notification>") && request.contains("<status>killed</status>"), "the final response must account for the stopped Shell: {request}");
            }
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
            socket.write_all(body.as_bytes()).await.unwrap();
        }
    });
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("model.json");
    std::fs::write(&config,serde_json::to_vec(&json!({"name":"fixture","protocol":"messages","base_url":format!("http://{address}"),"model":"fixture","api_key_env":"","max_output_tokens":256})).unwrap()).unwrap();
    let mut child = trusted_async_agent_command()
        .env("TICKLOOP_CONFIG_DIR", directory.path().join("config-root"))
        .arg("--transport-fixture")
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
        .write_all(format!("/command-output {operation}\n/stop-command {operation}\n").as_bytes())
        .await
        .unwrap();
    tokio::time::timeout(INTERACTION_TIMEOUT, async {
        let mut stopped_reply = false;
        loop {
            let line = output.next_line().await.unwrap().unwrap();
            transcript.push_str(&line);
            stopped_reply |= line.contains("The stopped background command is accounted for.");
            if stopped_reply && line.starts_with("Reply completed") {
                break;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the stopped Shell's continuation did not complete: {transcript}"));
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
    let search_config = tickloop_agent_protocol::SearchConfig {
        provider: tickloop_agent_protocol::SearchProviderKind::Searxng,
        endpoint: format!("http://{search_address}/search"),
        api_key_env: None,
    };
    UserSettings::in_root(&directory.path().join("config-root"))
        .save_search_config(Some(&search_config))
        .unwrap();
    let output = run_with_config(
        directory.path(),
        &["--no-session-persistence"],
        "/rule allow *\n/mode acceptEdits\nFind a source\n",
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
        .arg("--transport-fixture")
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
        .arg("--transport-fixture")
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
    let mut command = trusted_async_agent_command();
    let runtime_directory = support::cli_runtime::command_directory(command.as_std());
    let mut child = command
        .env("TICKLOOP_CONFIG_DIR", directory.path().join("config-root"))
        .arg("--transport-fixture")
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
    tokio::time::timeout(INTERACTION_TIMEOUT, async {
        let mut accepted = 0;
        while accepted < 2 {
            let line = output.next_line().await.unwrap().unwrap();
            if let Some(value) = line.strip_prefix("Chat: ") {
                chat = value.to_owned();
            }
            if line.starts_with("Accepted: Run ") {
                accepted += 1;
            }
        }
    })
    .await
    .expect("the second input must be accepted before the crash");
    support::cli_runtime::flush_transcripts(&runtime_directory)
        .await
        .unwrap();
    input.write_all(b"/exit\n").await.unwrap();
    drop(input);
    tokio::time::timeout(INTERACTION_TIMEOUT, child.wait())
        .await
        .unwrap()
        .unwrap();
    support::cli_runtime::shutdown_daemon(&runtime_directory)
        .await
        .unwrap();
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
    let mut command = trusted_async_agent_command();
    let runtime_directory = support::cli_runtime::command_directory(command.as_std());
    let mut child = command
        .env("TICKLOOP_CONFIG_DIR", directory.path().join("config-root"))
        .arg("--transport-fixture")
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
    let operation = tokio::time::timeout(INTERACTION_TIMEOUT, async {
        loop {
            let line = output.next_line().await.unwrap().unwrap();
            if let Some(value) = line.strip_prefix("Chat: ") {
                chat = value.to_owned();
            }
            if let Some(value) = line.strip_prefix("Command: ") {
                break value.split_whitespace().next().unwrap().to_owned();
            }
        }
    })
    .await
    .unwrap();
    support::cli_runtime::flush_transcripts(&runtime_directory)
        .await
        .unwrap();
    input.write_all(b"/exit\n").await.unwrap();
    drop(input);
    tokio::time::timeout(INTERACTION_TIMEOUT, child.wait())
        .await
        .unwrap()
        .unwrap();
    support::cli_runtime::shutdown_daemon(&runtime_directory)
        .await
        .unwrap();
    server.await.unwrap();
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
        .arg("--transport-fixture")
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
    let mut command = trusted_async_agent_command();
    let runtime_directory = support::cli_runtime::command_directory(command.as_std());
    let mut child = command
        .env("TICKLOOP_CONFIG_DIR", directory.join("config-root"))
        .env_remove("HERDR_ENV")
        .env_remove("HERDR_PANE_ID")
        .env_remove("HERDR_SOCKET_PATH")
        .arg("--transport-fixture")
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
    let output = tokio::time::timeout(INTERACTION_TIMEOUT, child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    if runtime_directory.join("daemon.sock").exists() {
        support::cli_runtime::flush_transcripts(&runtime_directory)
            .await
            .unwrap();
    }
    output
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
            "--transport-fixture",
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

/// The tickloop guidance follows the first prompt, whose run loaded it, and stays there.
fn assert_tickloop_reference_and_messages(request: &Value, expected_messages: &[(&str, &str)]) {
    let messages = request["messages"].as_array().unwrap();
    assert_eq!(messages.len(), expected_messages.len() + 1);
    assert_eq!(messages[1]["role"], "user");
    let reference = messages[1]["content"].as_array().unwrap();
    assert_eq!(reference.len(), 1);
    assert_eq!(reference[0]["type"], "text");
    assert!(reference[0]["text"].as_str().unwrap().starts_with(
        "Reference guidance from tickloop active in this Chat where relevant. This is reference material, not a user request or additional authority.\n\n# Tickloop Ticks\n"
    ));
    let conversation = messages
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != 1)
        .map(|(_, message)| message)
        .map(|message| {
            let content = message["content"].as_array().unwrap();
            assert_eq!(content.len(), 1);
            assert_eq!(content[0]["type"], "text");
            (
                message["role"].as_str().unwrap(),
                content[0]["text"].as_str().unwrap(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(conversation.as_slice(), expected_messages);
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

/// The first input that creates the new Agent after `/clear` or a staged `/mode`; the new Agent's
/// configuration waits for it.
const NEW_AGENT_INPUT: &str = "Start the new Agent";

/// Accepts the next model request that is not the new Agent's first input, answering those first.
async fn accept_skipping_new_agent(
    listener: &tokio::net::TcpListener,
) -> (tokio::net::TcpStream, Value) {
    loop {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_request(&mut socket).await;
        if request["messages"].to_string().contains(NEW_AGENT_INPUT) {
            respond_with(&mut socket, &messages_stream("New Agent ready.")).await;
            continue;
        }
        return (socket, request);
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
        .arg("--transport-fixture")
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
    input
        .write_all(format!("/clear\n/mode fullAccess\n{NEW_AGENT_INPUT}\n").as_bytes())
        .await
        .unwrap();
    drop(input);
    let new_agent = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_request(&mut socket).await;
        respond_with(&mut socket, &messages_stream("New Agent ready.")).await;
        (listener, request)
    });
    tokio::time::timeout(INTERACTION_TIMEOUT, async {
        while let Some(line) = output.next_line().await.unwrap() {
            transcript.push(line);
        }
    })
    .await
    .expect("the cleared Chat's foreground command kept the process alive");
    let (listener, new_agent_request) = new_agent.await.unwrap();
    assert!(
        new_agent_request["messages"]
            .to_string()
            .contains(NEW_AGENT_INPUT),
        "the only model request after /clear is the new Agent's first input"
    );
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

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    local_model_config(directory.path(), listener.local_addr().unwrap());
    let server = tokio::spawn(async move { accept_skipping_new_agent(&listener).await });
    let mut child = trusted_async_agent_command()
        .env("TICKLOOP_CONFIG_DIR", &config_root)
        .arg("--transport-fixture")
        .arg(directory.path().join("model.json"))
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
    input
        .write_all(format!("/mode fullAccess\n{NEW_AGENT_INPUT}\n").as_bytes())
        .await
        .unwrap();
    let first_chat = next_chat_line(&mut output).await;
    tokio::time::timeout(INTERACTION_TIMEOUT, async {
        while registered_chat(&registration).as_deref() != Some(first_chat.as_str())
            || stale.exists()
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("startup did not register its Chat and remove the stale process registration");
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

    input
        .write_all(format!("/clear\n/mode fullAccess\n{NEW_AGENT_INPUT}\n").as_bytes())
        .await
        .unwrap();
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
    server.abort();
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
        .arg("--transport-fixture")
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
    let (final_sender, final_request) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        wait.await.unwrap();
        respond_with(&mut socket, &tool_stream("Read", tool_input)).await;
        let (mut socket, request) = accept_skipping_new_agent(&listener).await;
        respond_with(&mut socket, &messages_stream("Background note read.")).await;
        final_sender.send(request).unwrap();
        // The new Agent's first request may come after the background Chat's.
        accept_skipping_new_agent(&listener).await;
    });
    let directory = tempfile::tempdir().unwrap();
    local_model_config(directory.path(), address);
    let mut child = spawn_piped_terminal(directory.path());
    let mut input = child.stdin.take().unwrap();
    let mut output = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    let mut transcript = Vec::new();
    input.write_all(b"Read the external note\n").await.unwrap();
    let original_chat = next_chat_line(&mut output).await;
    input
        .write_all(format!("/clear\n/mode fullAccess\n{NEW_AGENT_INPUT}\n").as_bytes())
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
    let final_request = tokio::time::timeout(INTERACTION_TIMEOUT, final_request)
        .await
        .unwrap()
        .unwrap();
    server.abort();
    assert!(
        final_request["messages"]
            .to_string()
            .contains("approved background note")
    );
}

/// Plain terminal output read as raw bytes, so text streamed without a line feed is visible.
struct RawOutput {
    stdout: tokio::process::ChildStdout,
    bytes: Vec<u8>,
}

impl RawOutput {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
    async fn read_until(&mut self, done: impl Fn(&str) -> bool) {
        let mut chunk = [0_u8; 4096];
        let waited = tokio::time::timeout(INTERACTION_TIMEOUT, async {
            while !done(&self.text()) {
                let read = self.stdout.read(&mut chunk).await.unwrap();
                assert!(read > 0, "output ended early: {}", self.text());
                self.bytes.extend_from_slice(&chunk[..read]);
            }
        })
        .await;
        assert!(
            waited.is_ok(),
            "expected output did not appear: {}",
            self.text()
        );
    }
    /// Waits for a complete line containing `needle` and returns it.
    async fn line_containing(&mut self, needle: &str) -> String {
        let complete_line = |text: &str| {
            text.rsplit_once('\n').and_then(|(complete, _)| {
                complete
                    .lines()
                    .find(|line| line.contains(needle))
                    .map(str::to_owned)
            })
        };
        self.read_until(|text| complete_line(text).is_some()).await;
        complete_line(&self.text()).unwrap()
    }
    async fn finish(
        mut self,
        mut child: tokio::process::Child,
    ) -> (std::process::ExitStatus, String) {
        let read = tokio::time::timeout(
            INTERACTION_TIMEOUT,
            self.stdout.read_to_end(&mut self.bytes),
        )
        .await;
        assert!(
            read.is_ok(),
            "the process kept its output open: {}",
            self.text()
        );
        let status = tokio::time::timeout(INTERACTION_TIMEOUT, child.wait())
            .await
            .unwrap()
            .unwrap();
        (status, self.text())
    }
}

#[tokio::test]
async fn a_background_permission_answered_during_a_partial_foreground_reply_gets_its_own_lines() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let external_path = outside.path().join("note.txt");
    std::fs::write(&external_path, "approved background note").unwrap();
    let tool_input = json!({"file_path":external_path});
    let (release_tool, tool_released) = tokio::sync::oneshot::channel::<()>();
    let (release_reply, reply_released) = tokio::sync::oneshot::channel::<()>();
    let follow_ups = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let answered = follow_ups.clone();
    let server = tokio::spawn(async move {
        let mut tool_released = Some(tool_released);
        let mut reply_released = Some(reply_released);
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_request(&mut socket).await;
            let messages = request["messages"].to_string();
            if messages.contains("approved background note") {
                answered.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                respond_with(&mut socket, &messages_stream("Background note read.")).await;
            } else if messages.contains(NEW_AGENT_INPUT) {
                // The foreground reply streams its first line without a line feed and then waits.
                let released = reply_released.take().expect("the new Agent asks once");
                tokio::spawn(async move {
                    let stream = messages_stream("Foreground reply");
                    let events = stream.split_inclusive("\n\n").collect::<Vec<_>>();
                    socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n").await.unwrap();
                    socket
                        .write_all(events[..3].concat().as_bytes())
                        .await
                        .unwrap();
                    let _ = released.await;
                    socket
                        .write_all(events[3..].concat().as_bytes())
                        .await
                        .unwrap();
                });
            } else {
                let released = tool_released.take().expect("the original Chat asks once");
                let tool_input = tool_input.clone();
                tokio::spawn(async move {
                    let _ = released.await;
                    respond_with(&mut socket, &tool_stream("Read", tool_input)).await;
                });
            }
        }
    });
    let directory = tempfile::tempdir().unwrap();
    local_model_config(directory.path(), address);
    let mut child = spawn_piped_terminal(directory.path());
    let mut input = child.stdin.take().unwrap();
    let mut output = RawOutput {
        stdout: child.stdout.take().unwrap(),
        bytes: Vec::new(),
    };
    input.write_all(b"Read the external note\n").await.unwrap();
    let original_chat = output.line_containing("Chat: ").await["Chat: ".len()..].to_owned();
    input
        .write_all(format!("/clear\n/mode fullAccess\n{NEW_AGENT_INPUT}\n").as_bytes())
        .await
        .unwrap();
    output
        .read_until(|text| {
            text.lines().any(|line| {
                line.strip_prefix("Chat: ")
                    .is_some_and(|chat| chat != original_chat)
            })
        })
        .await;
    output
        .read_until(|text| text.contains("Foreground reply"))
        .await;
    release_tool.send(()).unwrap();
    let original_label = format!("Background Chat {}", &original_chat.replace('-', "")[..8]);
    let request_notice = format!("{original_label} · permission request ");
    let notice = output.line_containing(&request_notice).await;
    let request_id = notice
        .split(&request_notice)
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
    output
        .line_containing("Permission decision accepted.")
        .await;
    output
        .line_containing(&format!("{original_label} · reply completed"))
        .await;
    release_reply.send(()).unwrap();
    drop(input);
    let (status, text) = output.finish(child).await;
    server.abort();
    assert!(status.success(), "{text}");
    let lines = text.lines().collect::<Vec<_>>();
    assert!(
        lines.contains(&"Foreground reply"),
        "the streamed reply keeps its text: {text}"
    );
    assert!(
        lines.iter().any(|line| line.starts_with(&request_notice)),
        "the background request starts its own line: {text}"
    );
    assert!(
        lines.contains(&"Permission decision accepted."),
        "the decision receipt starts its own line: {text}"
    );
    assert_eq!(
        follow_ups.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the approved tool result reached the model once: {text}"
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
    let (merged_sender, merged) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        wait.await.unwrap();
        respond_with(&mut socket, &tool_stream("Read", tool_input)).await;
        let (mut socket, merged_request) = accept_skipping_new_agent(&listener).await;
        respond_with(&mut socket, &messages_stream("Correction applied.")).await;
        merged_sender.send(merged_request).unwrap();
        // The new Agent's first request may come after the merged one.
        accept_skipping_new_agent(&listener).await;
    });
    local_model_config(directory.path(), address);
    let mut child = spawn_piped_terminal(directory.path());
    let mut input = child.stdin.take().unwrap();
    let mut output = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    let mut transcript = Vec::new();
    input.write_all(b"Read the note\n").await.unwrap();
    let original_chat = wait_for_line(&mut output, &mut transcript, |line| {
        line.starts_with("Chat: ")
    })
    .await
    .strip_prefix("Chat: ")
    .unwrap()
    .to_owned();
    input
        .write_all(
            format!("Also apply this correction\n/clear\n/mode fullAccess\n{NEW_AGENT_INPUT}\n")
                .as_bytes(),
        )
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
    let merged_request = tokio::time::timeout(INTERACTION_TIMEOUT, merged)
        .await
        .unwrap()
        .unwrap();
    server.abort();
    assert!(
        merged_request["messages"]
            .to_string()
            .contains("Also apply this correction"),
        "the queued input must join the running reply: {merged_request}"
    );
}

/// `cleanupPeriodDays` 为 0 时不保存 Chat：`-c` 说明原因，帮助文本介绍这个设置。
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

async fn daemon_catalog(
    runtime_directory: &std::path::Path,
) -> Vec<tickloop_agent_protocol::AgentSummary> {
    use tickloop_agent_client::LocalAgentClient;
    use tickloop_agent_protocol::{
        AgentAccess, AgentFilter, ObservationRequest, ObservationValue, Page,
    };
    let client = LocalAgentClient::connect(runtime_directory.join("daemon.sock"))
        .await
        .unwrap();
    let observation = client
        .observe(ObservationRequest::Overview {
            filter: AgentFilter::default(),
            page: Page {
                offset: 0,
                limit: 64,
            },
            after: None,
        })
        .await
        .unwrap();
    let ObservationValue::Overview(rows) = observation.initial else {
        panic!("expected the daemon catalog");
    };
    rows
}

#[tokio::test]
async fn new_agent_configuration_waits_for_the_first_input_to_create_the_agent() {
    let directory = tempfile::tempdir().unwrap();
    let config_root = directory.path().join("config-root");
    let mut command = trusted_async_agent_command();
    command
        .env("TICKLOOP_CONFIG_DIR", &config_root)
        .arg("--cwd")
        .arg(directory.path())
        .env("DEEPSEEK_API_KEY", "fixture-menu-key")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let runtime_directory = support::cli_runtime::command_directory(command.as_std());
    let mut child = command.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            b"/mode auto\n/thinking\ncancel\n/settings\ncancel\n/permissions\n/rules\n/exit\n",
        )
        .await
        .unwrap();
    let output = tokio::time::timeout(INTERACTION_TIMEOUT, child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    let transcript = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "{transcript}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!transcript.contains("Chat: "), "{transcript}");
    assert!(
        daemon_catalog(&runtime_directory).await.is_empty(),
        "configuring the New Agent registers no execution: {transcript}"
    );

    let mut child = trusted_async_agent_command()
        .env("TICKLOOP_DAEMON_RUNTIME_DIR", &runtime_directory)
        .env("TICKLOOP_CONFIG_DIR", &config_root)
        .arg("--cwd")
        .arg(directory.path())
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
        .write_all(b"/mode auto\n!printf first-input\n/exit\n")
        .await
        .unwrap();
    let output = tokio::time::timeout(INTERACTION_TIMEOUT, child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    let transcript = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(output.status.success(), "{transcript}");
    let rows = daemon_catalog(&runtime_directory).await;
    assert_eq!(
        rows.len(),
        1,
        "the first input creates exactly one Agent: {transcript}"
    );
    assert_eq!(rows[0].has_work, Some(true));
    let client =
        tickloop_agent_client::LocalAgentClient::connect(runtime_directory.join("daemon.sock"))
            .await
            .unwrap();
    let observation = tickloop_agent_protocol::AgentAccess::observe(
        &client,
        tickloop_agent_protocol::ObservationRequest::Agent {
            execution: rows[0].execution,
            detail: tickloop_agent_protocol::AgentDetailQuery::Snapshot,
            after: None,
        },
    )
    .await
    .unwrap();
    let tickloop_agent_protocol::ObservationValue::Agent(snapshot) = observation.initial else {
        panic!("expected the created Agent");
    };
    assert_eq!(
        snapshot.policy.mode,
        tickloop_agent_protocol::ExecutionMode::Auto,
        "the staged mode replaces the startup acceptEdits mode before the first input runs"
    );
}
