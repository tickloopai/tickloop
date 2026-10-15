use std::{fs, path::PathBuf, process::Command};

const UTF8_LOCALE: &str = "en_US.UTF-8";

fn tick() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tick"));
    command.env_clear();
    command.env("PATH", std::env::var("PATH").unwrap_or_default());
    command.env("LANG", UTF8_LOCALE);
    command.env("HOME", scratch_dir("tick-home"));
    command
}

fn scratch_dir(name: &str) -> PathBuf {
    let dir =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
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
fn tick_help_lists_every_provider_with_examples_and_exit_codes() {
    let (code, stdout, _) = run(tick().arg("--help"));
    assert_eq!(code, 0);
    for command in ["builtin", "longbridge", "futu", "reader", "describe"] {
        assert!(
            stdout.contains(&format!("\n  {command}")),
            "missing {command}:\n{stdout}"
        );
    }
    assert!(!stdout.contains("\n  demo"), "{stdout}");
    assert!(
        stdout.contains("tick longbridge quote TSLA.US 700.HK"),
        "{stdout}"
    );
    assert!(
        stdout.contains("tick longbridge earnings TSLA.US --count 12"),
        "{stdout}"
    );
    assert!(stdout.contains("tick builtin kline 700.HK"), "{stdout}");
    assert!(stdout.contains("0  success"), "{stdout}");
    assert!(
        stdout.contains("2  usage or configuration error"),
        "{stdout}"
    );
    assert!(stdout.contains("3  not authorized"), "{stdout}");
    assert!(stdout.contains("4  network or API error"), "{stdout}");
    assert!(stdout.contains("--config"), "{stdout}");
    assert!(stdout.contains("--color-mode"), "{stdout}");
}

#[test]
fn every_provider_lists_its_ticks_and_every_tick_has_its_own_help() {
    let ticks = [
        "quote",
        "watchlist",
        "kline",
        "positions",
        "news",
        "earnings",
        "options",
        "community",
    ];
    for provider in ["builtin", "longbridge", "futu"] {
        let (code, stdout, _) = run(tick().args([provider, "--help"]));
        assert_eq!(code, 0, "{provider}");
        for command in ticks {
            assert!(
                stdout.contains(&format!("\n  {command}")),
                "{provider} misses {command}:\n{stdout}"
            );
            let (code, stdout, _) = run(tick().args([provider, command, "--help"]));
            assert_eq!(code, 0, "{provider} {command}");
            assert!(
                stdout.contains(&format!("Usage: tick {provider} {command}")),
                "{provider} {command}:\n{stdout}"
            );
        }
    }
    let (_, longbridge, _) = run(tick().args(["longbridge", "--help"]));
    assert!(longbridge.contains("\n  auth"), "{longbridge}");
    let (_, builtin, _) = run(tick().args(["builtin", "--help"]));
    assert!(builtin.contains("\n  reader"), "{builtin}");
}

#[test]
fn a_provider_refuses_a_tick_it_has_no_interface_for_and_names_the_providers_that_do() {
    let (code, stdout, stderr) = run(tick().args(["longbridge", "options", "AAPL.US"]));
    assert_eq!(code, 2, "{stderr}");
    assert!(stdout.is_empty(), "{stdout}");
    assert!(
        stderr.contains("longbridge does not implement OptionsApi"),
        "{stderr}"
    );
    assert!(stderr.contains("tick futu options"), "{stderr}");
    assert!(!stderr.contains("tick longbridge auth"), "{stderr}");

    let (code, _, stderr) = run(tick().args(["futu", "positions", "--once"]));
    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains("futu does not implement QuoteApi and TradeApi"),
        "{stderr}"
    );
    assert!(
        stderr.contains("tick builtin positions or tick longbridge positions"),
        "{stderr}"
    );
}

#[test]
fn removed_commands_are_unknown() {
    for command in [
        &["demo", "quote"][..],
        &["auth", "status"],
        &["quote", "TSLA"],
        &["options", "AAPL.US"],
    ] {
        let (code, stdout, stderr) = run(tick().args(command));
        assert_eq!(code, 2, "{command:?}: {stderr}");
        assert!(stdout.is_empty(), "{command:?}: {stdout}");
        assert!(
            stderr.contains("unrecognized subcommand"),
            "{command:?}: {stderr}"
        );
    }
}

