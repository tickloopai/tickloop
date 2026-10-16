use std::{fs, path::PathBuf, process::Command};

use tick_longbridge::AUTH_HINT;

const UTF8_LOCALE: &str = "en_US.UTF-8";
const TICKS: [&str; 7] = [
    "quote",
    "watchlist",
    "kline",
    "positions",
    "news",
    "earnings",
    "community",
];
/// Where Tickloop keeps the login under a home directory's default config root.
const TOKEN_FILE: &str = ".tickloop/credentials/longbridge/oauth2.json";

fn tick_longbridge() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tick-longbridge"));
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

/// A token of the global server, which expires at `expires_at`.
fn token(expires_at: u64, refresh_token: Option<&str>) -> String {
    serde_json::json!({
        "access_token": "ap_fixture",
        "token_type": "Bearer",
        "refresh_token": refresh_token,
        "issuer": "https://openapi.longbridge.com",
        "client_id": "test",
        "token_endpoint": "https://openapi.longbridge.com/oauth2/token",
        "expires_at": expires_at,
    })
    .to_string()
}

/// A home directory whose default config root holds `saved` as the login.
fn home_with_login(name: &str, saved: &str) -> PathBuf {
    let home = scratch_dir(name);
    let token_file = home.join(TOKEN_FILE);
    fs::create_dir_all(token_file.parent().unwrap()).unwrap();
    fs::write(token_file, saved).unwrap();
    home
}

fn authorized_home(name: &str) -> PathBuf {
    home_with_login(name, &token(4102444800, None))
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
fn help_lists_every_tick_and_the_login_with_examples_and_exit_codes() {
    let (code, stdout, _) = run(tick_longbridge().arg("--help"));
    assert_eq!(code, 0);
    for command in TICKS.iter().chain(&["auth", "describe"]) {
        assert!(
            stdout.contains(&format!("\n  {command}")),
            "missing {command}:\n{stdout}"
        );
    }
    for absent in ["options", "reader", "probe-fundamentals", "demo", "builtin"] {
        assert!(!stdout.contains(&format!("\n  {absent}")), "{stdout}");
    }
    assert!(
        stdout.contains("tick-longbridge quote TSLA.US 700.HK"),
        "{stdout}"
    );
    assert!(
        stdout.contains("tick-longbridge earnings TSLA.US --count 12"),
        "{stdout}"
    );
    assert!(stdout.contains("0  success"), "{stdout}");
    assert!(
        stdout.contains("2  usage or configuration error"),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!("3  not authorized; {AUTH_HINT}")),
        "{stdout}"
    );
    assert!(stdout.contains("4  network or API error"), "{stdout}");
    for flag in ["--config", "--color-mode"] {
        assert!(stdout.contains(flag), "missing {flag}:\n{stdout}");
    }
    assert!(
        !stdout.contains("--region"),
        "a login picks the network: {stdout}"
    );
}

#[test]
fn every_tick_has_its_own_help() {
    for tick in TICKS {
        let (code, stdout, _) = run(tick_longbridge().args([tick, "--help"]));
        assert_eq!(code, 0, "{tick}");
        assert!(
            stdout.contains(&format!("Usage: tick-longbridge {tick}")),
            "{tick}:\n{stdout}"
        );
    }
}

#[test]
fn ticks_of_other_programs_and_removed_commands_are_unknown() {
    for command in [
        &["options", "AAPL.US"][..],
        &["reader", "./release.md"],
        &["futu", "options", "AAPL.US"],
        &["longbridge", "quote", "TSLA"],
        &["builtin", "kline", "700.HK"],
        &["demo", "quote"],
    ] {
        let (code, stdout, stderr) = run(tick_longbridge().args(command));
        assert_eq!(code, 2, "{command:?}: {stderr}");
        assert!(stdout.is_empty(), "{command:?}: {stdout}");
        assert!(
            stderr.contains("unrecognized subcommand"),
            "{command:?}: {stderr}"
        );
        assert!(!stderr.contains(AUTH_HINT), "{stderr}");
    }
    for command in [
        &["auth", "check"][..],
        &["auth", "status", "--json"],
        &["quote", "TSLA", "--region", "cn"],
    ] {
        let (code, stdout, stderr) = run(tick_longbridge().args(command));
        assert_eq!(code, 2, "{command:?}: {stderr}");
        assert!(stdout.is_empty(), "{command:?}: {stdout}");
    }
}

