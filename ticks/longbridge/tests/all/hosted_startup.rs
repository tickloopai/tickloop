#![cfg(unix)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use tick_longbridge::region::GLOBAL_ISSUER;

/// Where a debug build of `tick-longbridge` finds a stand-in for Longbridge.
const TEST_ENDPOINT_ENV: &str = "TICKLOOP_LONGBRIDGE_TEST_ENDPOINT";

/// A home directory whose default config root holds `token` as the login.
fn home(name: &str, token: Option<&tick_packages::OAuthToken>) -> PathBuf {
    let home =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&home);
    fs::create_dir_all(&home).unwrap();
    if let Some(token) = token {
        crate::support::login::save(&home.join(".tickloop"), token);
    }
    home
}

/// `tick-longbridge` as a Tick Instance with nothing from the caller's environment: its login
/// is in `home`, and a debug build connects to `longbridge` for every Longbridge endpoint.
fn hosted_longbridge(home: &Path, longbridge: &str, arguments: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tick-longbridge"));
    command
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("LANG", "en_US.UTF-8")
        .env("HOME", home)
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("TICKLOOP_INSTANCE_ID", "hosted:startup")
        .env(TEST_ENDPOINT_ENV, longbridge)
        .current_dir(home)
        .args(["--language", "en"])
        .args(arguments);
    command
}

/// The only checks before the Tick speaks the protocol need no network: a login that is missing,
/// or has lapsed with nothing to renew it, still fails the launch with how to log in.
#[test]
fn a_hosted_tick_without_a_usable_login_exits_three_before_speaking_the_protocol() {
    let lapsed = crate::support::login::token(GLOBAL_ISSUER, "ap_fixture", None, 1);
    for (name, token) in [
        ("hosted-logged-out", None),
        ("hosted-lapsed", Some(&lapsed)),
    ] {
        let home = home(name, token);
        let output = hosted_longbridge(&home, "127.0.0.1:9", &["quote", "TSLA.US"])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(3), "{name}: {stderr}");
        assert!(output.stdout.is_empty(), "{name}");
        assert!(
            stderr.contains(tick_longbridge::AUTH_HINT),
            "{name}: {stderr}"
        );
    }
}

/// Only a debug build reads the test endpoint: a release build, which is what users install,
/// does not even contain its name.
#[test]
fn only_a_debug_build_reads_the_test_endpoint() {
    let binary = fs::read(env!("CARGO_BIN_EXE_tick-longbridge")).unwrap();
    let named = binary
        .windows(TEST_ENDPOINT_ENV.len())
        .any(|window| window == TEST_ENDPOINT_ENV.as_bytes());
    assert_eq!(named, cfg!(debug_assertions));
}

/// Tests against a local stand-in for Longbridge, which only a debug build can be pointed at.
#[cfg(debug_assertions)]
mod against_a_stand_in {
    use std::{
        fs,
        io::{BufRead, BufReader, Write},
        net::TcpListener,
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };

    use serde_json::{Value, json};
    use tick_packages::OAuthToken;
    use tickloop_agent_test_support::authorization_server::{
        AuthorizationServer, TOKEN_PATH, TokenReply,
    };

    use super::{GLOBAL_ISSUER, home, hosted_longbridge};
    use crate::support::{
        hosted::{self, Hosted},
        login,
    };

    /// How long the stand-in holds each request it does not answer before closing it.
    const UNANSWERED_FOR: Duration = Duration::from_millis(1500);