#[test]
fn non_utf8_locale_is_rejected_before_anything_else_runs() {
    for (name, value) in [
        ("LC_ALL", "C"),
        ("LC_ALL", "zh_CN.GB18030"),
        ("LANG", "POSIX"),
    ] {
        let mut command = tick();
        command.env_remove("LANG");
        command.env(name, value);
        let (code, _, stderr) = run(command.args(["longbridge", "positions"]));
        assert_eq!(code, 2, "{name}={value}");
        assert!(stderr.contains("UTF-8"), "{name}={value}: {stderr}");
        assert!(stderr.contains(value), "{name}={value}: {stderr}");
    }
}

#[test]
fn lc_all_takes_precedence_over_lang_when_checking_locale() {
    let mut command = tick();
    command.env("LC_ALL", "C.UTF-8");
    command.env("LANG", "C");
    let (_, _, stderr) = run(command.args(["longbridge", "positions"]));
    assert!(!stderr.contains("UTF-8"), "{stderr}");

    let mut command = tick();
    command.env("LC_ALL", "C");
    command.env("LANG", UTF8_LOCALE);
    let (code, _, stderr) = run(command.args(["longbridge", "positions"]));
    assert_eq!(code, 2);
    assert!(stderr.contains("LC_ALL=C"), "{stderr}");
}

#[test]
fn help_works_regardless_of_locale() {
    let mut command = tick();
    command.env("LANG", "C");
    let (code, stdout, _) = run(command.args(["longbridge", "quote", "--help"]));
    assert_eq!(code, 0);
    assert!(stdout.contains("Usage: tick longbridge quote"), "{stdout}");
}

#[test]
fn missing_locale_variables_are_reported_as_unknown() {
    let mut command = tick();
    command.env_remove("LANG");
    let (code, _, stderr) = run(command.args(["longbridge", "positions"]));
    assert_eq!(code, 2);
    assert!(stderr.contains("LANG"), "{stderr}");
}

#[test]
fn invalid_configuration_file_explains_the_problem_and_exits_with_usage_code() {
    let dir = scratch_dir("bad-config");
    let path = dir.join("config.toml");
    fs::write(&path, "color_mode = 'purple'\n").unwrap();
    let (code, _, stderr) = run(tick().args([
        "--config",
        path.to_str().unwrap(),
        "longbridge",
        "positions",
    ]));
    assert_eq!(code, 2);
    assert!(stderr.contains("purple"), "{stderr}");
    assert!(stderr.contains(path.to_str().unwrap()), "{stderr}");
}

#[test]
fn explicit_configuration_path_must_exist() {
    let dir = scratch_dir("missing-config");
    let path = dir.join("nope.toml");
    let (code, _, stderr) = run(tick().args([
        "--config",
        path.to_str().unwrap(),
        "longbridge",
        "positions",
    ]));
    assert_eq!(code, 2);
    assert!(stderr.contains("nope.toml"), "{stderr}");
}

#[test]
fn default_configuration_path_may_be_absent() {
    let (code, _, stderr) = run(tick().args(["longbridge", "quote", "--help"]));
    assert_eq!(code, 0, "{stderr}");
}

#[test]
fn market_commands_without_authorization_explain_how_to_log_in() {
    for command in [
        "watchlist",
        "quote",
        "kline",
        "positions",
        "news",
        "community",
    ] {
        let mut process = tick();
        process.args(["longbridge", command]);
        if matches!(command, "quote" | "kline") {
            process.arg("TSLA");
        }
        if command == "community" {
            process.arg("91");
        }
        let (code, stdout, stderr) = run(&mut process);
        assert_eq!(code, 3, "{command}: {stderr}");
        assert!(stdout.is_empty(), "{stdout}");
        assert!(stderr.contains("tick longbridge auth"), "{stderr}");
    }
}

