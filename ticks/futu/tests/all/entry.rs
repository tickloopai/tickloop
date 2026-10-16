use std::{path::Path, process::Command};

use crate::support::fake_opend;

const UTF8_LOCALE: &str = "en_US.UTF-8";

fn tick_futu(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tick-futu"));
    command.env_clear();
    command.env("PATH", std::env::var("PATH").unwrap_or_default());
    command.env("LANG", UTF8_LOCALE);
    command.env("HOME", home);
    command
}

fn run(command: &mut Command) -> (i32, String, String) {
    let output = command.output().unwrap();
    (
        output.status.code().unwrap(),
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

#[test]
fn help_lists_options_and_describe_with_the_opend_address_examples_and_exit_codes() {
    let home = tempfile::tempdir().unwrap();
    let (code, stdout, _) = run(tick_futu(home.path()).arg("--help"));
    assert_eq!(code, 0);
    for command in ["options", "describe"] {
        assert!(
            stdout.contains(&format!("\n  {command}")),
            "missing {command}:\n{stdout}"
        );
    }
    for absent in ["quote", "positions", "auth", "reader"] {
        assert!(!stdout.contains(&format!("\n  {absent}")), "{stdout}");
    }
    assert!(
        stdout.contains("tick-futu options AAPL.US --port 11111"),
        "{stdout}"
    );
    for flag in ["--host", "--port", "--config", "--color-mode"] {
        assert!(stdout.contains(flag), "missing {flag}:\n{stdout}");
    }
    assert!(!stdout.contains("--region"), "{stdout}");
    assert!(stdout.contains("0  success"), "{stdout}");
    assert!(
        stdout.contains("2  usage or configuration error"),
        "{stdout}"
    );
    assert!(stdout.contains("4  network or API error"), "{stdout}");
    assert!(!stdout.contains("not authorized"), "{stdout}");

    let (code, stdout, _) = run(tick_futu(home.path()).args(["options", "--help"]));
    assert_eq!(code, 0);
    assert!(stdout.contains("Usage: tick-futu options"), "{stdout}");
    assert!(stdout.contains("--host"), "{stdout}");
}

#[test]
fn describe_lists_only_the_options_tick() {
    let home = tempfile::tempdir().unwrap();
    let (code, stdout, stderr) = run(tick_futu(home.path()).env("LANG", "C").arg("describe"));
    assert_eq!(code, 0, "{stderr}");
    let ticks: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let ticks = ticks.as_array().unwrap();
    assert_eq!(ticks.len(), 1);
    let options = &ticks[0];
    assert_eq!(options["kind"], "options");
    assert_eq!(options["title"], "Futu Options");
    assert_eq!(options["protocol"], 3);
    assert_eq!(options["recover_with"], "chain");
    assert_eq!(
        options["minimum_panel_size"],
        serde_json::json!({"columns":58,"rows":14})
    );
}

#[test]
fn ticks_of_other_programs_and_removed_commands_are_unknown() {
    let home = tempfile::tempdir().unwrap();
    for command in [
        &["quote", "TSLA"][..],
        &["positions", "--once"],
        &["auth", "status"],
        &["reader", "./release.md"],
        &["futu", "options", "AAPL.US"],
    ] {
        let (code, stdout, stderr) = run(tick_futu(home.path()).args(command));
        assert_eq!(code, 2, "{command:?}: {stderr}");
        assert!(stdout.is_empty(), "{command:?}: {stdout}");
        assert!(
            stderr.contains("unrecognized subcommand"),
            "{command:?}: {stderr}"
        );
    }
}

#[test]
fn an_options_snapshot_prints_plain_json_from_opend() {
    let home = tempfile::tempdir().unwrap();
    let port = fake_opend::start().to_string();
    let port = port.as_str();
    for arguments in [
        [
            "options", "AAPL.US", "--port", port, "--once", "--format", "json",
        ],
        [
            "--port", port, "options", "AAPL.US", "--once", "--format", "json",
        ],
    ] {
        let (code, stdout, stderr) = run(tick_futu(home.path()).args(arguments));
        assert_eq!(code, 0, "{stderr}");
        assert!(!stdout.contains('\u{1b}'));
        let snapshot: serde_json::Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(snapshot["source"], "futu");
        assert_eq!(snapshot["symbol"], "AAPL.US");
        assert_eq!(snapshot["expiry"], fake_opend::EXPIRY);
        assert_eq!(snapshot["rows"].as_array().unwrap().len(), 1);
    }
}

#[test]
fn an_unreachable_opend_exits_four_and_a_remote_host_exits_two() {
    let home = tempfile::tempdir().unwrap();
    let port = fake_opend::closed_port().to_string();
    let (code, stdout, stderr) =
        run(tick_futu(home.path()).args(["options", "AAPL.US", "--port", port.as_str(), "--once"]));
    assert_eq!(code, 4, "{stderr}");
    assert!(stdout.is_empty(), "{stdout}");
    assert!(stderr.starts_with("tick-futu: "), "{stderr}");
    assert!(stderr.contains("cannot connect to Futu OpenD"), "{stderr}");

    let (code, _, stderr) =
        run(tick_futu(home.path()).args(["options", "AAPL.US", "--host", "192.0.2.1", "--once"]));
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("loopback"), "{stderr}");
}

#[test]
fn a_tick_instance_refuses_the_one_shot_commands() {
    let home = tempfile::tempdir().unwrap();
    for arguments in [&["describe"][..], &["options", "AAPL.US", "--once"]] {
        let (code, stdout, stderr) = run(tick_futu(home.path())
            .env("TICKLOOP_INSTANCE_ID", "hosted:one-shot")
            .args(arguments));
        assert_eq!(code, 2, "{arguments:?}: {stderr}");
        assert!(stdout.is_empty(), "{arguments:?}: {stdout}");
        assert!(stderr.contains("requires a live Tick"), "{stderr}");
    }
}