    fn authorized() -> Option<&'static OAuthToken> {
        static AUTHORIZED: std::sync::LazyLock<OAuthToken> = std::sync::LazyLock::new(|| {
            login::token(GLOBAL_ISSUER, "ap_fixture", None, 4102444800)
        });
        Some(&AUTHORIZED)
    }

    /// What a stand-in for Longbridge answers.
    #[derive(Clone, Copy)]
    enum Answers {
        Nothing,
        /// A socket token to each request for one.
        SocketTokens,
        /// An OpenAPI error to every request, as Longbridge turns down an invalid request.
        Rejections,
    }

    const REJECTION: &str = r#"{"code":301600,"message":"invalid request","data":null}"#;

    /// A stand-in for Longbridge: its `host:port` and the first line of each request it received.
    /// It holds every request it does not answer for [`UNANSWERED_FOR`] before closing it.
    fn stand_in(answers: Answers) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let received = Arc::clone(&requests);
        std::thread::spawn(move || {
            for connection in listener.incoming() {
                let Ok(mut connection) = connection else {
                    return;
                };
                let received = Arc::clone(&received);
                std::thread::spawn(move || {
                    let mut request = String::new();
                    let _ = BufReader::new(&connection).read_line(&mut request);
                    let request = request.trim_end().to_owned();
                    received.lock().unwrap().push(request.clone());
                    let body = match answers {
                        Answers::SocketTokens if request.contains("/v1/socket/token") => {
                            r#"{"code":0,"message":"","data":{"otp":"stand-in","limit":10,"online":0}}"#
                        }
                        Answers::Rejections => REJECTION,
                        _ => {
                            std::thread::sleep(UNANSWERED_FOR);
                            return;
                        }
                    };
                    let _ = write!(
                        connection,
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                });
            }
        });
        (address, requests)
    }

    fn wait_for_exit(tick: &mut Hosted) -> std::process::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Some(status) = tick.child.try_wait().unwrap() {
                return status;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        tick.child.kill().unwrap();
        panic!("tick-longbridge did not exit: {}", tick.screen());
    }

    /// A hosted quote Tick that cannot reach Longbridge is ready for its host at once, describing
    /// itself and reading as no data; the Agent then learns once why it keeps trying, and the
    /// view says which attempt is under way and why the last one failed.
    #[test]
    fn a_hosted_tick_that_cannot_reach_longbridge_answers_at_once_and_reports_why_it_retries() {
        let home = home("hosted-unreachable", authorized());
        let (longbridge, _) = stand_in(Answers::Nothing);
        let mut tick = hosted::launch(hosted_longbridge(&home, &longbridge, &["quote", "TSLA.US"]));

        tick.request(1, "tick/describe", json!({}));
        tick.request(2, "tick/read", json!({}));
        let described = tick.frame_where(Duration::from_secs(1), |frame| frame["id"] == 1);
        assert_eq!(described["result"]["kind"], "quote");
        let read = tick.frame_where(Duration::from_secs(1), |frame| frame["id"] == 2);
        assert_eq!(read["result"], json!({"seq": 0, "data": null}));

        let retrying = tick.frame_where(Duration::from_secs(15), |frame| {
            frame["method"] == "tick/update"
        });
        let connection = &retrying["params"]["data"]["connection"];
        assert_eq!(connection["state"], "retrying", "{retrying}");
        let error = connection["error"].as_str().unwrap_or_default().to_owned();
        assert!(!error.is_empty(), "{retrying}");

        tick.request(3, "tick/read", json!({}));
        let read = tick.frame_where(Duration::from_secs(1), |frame| frame["id"] == 3);
        assert_eq!(read["result"]["data"], retrying["params"]["data"]);

        // The next attempt fails the same way and publishes nothing new.
        let started = Instant::now();
        let mut later: Vec<Value> = Vec::new();
        while started.elapsed() < UNANSWERED_FOR * 3 {
            if let Some(frame) = tick.next_frame(Duration::from_millis(200)) {
                later.push(frame);
            }
        }
        assert!(later.is_empty(), "{later:?}");
        let screen = tick.screen();
        assert!(
            screen.contains("Connecting to Longbridge (attempt 2)")
                || screen.contains("Connecting to Longbridge (attempt 3)"),
            "{screen}"
        );
        assert!(screen.contains("Last error:"), "{screen}");

        tick.tty.write_all(b"q").unwrap();
        assert!(wait_for_exit(&mut tick).success(), "{}", tick.screen());
    }

    /// Two Ticks starting at once with a due token renew it once: the one holding the token
    /// file's lock asks the token endpoint and saves the rotated token, and the other takes that
    /// token, as a rotating server would refuse the old refresh token. Both then reach
    /// Longbridge with the renewed login, found in the file Tickloop names when it starts them.
    #[test]
    fn ticks_starting_together_with_a_due_token_renew_it_once() {
        let server = AuthorizationServer::start();
        server.answer_polls([TokenReply::Approve]);
        let config_root = home("renewing-together", None).join("config");
        login::save(
            &config_root,
            &login::token(server.issuer(), "ap_lapsed", Some("ap_refresh"), 1),
        );
        let token_file = login::credentials(&config_root).oauth_token_path();
        let (longbridge, requests) = stand_in(Answers::SocketTokens);
        let elsewhere = home("renewing-elsewhere", None);
        let mut ticks: Vec<Hosted> = ["TSLA.US", "700.HK"]
            .into_iter()
            .map(|symbol| {
                let mut command = hosted_longbridge(&elsewhere, &longbridge, &["quote", symbol]);
                command.env("TICKLOOP_OAUTH_TOKEN_FILE", &token_file);
                hosted::launch(command)
            })
            .collect();

        // Each gets past the login to Longbridge, whose quote stream never answers.
        for tick in &ticks {
            let update = tick.frame_where(Duration::from_secs(15), |frame| {
                frame["method"] == "tick/update"
            });
            assert_eq!(
                update["params"]["data"]["connection"]["state"], "retrying",
                "{update}"
            );
        }

        assert_eq!(server.requests_to(TOKEN_PATH).len(), 1);
        let renewed = login::saved(&config_root).unwrap();
        assert_eq!(renewed.access_token, "access-1");
        assert_eq!(renewed.refresh_token.as_deref(), Some("refresh-1"));
        assert!(
            requests
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request.starts_with("GET /v1/socket/token"))
                .count()
                >= 2,
            "both reached Longbridge"
        );
        assert!(
            !elsewhere.join(".tickloop/credentials").exists(),
            "the default config root is not used"
        );
        for tick in &mut ticks {
            tick.tty.write_all(b"q").unwrap();
            assert!(wait_for_exit(tick).success(), "{}", tick.screen());
        }
    }

    /// The SDK reads its own LONGBRIDGE_* endpoint settings from the environment and from a
    /// `.env` file in the working directory, which is the Agent's; the Tick sets its endpoints
    /// itself, so neither can send the access token anywhere else.
    #[test]
    fn longbridge_endpoint_settings_in_the_environment_or_a_dotenv_file_are_ignored() {
        let (longbridge, requests) = stand_in(Answers::SocketTokens);
        let (environment, from_environment) = stand_in(Answers::Nothing);
        let (dotenv, from_dotenv) = stand_in(Answers::Nothing);
        let settings = |address: &str| {
            [
                ("LONGBRIDGE_HTTP_URL", format!("http://{address}")),
                ("LONGBRIDGE_QUOTE_WS_URL", format!("ws://{address}/v2")),
                ("LONGBRIDGE_TRADE_WS_URL", format!("ws://{address}/v2")),
            ]
        };

        let in_environment = home("endpoints-in-environment", authorized());
        let mut command = hosted_longbridge(&in_environment, &longbridge, &["quote", "TSLA.US"]);
        command.envs(settings(&environment));
        let in_dotenv = home("endpoints-in-dotenv", authorized());
        let dotenv_file: String = settings(&dotenv)
            .iter()
            .map(|(name, value)| format!("{name}={value}\n"))
            .collect();
        fs::write(in_dotenv.join(".env"), dotenv_file).unwrap();
        for command in [
            command,
            hosted_longbridge(&in_dotenv, &longbridge, &["quote", "TSLA.US"]),
        ] {
            let mut tick = hosted::launch(command);
            let retrying = tick.frame_where(Duration::from_secs(15), |frame| {
                frame["method"] == "tick/update"
            });
            assert_eq!(
                retrying["params"]["data"]["connection"]["state"], "retrying",
                "{retrying}"
            );
            tick.tty.write_all(b"q").unwrap();
            assert!(wait_for_exit(&mut tick).success(), "{}", tick.screen());
        }

        let requests = requests.lock().unwrap();
        let tokens = requests
            .iter()
            .filter(|request| request.starts_with("GET /v1/socket/token"))
            .count();
        let quote_connections = requests
            .iter()
            .filter(|request| request.starts_with("GET /v2"))
            .count();
        assert_eq!((tokens, quote_connections), (2, 2), "{requests:?}");
        assert!(from_environment.lock().unwrap().is_empty());
        assert!(from_dotenv.lock().unwrap().is_empty());
    }

    /// The SDK loads its settings from the environment, where a `.env` file in the working
    /// directory, the Agent's, would have put them: Tickloop builds it without that, so such a
    /// file cannot make it write its logs, which hold whole API responses, anywhere. The same
    /// setting in the Tick's own environment still applies.
    #[test]
    fn a_dotenv_file_in_the_working_directory_sets_nothing_for_the_sdk() {
        let (longbridge, requests) = stand_in(Answers::SocketTokens);
        let in_environment = home("logs-in-environment", authorized());
        let in_dotenv = home("logs-in-dotenv", authorized());
        let environment_logs = in_environment.join("sdk-logs");
        let dotenv_logs = in_dotenv.join("sdk-logs");
        fs::write(
            in_dotenv.join(".env"),
            format!("LONGBRIDGE_LOG_PATH={}\n", dotenv_logs.display()),
        )
        .unwrap();
        let mut command = hosted_longbridge(&in_environment, &longbridge, &["quote", "TSLA.US"]);
        command.env("LONGBRIDGE_LOG_PATH", &environment_logs);
        let mut published = Vec::new();
        for command in [
            command,
            hosted_longbridge(&in_dotenv, &longbridge, &["quote", "TSLA.US"]),
        ] {
            let mut tick = hosted::launch(command);
            let retrying = tick.frame_where(Duration::from_secs(15), |frame| {
                frame["method"] == "tick/update"
            });
            assert_eq!(
                retrying["params"]["data"]["connection"]["state"], "retrying",
                "{retrying}"
            );
            published.push(retrying["params"]["data"].clone());
            tick.tty.write_all(b"q").unwrap();
            assert!(wait_for_exit(&mut tick).success(), "{}", tick.screen());
        }
        assert_eq!(
            published[0], published[1],
            "the .env file changes nothing the Tick does"
        );
        let requests = requests.lock().unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.starts_with("GET /v1/socket/token"))
                .count(),
            2,
            "both runs reached the SDK's HTTP client: {requests:?}"
        );
        assert!(
            environment_logs.exists(),
            "the Tick's own environment still sets the SDK's log path"
        );
        assert!(
            !dotenv_logs.exists(),
            "a .env file in the working directory set the SDK's log path"
        );
    }

    /// A request Longbridge turns down, here every request, is not asked again: the Tick stays
    /// open, the Agent learns the reason once and a Tick Tool call fails with it.
    #[test]
    fn a_hosted_tick_whose_request_longbridge_turns_down_shows_why_without_asking_again() {
        let home = home("hosted-rejected", authorized());
        let (longbridge, requests) = stand_in(Answers::Rejections);
        let mut tick = hosted::launch(hosted_longbridge(&home, &longbridge, &["watchlist"]));

        let failed = tick.frame_where(Duration::from_secs(15), |frame| {
            frame["method"] == "tick/update"
        });
        let connection = &failed["params"]["data"]["connection"];
        assert_eq!(connection["state"], "failed", "{failed}");
        let error = connection["error"].as_str().unwrap_or_default();
        assert!(error.contains("301600"), "{failed}");
        let asked = requests.lock().unwrap().len();

        tick.request(1, "tick/describe", json!({}));
        let described = tick.frame_where(Duration::from_secs(1), |frame| frame["id"] == 1);
        let tool = described["result"]["tools"][0]["name"].clone();
        let revision = described["result"]["revision"].clone();
        tick.request(
            2,
            "tick/call",
            json!({"tool": tool, "args": {}, "revision": revision}),
        );
        let called = tick.frame_where(Duration::from_secs(1), |frame| frame["id"] == 2);
        let message = called["error"]["message"].as_str().unwrap_or_default();
        assert!(message.contains("301600"), "{called}");

        // Longer than the first retry would wait: nothing is asked or published again.
        let started = Instant::now();
        let mut later: Vec<Value> = Vec::new();
        while started.elapsed() < Duration::from_secs(3) {
            if let Some(frame) = tick.next_frame(Duration::from_millis(200)) {
                later.push(frame);
            }
        }
        assert!(later.is_empty(), "{later:?}");
        assert_eq!(requests.lock().unwrap().len(), asked);
        assert!(
            tick.child.try_wait().unwrap().is_none(),
            "the Tick stays open"
        );
        let screen = tick.screen();
        assert!(
            screen.contains("Longbridge turned this Tick's request down"),
            "{screen}"
        );

        tick.tty.write_all(b"q").unwrap();
        assert!(wait_for_exit(&mut tick).success(), "{}", tick.screen());
    }
}