#[test]
fn non_utf8_locale_is_rejected_before_anything_else_runs() {
    for (name, value) in [
        ("LC_ALL", "C"),
        ("LC_ALL", "zh_CN.GB18030"),
        ("LANG", "POSIX"),
    ] {
        let mut command = tick_longbridge();
        command.env_remove("LANG");
        command.env(name, value);
        let (code, _, stderr) = run(command.arg("positions"));
        assert_eq!(code, 2, "{name}={value}");
        assert!(stderr.contains("UTF-8"), "{name}={value}: {stderr}");
        assert!(stderr.contains(value), "{name}={value}: {stderr}");
    }
}

#[test]
fn lc_all_takes_precedence_over_lang_when_checking_locale() {
    let mut command = tick_longbridge();
    command.env("LC_ALL", "C.UTF-8");
    command.env("LANG", "C");
    let (_, _, stderr) = run(command.arg("positions"));
    assert!(!stderr.contains("UTF-8"), "{stderr}");

    let mut command = tick_longbridge();
    command.env("LC_ALL", "C");
    command.env("LANG", UTF8_LOCALE);
    let (code, _, stderr) = run(command.arg("positions"));
    assert_eq!(code, 2);
    assert!(stderr.contains("LC_ALL=C"), "{stderr}");
}

#[test]
fn help_works_regardless_of_locale() {
    let mut command = tick_longbridge();
    command.env("LANG", "C");
    let (code, stdout, _) = run(command.args(["quote", "--help"]));
    assert_eq!(code, 0);
    assert!(stdout.contains("Usage: tick-longbridge quote"), "{stdout}");
}

#[test]
fn missing_locale_variables_are_reported_as_unknown() {
    let mut command = tick_longbridge();
    command.env_remove("LANG");
    let (code, _, stderr) = run(command.arg("positions"));
    assert_eq!(code, 2);
    assert!(stderr.contains("LANG"), "{stderr}");
}

#[test]
fn invalid_configuration_file_explains_the_problem_and_exits_with_usage_code() {
    let dir = scratch_dir("bad-config");
    let path = dir.join("config.toml");
    fs::write(&path, "color_mode = 'purple'\n").unwrap();
    let (code, _, stderr) =
        run(tick_longbridge().args(["--config", path.to_str().unwrap(), "positions"]));
    assert_eq!(code, 2);
    assert!(stderr.contains("purple"), "{stderr}");
    assert!(stderr.contains(path.to_str().unwrap()), "{stderr}");
}

#[test]
fn explicit_configuration_path_must_exist() {
    let dir = scratch_dir("missing-config");
    let path = dir.join("nope.toml");
    let (code, _, stderr) =
        run(tick_longbridge().args(["--config", path.to_str().unwrap(), "positions"]));
    assert_eq!(code, 2);
    assert!(stderr.contains("nope.toml"), "{stderr}");
}

#[test]
fn default_configuration_path_may_be_absent() {
    let (code, _, stderr) = run(tick_longbridge().args(["quote", "--help"]));
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
        let mut process = tick_longbridge();
        process.arg(command);
        if matches!(command, "quote" | "kline") {
            process.arg("TSLA");
        }
        if command == "community" {
            process.arg("91");
        }
        let (code, stdout, stderr) = run(&mut process);
        assert_eq!(code, 3, "{command}: {stderr}");
        assert!(stdout.is_empty(), "{stdout}");
        assert!(stderr.starts_with("tick-longbridge: "), "{stderr}");
        assert!(stderr.contains(AUTH_HINT), "{stderr}");
    }
}

#[test]
fn hidden_fundamentals_probe_is_noninteractive_and_exits_three_without_authorization() {
    let missing_home = scratch_dir("probe-missing-auth");
    let report = missing_home.join("probe.md");
    let (code, stdout, stderr) = run(tick_longbridge().env("HOME", &missing_home).args([
        "probe-fundamentals",
        "TSLA.US",
        "--out",
        report.to_str().unwrap(),
    ]));
    assert_eq!(code, 3, "{stderr}");
    assert!(stdout.is_empty(), "{stdout}");
    assert!(stderr.contains(AUTH_HINT), "{stderr}");
    assert!(!report.exists(), "an unauthorized probe writes no report");

    let (code, stdout, _) = run(tick_longbridge().arg("--help"));
    assert_eq!(code, 0);
    assert!(!stdout.contains("probe-fundamentals"), "{stdout}");

    let (code, stdout, stderr) = run(tick_longbridge().args(["probe-fundamentals", "--help"]));
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stdout.contains("Usage: tick-longbridge probe-fundamentals"),
        "{stdout}"
    );
}