#[test]
fn hidden_fundamentals_probe_is_noninteractive_and_exits_three_without_authorization() {
    let missing_home = scratch_dir("probe-missing-auth");
    let report = missing_home.join("probe.md");
    let (code, stdout, stderr) = run(tick().env("HOME", &missing_home).args([
        "longbridge",
        "probe-fundamentals",
        "TSLA.US",
        "--out",
        report.to_str().unwrap(),
    ]));
    assert_eq!(code, 3, "{stderr}");
    assert!(stdout.is_empty(), "{stdout}");
    assert!(stderr.contains("tick longbridge auth"), "{stderr}");
    assert!(!report.exists(), "an unauthorized probe writes no report");

    let (code, stdout, _) = run(tick().args(["longbridge", "--help"]));
    assert_eq!(code, 0);
    assert!(!stdout.contains("probe-fundamentals"), "{stdout}");

    let (code, stdout, stderr) = run(tick().args(["longbridge", "probe-fundamentals", "--help"]));
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stdout.contains("Usage: tick longbridge probe-fundamentals"),
        "{stdout}"
    );
}

#[test]
fn auth_status_and_logout_work_without_a_login_or_region_prompt() {
    let (code, stdout, stderr) = run(tick().args(["longbridge", "auth", "status"]));
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("Not logged in"), "{stdout}");
    assert!(stdout.contains("Region: not configured"), "{stdout}");
    let (code, stdout, stderr) = run(tick().args(["longbridge", "auth", "logout"]));
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("Logged out"), "{stdout}");
}

#[test]
fn hidden_auth_check_uses_exit_status_for_noninteractive_callers() {
    let missing_home = scratch_dir("auth-check-missing");
    let (code, stdout, stderr) =
        run(tick()
            .env("HOME", missing_home)
            .args(["longbridge", "auth", "check"]));
    assert_eq!(code, 3, "{stderr}");
    assert!(stdout.is_empty(), "{stdout}");
    assert!(stderr.contains("tick longbridge auth"), "{stderr}");

    let authorized_home = scratch_dir("auth-check-authorized");
    fs::create_dir_all(authorized_home.join(".tickloop/longbridge")).unwrap();
    fs::write(
        authorized_home.join(".tickloop/longbridge/auth.json"),
        r#"{"client_id":"test","access_token":"ap_fixture","refresh_token":null,"expires_at":4102444800,"logged_in_at":1}"#,
    )
    .unwrap();
    let (code, stdout, stderr) =
        run(tick()
            .env("HOME", authorized_home)
            .args(["longbridge", "auth", "check"]));
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.is_empty(), "{stdout}");
    assert!(stderr.is_empty(), "{stderr}");

    let (code, stdout, stderr) = run(tick().args(["longbridge", "auth", "--help"]));
    assert_eq!(code, 0, "{stderr}");
    assert!(!stdout.contains("check"), "{stdout}");
}

#[test]
fn quote_once_json_without_auth_emits_no_snapshot_and_exits_three() {
    let (code, stdout, stderr) = run(tick().args([
        "longbridge",
        "quote",
        "TSLA",
        "700",
        "--once",
        "--format",
        "json",
    ]));
    assert_eq!(code, 3, "{stderr}");
    assert!(stdout.is_empty(), "{stdout}");
    assert!(stderr.contains("tick longbridge auth"), "{stderr}");
}

