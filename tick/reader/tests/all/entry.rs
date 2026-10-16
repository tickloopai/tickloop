use std::{fs, path::Path, process::Command};

const UTF8_LOCALE: &str = "en_US.UTF-8";
const RELEASE: &str = "# Quarterly results\n\nRevenue was $12.8 billion.\n";

fn tick_reader(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tick-reader"));
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
fn help_names_the_documents_describe_examples_and_exit_codes() {
    let home = tempfile::tempdir().unwrap();
    let (code, stdout, _) = run(tick_reader(home.path()).arg("--help"));
    assert_eq!(code, 0);
    assert!(
        stdout.contains("Read original financial documents from URLs or local files"),
        "{stdout}"
    );
    assert!(
        stdout.contains("Usage: tick-reader [OPTIONS] [DOCUMENTS]..."),
        "{stdout}"
    );
    assert!(stdout.contains("\n  describe"), "{stdout}");
    assert!(
        stdout.contains("tick-reader ./earnings-release.pdf"),
        "{stdout}"
    );
    for flag in [
        "--config",
        "--color-mode",
        "--language",
        "--ambiguous-width",
        "--size",
        "--symbol",
        "--fiscal-period",
        "--once",
    ] {
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
}

#[test]
fn describe_lists_only_the_reader() {
    let home = tempfile::tempdir().unwrap();
    let (code, stdout, stderr) = run(tick_reader(home.path()).env("LANG", "C").arg("describe"));
    assert_eq!(code, 0, "{stderr}");
    let ticks: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let ticks = ticks.as_array().unwrap();
    assert_eq!(ticks.len(), 1);
    let reader = &ticks[0];
    assert_eq!(reader["kind"], "reader");
    assert_eq!(reader["title"], "Reader");
    assert_eq!(reader["protocol"], 3);
    assert_eq!(reader["recover_with"], "documents");
    assert_eq!(
        reader["minimum_panel_size"],
        serde_json::json!({"columns":40,"rows":10})
    );
    let tools: Vec<_> = reader["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert!(tools.contains(&"search"), "{tools:?}");
    assert!(tools.contains(&"open_document"), "{tools:?}");
}

#[test]
fn reader_prints_a_local_original_once_without_a_provider_or_login() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("release.md");
    fs::write(&path, RELEASE).unwrap();
    let (code, stdout, stderr) =
        run(tick_reader(home.path()).args([path.to_str().unwrap(), "--once", "--format", "json"]));
    assert_eq!(code, 0, "{stderr}");
    let output: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(output["documents"].as_array().unwrap().len(), 1);
    assert!(stdout.contains("Revenue was $12.8 billion."), "{stdout}");
    assert!(!stdout.contains('\u{1b}'));
}

#[test]
fn a_local_file_named_describe_is_read_through_its_path() {
    let home = tempfile::tempdir().unwrap();
    fs::write(home.path().join("describe"), RELEASE).unwrap();
    let (code, stdout, stderr) = run(tick_reader(home.path()).current_dir(home.path()).args([
        "./describe",
        "--once",
        "--format",
        "json",
    ]));
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("Revenue was $12.8 billion."), "{stdout}");
    let (_, stdout, _) = run(tick_reader(home.path())
        .current_dir(home.path())
        .arg("describe"));
    let descriptors: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(descriptors[0]["kind"], "reader");
}

#[test]
fn describe_follows_the_global_flags_like_any_subcommand() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("config.toml");
    fs::write(&config, "language = 'en'\n").unwrap();
    let config = config.to_str().unwrap();
    for arguments in [
        &["--language", "en", "describe"][..],
        &["--config", config, "describe"],
        &["describe", "--color-mode", "green-up"],
    ] {
        let (code, stdout, stderr) = run(tick_reader(home.path()).args(arguments));
        assert_eq!(code, 0, "{arguments:?}: {stderr}");
        let descriptors: serde_json::Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(descriptors[0]["kind"], "reader", "{arguments:?}");
    }
}

#[test]
fn a_bare_describe_among_documents_is_refused_with_its_path_form() {
    let home = tempfile::tempdir().unwrap();
    fs::write(home.path().join("release.md"), RELEASE).unwrap();
    fs::write(home.path().join("describe"), RELEASE).unwrap();
    let (code, stdout, stderr) = run(tick_reader(home.path()).current_dir(home.path()).args([
        "./release.md",
        "describe",
        "--once",
    ]));
    assert_eq!(code, 2, "{stderr}");
    assert!(stdout.is_empty(), "{stdout}");
    assert!(stderr.contains("./describe"), "{stderr}");
}

#[test]
fn a_non_utf8_locale_is_rejected_before_reading() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("release.md");
    fs::write(&path, RELEASE).unwrap();
    let mut command = tick_reader(home.path());
    command.env("LC_ALL", "C");
    let (code, stdout, stderr) = run(command.arg(&path));
    assert_eq!(code, 2, "{stderr}");
    assert!(stdout.is_empty(), "{stdout}");
    assert!(stderr.starts_with("tick-reader: "), "{stderr}");
    assert!(stderr.contains("LC_ALL=C"), "{stderr}");
}

#[test]
fn a_tick_instance_refuses_the_one_shot_commands() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("release.md");
    fs::write(&path, RELEASE).unwrap();
    for arguments in [
        vec!["describe"],
        vec![path.to_str().unwrap(), "--once", "--format", "json"],
    ] {
        let (code, stdout, stderr) = run(tick_reader(home.path())
            .env("TICKLOOP_INSTANCE_ID", "hosted:one-shot")
            .args(&arguments));
        assert_eq!(code, 2, "{arguments:?}: {stderr}");
        assert!(stdout.is_empty(), "{arguments:?}: {stdout}");
        assert!(stderr.contains("requires a live Tick"), "{stderr}");
    }
}