#[test]
fn auth_status_and_logout_work_without_a_login_and_create_nothing() {
    let home = scratch_dir("logged-out-home");
    let (code, stdout, stderr) = run(tick_longbridge()
        .env("HOME", &home)
        .args(["auth", "status"]));
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stdout, format!("Not logged in; {AUTH_HINT}.\n"));
    let (code, stdout, stderr) = run(tick_longbridge()
        .env("HOME", &home)
        .args(["auth", "logout"]));
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("Logged out"), "{stdout}");
    assert!(
        !home.join(".tickloop").exists(),
        "logging out without a login creates no directory or lock"
    );
}

/// Logging out of a token file in a directory the user made, such as the home directory,
/// deletes the token and leaves the directory's permissions as they were.
#[test]
fn logging_out_of_a_named_token_file_leaves_its_directory_alone() {
    use std::os::unix::fs::PermissionsExt;
    let shared = scratch_dir("logout-shared");
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o755)).unwrap();
    let token_file = shared.join("oauth2.json");
    fs::write(&token_file, token(4102444800, None)).unwrap();

    let (code, stdout, stderr) = run(tick_longbridge()
        .env("TICKLOOP_OAUTH_TOKEN_FILE", &token_file)
        .args(["auth", "logout"]));

    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("Logged out"), "{stdout}");
    assert!(!token_file.exists());
    assert_eq!(
        fs::metadata(&shared).unwrap().permissions().mode() & 0o777,
        0o755
    );
}

/// The status reads the saved login alone and renews nothing; a lapsed access token still
/// counts as logged in while a refresh token can renew it.
#[test]
fn auth_status_reads_the_saved_login_alone() {
    let cases = [
        (
            "auth-text-authorized",
            token(4102444800, None),
            "Logged in\nExpires: 2100-01-01T00:00:00Z\nRegion: global\n",
        ),
        (
            "auth-text-renewable",
            token(1, Some("ap_refresh")),
            "Logged in; the access token is renewed when a Tick opens\n\
             Access token expired: 1970-01-01T00:00:01Z\nRegion: global\n",
        ),
        (
            "auth-text-expired",
            token(1, None),
            "Authorization expired; log in with /ticks in the Agent, or `tickloop tick login \
             longbridge` in a terminal.\nExpires: 1970-01-01T00:00:01Z\nRegion: global\n",
        ),
    ];
    for (name, saved, expected) in cases {
        let home = home_with_login(name, &saved);
        let (code, stdout, stderr) = run(tick_longbridge()
            .env("HOME", &home)
            .args(["auth", "status"]));
        assert_eq!(code, 0, "{name}: {stderr}");
        assert_eq!(stdout, expected, "{name}");
        assert_eq!(
            fs::read_to_string(home.join(TOKEN_FILE)).unwrap(),
            saved,
            "{name}: the status renews nothing"
        );
    }

    let home = home_with_login("auth-text-unreadable", "{");
    let (code, stdout, stderr) = run(tick_longbridge()
        .env("HOME", &home)
        .args(["auth", "status"]));
    assert_eq!(code, 3, "{stderr}");
    assert!(stdout.is_empty(), "{stdout}");
    assert!(stderr.contains(AUTH_HINT), "{stderr}");

    let (code, stdout, stderr) = run(tick_longbridge()
        .env("HOME", &home)
        .args(["auth", "logout"]));
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("Logged out"), "{stdout}");
    assert!(!home.join(TOKEN_FILE).exists());
}

