use std::process::Command;

const UTF8_LOCALE: &str = "en_US.UTF-8";

fn tick_mock(home: &tempfile::TempDir) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tick-mock"));
    command.env_clear();
    command.env("PATH", std::env::var("PATH").unwrap_or_default());
    command.env("LANG", UTF8_LOCALE);
    command.env("HOME", home.path());
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
fn a_quote_snapshot_completes_symbols_without_credentials_and_prints_plain_json() {
    let home = tempfile::tempdir().unwrap();
    let (code, stdout, stderr) =
        run(tick_mock(&home).args(["quote", "TSLA", "700", "--once", "--format", "json"]));
    assert_eq!(code, 0, "{stderr}");
    let rows: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(rows[0]["symbol"], "TSLA.US");
    assert_eq!(rows[1]["symbol"], "700.HK");
    assert!(!stdout.contains('\u{1b}'));
}

#[test]
fn an_options_snapshot_runs_offline() {
    let home = tempfile::tempdir().unwrap();
    let (code, stdout, stderr) =
        run(tick_mock(&home).args(["options", "DEMO.US", "--once", "--format", "json"]));
    assert_eq!(code, 0, "{stderr}");
    let snapshot: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(snapshot["source"], "mock");
    assert_eq!(snapshot["symbol"], "DEMO.US");
}
