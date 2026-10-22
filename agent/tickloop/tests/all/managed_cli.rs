use std::{
    ffi::OsString,
    process::{Command, Stdio},
};

use tickloop_agent::{ClientEntry, client_entry};

fn entry(values: &[&str]) -> ClientEntry {
    client_entry(&values.iter().map(OsString::from).collect::<Vec<_>>())
}

#[test]
fn managed_dispatch_respects_option_values_and_prompt_boundaries() {
    assert_eq!(entry(&["-p", "hello"]), ClientEntry::Print);
    assert_eq!(
        entry(&["--cwd", "/tmp", "--print", "hello"]),
        ClientEntry::Print
    );
    assert_eq!(entry(&["--p", "hello"]), ClientEntry::Print); // reject in print parser, exit 2
    assert_eq!(entry(&["acp"]), ClientEntry::Acp);
    assert_eq!(entry(&["--cwd", "-p"]), ClientEntry::Console);
    assert_eq!(entry(&["--", "-p"]), ClientEntry::Console);
    assert_eq!(entry(&["ordinary prompt", "-p"]), ClientEntry::Console);
    assert_eq!(entry(&["worker", "-p"]), ClientEntry::Internal);
    assert_eq!(entry(&["tick", "install", "-p"]), ClientEntry::Internal);
}

#[test]
fn resume_chat_id_is_an_option_value_not_a_console_prompt() {
    const CHAT: &str = "0190a5b2-7c1e-7000-8000-000000000001";
    assert_eq!(entry(&["-r", CHAT, "-p", "hi"]), ClientEntry::Print);
    assert_eq!(
        entry(&["--resume", CHAT, "--print", "hi"]),
        ClientEntry::Print
    );
    assert_eq!(entry(&["-r", "-p", "hi"]), ClientEntry::Print);
    assert_eq!(entry(&["-r", CHAT, "prompt", "-p"]), ClientEntry::Console);
}

#[tokio::test]
async fn print_missing_provider_fails_without_interactive_login() {
    let root = tempfile::tempdir().unwrap();
    // Credential safety rejects symlink ancestors (for example /var on macOS).
    // Exercise missing provider configuration, not an unsafe fixture path.
    let folder = root.path().canonicalize().unwrap();
    let runtime = super::support::cli_runtime::directory();
    let mut command = super::support::fixture_cli::tokio_command();
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &folder)
        .env("TICKLOOP_CONFIG_DIR", folder.join("config"))
        .env("TICKLOOP_DAEMON_RUNTIME_DIR", &runtime)
        .env("TICKLOOP_TICKHOST_RUNTIME_DIR", runtime.join("tickhost"))
        .current_dir(&folder)
        .args([
            "-p",
            "hello",
            "--working-directory-is-trusted",
            "--timeout",
            "10",
            "--output-format",
            "json",
        ])
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(std::time::Duration::from_secs(30), command.output())
        .await
        .expect("missing provider must fail promptly")
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["status"], "failed");
    let error = result["error"].as_str().unwrap().to_lowercase();
    assert!(
        error.contains("api key") || error.contains("provider"),
        "{error}"
    );
    assert!(!output.stderr.contains(&0x1b));
}

fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tickloop"));
    command
        .stdin(Stdio::null())
        .env_remove("HERDR_SOCKET")
        .env("NO_COLOR", "1");
    command
}

#[test]
fn print_parameter_errors_do_not_open_console_or_write_stdout() {
    for args in [
        vec!["--p", "hello"],
        vec!["-p", "--output-format", "bad"],
        vec!["-p", "--resume"],
        vec!["-r", "0190a5b2-7c1e-7000-8000-000000000001", "-p", "hi"],
        vec![
            "--resume",
            "0190a5b2-7c1e-7000-8000-000000000001",
            "--print",
            "hi",
        ],
    ] {
        let output = command().args(args).output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(2),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.contains(&0x1b), "stderr must be ANSI-free");
    }
}

#[test]
fn managed_help_does_not_connect_or_start_tui() {
    for args in [
        vec!["-p", "--help"],
        vec!["--print", "--help"],
        vec!["acp", "--help"],
    ] {
        let acp = args[0] == "acp";
        let output = command().args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        if acp {
            assert!(
                stdout.is_empty(),
                "ACP stdout is reserved for protocol frames"
            );
            assert!(!output.stderr.is_empty());
        } else {
            assert!(!stdout.is_empty());
        }
        assert!(!stdout.contains('\u{1b}'));
    }
}