/// The login is the one in the token file Tickloop names when it starts the program; run on its
/// own, the program finds it in Tickloop's config root, which TICKLOOP_CONFIG_DIR moves.
#[test]
fn the_login_is_found_where_tickloop_keeps_it() {
    let status = |command: &mut Command| {
        let (code, stdout, stderr) = run(command.args(["auth", "status"]));
        assert_eq!(code, 0, "{stderr}");
        stdout
    };
    let logged_out = format!("Not logged in; {AUTH_HINT}.\n");
    let home = authorized_home("login-in-home");
    assert!(status(tick_longbridge().env("HOME", &home)).starts_with("Logged in\n"));

    let config_root = scratch_dir("login-config-root");
    assert_eq!(
        status(
            tick_longbridge()
                .env("HOME", &home)
                .env("TICKLOOP_CONFIG_DIR", &config_root)
        ),
        logged_out,
        "TICKLOOP_CONFIG_DIR moves the config root"
    );
    assert!(
        status(
            tick_longbridge()
                .env("HOME", &home)
                .env("TICKLOOP_CONFIG_DIR", "")
        )
        .starts_with("Logged in\n"),
        "an empty TICKLOOP_CONFIG_DIR is no config root"
    );

    let named = scratch_dir("login-named").join("credentials/longbridge/oauth2.json");
    assert_eq!(
        status(
            tick_longbridge()
                .env("HOME", &home)
                .env("TICKLOOP_OAUTH_TOKEN_FILE", &named)
        ),
        logged_out,
        "the named token file is the login"
    );
    fs::create_dir_all(named.parent().unwrap()).unwrap();
    fs::write(&named, token(4102444800, None)).unwrap();
    assert!(
        status(
            tick_longbridge()
                .env("TICKLOOP_CONFIG_DIR", &config_root)
                .env("TICKLOOP_OAUTH_TOKEN_FILE", &named)
        )
        .starts_with("Logged in\n")
    );

    for invalid in ["oauth2.json", "/tmp/token.json"] {
        let (code, _, stderr) = run(tick_longbridge()
            .env("TICKLOOP_OAUTH_TOKEN_FILE", invalid)
            .args(["auth", "status"]));
        assert_eq!(code, 2, "{invalid}: {stderr}");
        assert!(stderr.contains("TICKLOOP_OAUTH_TOKEN_FILE"), "{stderr}");
    }
}

#[test]
fn quote_once_json_without_auth_emits_no_snapshot_and_exits_three() {
    let (code, stdout, stderr) =
        run(tick_longbridge().args(["quote", "TSLA", "700", "--once", "--format", "json"]));
    assert_eq!(code, 3, "{stderr}");
    assert!(stdout.is_empty(), "{stdout}");
    assert!(stderr.contains(AUTH_HINT), "{stderr}");
}

#[test]
fn authenticated_interactive_quote_without_tty_explains_once_mode() {
    let (code, stdout, stderr) = run(tick_longbridge()
        .env("HOME", authorized_home("authenticated-home"))
        .args(["quote", "TSLA"]));
    assert_eq!(code, 2, "{stderr}");
    assert!(stdout.is_empty());
    assert!(
        stderr.contains("TTY") && stderr.contains("--once"),
        "{stderr}"
    );
}

#[test]
fn a_tick_instance_refuses_the_one_shot_commands() {
    for arguments in [
        &["describe"][..],
        &["auth", "status"],
        &["probe-fundamentals", "TSLA.US"],
        &["quote", "TSLA", "--once"],
    ] {
        let (code, stdout, stderr) = run(tick_longbridge()
            .env("TICKLOOP_INSTANCE_ID", "hosted:one-shot")
            .args(arguments));
        assert_eq!(code, 2, "{arguments:?}: {stderr}");
        assert!(stdout.is_empty(), "{arguments:?}: {stdout}");
        assert!(stderr.contains("requires a live Tick"), "{stderr}");
    }
}

#[test]
fn a_tick_instance_without_authorization_exits_three_before_speaking_the_protocol() {
    let (code, stdout, stderr) = run(tick_longbridge()
        .env("TICKLOOP_INSTANCE_ID", "hosted:unauthorized")
        .args(["kline", "700.HK"]));
    assert_eq!(code, 3, "{stderr}");
    assert!(stdout.is_empty(), "{stdout}");
    assert!(stderr.contains(AUTH_HINT), "{stderr}");
}

#[test]
fn describe_lists_exactly_the_ticks_it_runs_with_their_minimum_panel_sizes() {
    let (code, stdout, stderr) = run(tick_longbridge().env("LANG", "C").arg("describe"));
    assert_eq!(code, 0, "{stderr}");
    let ticks: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let kinds: Vec<_> = ticks
        .as_array()
        .unwrap()
        .iter()
        .map(|tick| tick["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, TICKS);
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