#[test]
fn authenticated_interactive_quote_without_tty_explains_once_mode() {
    let home = scratch_dir("authenticated-home");
    fs::create_dir_all(home.join(".tickloop/longbridge")).unwrap();
    fs::write(home.join(".tickloop/longbridge/auth.json"), r#"{"client_id":"test","access_token":"ap_fixture","refresh_token":null,"expires_at":4102444800,"logged_in_at":1}"#).unwrap();
    let (code, stdout, stderr) =
        run(tick()
            .env("HOME", home)
            .args(["longbridge", "quote", "TSLA"]));
    assert_eq!(code, 2, "{stderr}");
    assert!(stdout.is_empty());
    assert!(
        stderr.contains("TTY") && stderr.contains("--once"),
        "{stderr}"
    );
}

#[test]
fn tick_descriptions_use_tick_terminology_and_minimum_panel_sizes() {
    let (code, stdout, stderr) = run(tick().env("LANG", "C").arg("describe"));
    assert_eq!(code, 0, "{stderr}");
    let ticks: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(ticks.as_array().unwrap().len(), 9);
    let descriptor = |kind: &str| {
        ticks
            .as_array()
            .unwrap()
            .iter()
            .find(|tick| tick["kind"] == kind)
            .unwrap_or_else(|| panic!("missing {kind} descriptor"))
            .clone()
    };
    let kline = descriptor("kline");
    assert_eq!(
        kline["minimum_panel_size"],
        serde_json::json!({"columns":45,"rows":14})
    );
    assert_eq!(kline["title"], "Kline");
    assert_eq!(kline["runtime"], "command");
    assert_eq!(kline["configuration"], "user_input");
    assert_eq!(kline["protocol"], 3);
    assert_eq!(kline["recover_with"], "candles");
    let kline_tools: Vec<(&str, bool)> = kline["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| {
            (
                tool["name"].as_str().unwrap(),
                tool["mutates"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        kline_tools,
        [
            ("candles", false),
            ("mark", true),
            ("unmark", true),
            ("event_history", false),
            ("set_event", true),
            ("set_context", true),
            ("set_view", true)
        ]
    );
    assert!(
        kline["description"]
            .as_str()
            .unwrap()
            .contains("candlestick")
    );
    let earnings = descriptor("earnings");
    assert_eq!(
        earnings["minimum_panel_size"],
        serde_json::json!({"columns":62,"rows":14})
    );
    assert_eq!(earnings["title"], "Earnings");
    assert_eq!(earnings["runtime"], "command");
    assert_eq!(earnings["configuration"], "user_input");
    assert_eq!(earnings["protocol"], 3);
    assert_eq!(earnings["recover_with"], "history");
    let tools = earnings["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 5);
    assert_eq!(tools[0]["name"], "history");
    assert_eq!(tools[0]["mutates"], false);
    assert_eq!(
        tools[0]["input_schema"]["properties"]["count"]["maximum"], 20,
        "the declared schema bounds the periods the Agent may ask for"
    );
    let community = descriptor("community");
    assert_eq!(community["configuration"], "user_input");
    assert_eq!(community["recover_with"], "posts");
    let tools = community["tools"].as_array().unwrap();
    assert_eq!(
        (tools[0]["name"].clone(), tools[0]["mutates"].clone()),
        (serde_json::json!("posts"), serde_json::json!(false))
    );
    assert_eq!(
        (tools[1]["name"].clone(), tools[1]["mutates"].clone()),
        (serde_json::json!("reply"), serde_json::json!(true))
    );
    assert_eq!(
        tools[1]["input_schema"]["properties"]["topic_id"]["format"],
        "int64"
    );
    assert!(
        ticks
            .as_array()
            .unwrap()
            .iter()
            .all(|tick| tick.get("module").is_none())
    );
}

#[test]
fn builtin_quote_is_available_without_credentials_and_labels_its_data() {
    let (code, stdout, stderr) = run(tick().args([
        "builtin", "quote", "TSLA", "700", "--once", "--format", "json",
    ]));
    assert_eq!(code, 0, "{stderr}");
    let rows: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(rows[0]["symbol"], "TSLA.US");
    assert_eq!(rows[1]["symbol"], "700.HK");
    assert!(rows[0]["name"].as_str().unwrap().contains("Demo"));
    assert!(!stdout.contains('\u{1b}'));
}

#[test]
fn builtin_runs_options_and_the_sample_reader_offline() {
    let (code, stdout, stderr) = run(tick().args([
        "builtin", "options", "DEMO.US", "--once", "--format", "json",
    ]));
    assert_eq!(code, 0, "{stderr}");
    let snapshot: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(snapshot["source"], "demo");
    assert_eq!(snapshot["symbol"], "DEMO.US");

    let (code, stdout, stderr) =
        run(tick().args(["builtin", "reader", "--once", "--format", "json"]));
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("DEMO"), "{stdout}");
}
