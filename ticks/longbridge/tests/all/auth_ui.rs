use crate::support::http;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use futures_util::StreamExt;
use longbridge::oauth::{StoredToken, TokenStorage};
use ratatui::{Terminal, backend::TestBackend, buffer::CellDiffOption};
use serde_json::json;
use tick_longbridge::auth::{AuthEndpoints, storage::AuthStorage};
use tick_longbridge::login::{self, LoginOutcome};
use tick_market::config::Settings;
use tick_provider::preferences::Region;

fn key(code: KeyCode) -> std::io::Result<Event> {
    Ok(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
}

fn screen(terminal: &Terminal<TestBackend>) -> String {
    let width = terminal.backend().buffer().area.width as usize;
    terminal
        .backend()
        .buffer()
        .content
        .chunks(width)
        .map(|row| {
            row.iter()
                .filter(|cell| cell.diff_option != CellDiffOption::Skip)
                .map(|cell| cell.symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn a_valid_login_is_kept_with_enter_and_never_reaches_the_network() {
    let home = tempfile::tempdir().unwrap();
    let config_path = home.path().join(".tickloop/config.toml");
    let storage = AuthStorage::in_home(home.path());
    storage
        .save(&StoredToken {
            client_id: "client".into(),
            access_token: "ap_token".into(),
            refresh_token: None,
            expires_at: 4102445800,
        })
        .unwrap();
    let settings = Settings {
        region: Some(Region::Global),
        ..Settings::default()
    };
    let input = futures_util::stream::iter([key(KeyCode::Enter)]);
    let mut terminal = Terminal::new(TestBackend::new(72, 16)).unwrap();

    let outcome = login::run(
        &settings,
        &config_path,
        &storage,
        None,
        &mut terminal,
        input,
    )
    .await
    .unwrap();

    assert_eq!(outcome, LoginOutcome::Authorized(Region::Global));
    let screen = screen(&terminal);
    assert!(screen.contains("Longbridge"), "{screen}");
    assert!(screen.contains("global"), "{screen}");
    assert!(
        !screen.contains("Global"),
        "the region list is not shown: {screen}"
    );
    assert!(!config_path.exists(), "keeping a login writes nothing");
}

#[tokio::test]
async fn r_on_the_signed_in_screen_starts_a_fresh_login() {
    let home = tempfile::tempdir().unwrap();
    let config_path = home.path().join(".tickloop/config.toml");
    let storage = AuthStorage::in_home(home.path());
    storage
        .save(&StoredToken {
            client_id: "client".into(),
            access_token: "ap_token".into(),
            refresh_token: None,
            expires_at: 4102445800,
        })
        .unwrap();
    let input = futures_util::stream::iter([key(KeyCode::Char('r'))]);
    let mut terminal = Terminal::new(TestBackend::new(64, 16)).unwrap();

    let outcome = login::run(
        &Settings::default(),
        &config_path,
        &storage,
        None,
        &mut terminal,
        input,
    )
    .await
    .unwrap();

    assert_eq!(outcome, LoginOutcome::Cancelled);
    let screen = screen(&terminal);
    assert!(
        screen.contains("Global"),
        "the region list is shown: {screen}"
    );
    assert!(screen.contains("> "), "{screen}");
}

#[tokio::test]
async fn arrow_keys_select_the_region_without_writing_escape_sequences() {
    let home = tempfile::tempdir().unwrap();
    let config_path = home.path().join(".tickloop/config.toml");
    let storage = AuthStorage::in_home(home.path());
    let input = futures_util::stream::iter([key(KeyCode::Down)]);
    let mut terminal = Terminal::new(TestBackend::new(64, 16)).unwrap();

    let outcome = login::run(
        &Settings::default(),
        &config_path,
        &storage,
        None,
        &mut terminal,
        input,
    )
    .await
    .unwrap();

    assert_eq!(outcome, LoginOutcome::Cancelled);
    let screen = screen(&terminal);
    assert!(screen.contains("> Global"), "{screen}");
    assert!(!screen.contains("^[["), "{screen}");
    assert!(!config_path.exists());
}

#[tokio::test]
async fn selected_region_authorizes_and_saves_through_the_shared_tui() {
    use std::{
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };
    const SETUP_TIMEOUT: Duration = Duration::from_secs(10);
    const COMPLETION_TIMEOUT: Duration = Duration::from_secs(4);
    let (device_response_sender, mut device_response_receiver) =
        tokio::sync::oneshot::channel::<Instant>();
    let device_response_sender = Mutex::new(Some(device_response_sender));
    let observations = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&observations);
    let stub = http::Stub::start_with_observer(
        |request| {
            let response = if request.contains("/register ") {
                (201, json!({"client_id":"client"}))
            } else if request.contains("/device/authorize ") {
                (
                    200,
                    json!({
                        "device_code":"device-secret",
                        "user_code":"ABCD-EFGH",
                        "verification_uri":"https://example.com/device",
                        "expires_in":30,
                        "interval":1
                    }),
                )
            } else if request.contains("x-dc-region: ap") {
                (200, json!({"access_token":"ap_access","expires_in":3600}))
            } else {
                (400, json!({"error":"authorization_pending"}))
            };
            Some(response)
        },
        move |observation| {
            if observation.phase == "response-write-ended"
                && observation.endpoint == "device-authorize"
                && observation.status == Some(200)
                && observation.write_ok == Some(true)
                && let Some(sender) = device_response_sender.lock().unwrap().take()
            {
                let _ = sender.send(observation.at);
            }
            observed.lock().unwrap().push(observation);
        },
    )
    .await;
    let home = tempfile::tempdir().unwrap();
    let config_path = home.path().join(".tickloop/config.toml");
    let storage = AuthStorage::in_home(home.path());
    let endpoints = AuthEndpoints {
        ap: format!("{}/oauth2", stub.url),
        us: format!("{}/oauth2", stub.url),
    };
    let input = futures_util::stream::iter([key(KeyCode::Down), key(KeyCode::Enter)])
        .chain(futures_util::stream::pending());
    let mut terminal = Terminal::new(TestBackend::new(76, 18)).unwrap();

    let started = Instant::now();
    let mut device_response_at = None;
    let outcome = {
        let settings = Settings::default();
        let login = login::run(
            &settings,
            &config_path,
            &storage,
            Some(endpoints),
            &mut terminal,
            input,
        );
        tokio::pin!(login);
        let setup_deadline = started + SETUP_TIMEOUT;
        let setup = tokio::select! {
            biased;
            response = &mut device_response_receiver => {
                response.map(|at| (at, None)).map_err(|_| "missing-device-response-boundary")
            }
            completed = &mut login => {
                device_response_receiver.try_recv()
                    .map(|at| (at, Some(completed)))
                    .map_err(|_| "missing-device-response-boundary")
            }
            _ = tokio::time::sleep_until(setup_deadline.into()) => Err("setup-timeout"),
        };
        match setup {
            Err(failure) => Err(failure),
            Ok((response_at, completed)) => {
                device_response_at = Some(response_at);
                let received_at = Instant::now();
                let completion_deadline = response_at + COMPLETION_TIMEOUT;
                if response_at < started || response_at > setup_deadline {
                    Err("device-response-outside-setup-deadline")
                } else if received_at > setup_deadline {
                    Err("setup-deadline-exceeded")
                } else if received_at > completion_deadline {
                    Err("completion-deadline-already-elapsed")
                } else {
                    let completed = if let Some(completed) = completed {
                        Ok(completed)
                    } else {
                        tokio::time::timeout_at(completion_deadline.into(), &mut login)
                            .await
                            .map_err(|_| "completion-timeout")
                    };
                    if Instant::now() > completion_deadline {
                        Err("completion-deadline-exceeded")
                    } else {
                        completed
                    }
                }
            }
        }
    };
    if let Err(failure) = &outcome {
        let rendered = screen(&terminal);
        let compact_screen: String = rendered.split_whitespace().collect();
        let screen_phase = if compact_screen.contains("授权成功") {
            "authorized"
        } else if compact_screen.contains("申请设备码") {
            "requesting-device-code"
        } else if compact_screen.contains("核对设备码并批准") {
            "waiting-for-approval"
        } else if compact_screen.contains("选择接入区域") {
            "selecting-region"
        } else {
            "other"
        };
        let token_saved = storage.token().map(|token| token.is_some()).ok();
        eprintln!(
            "auth-ui deadline failure: phase={failure} elapsed_ms={} setup_limit_ms={} completion_limit_ms={} device_response_ms={:?} token_saved={token_saved:?} screen_phase={screen_phase}",
            started.elapsed().as_millis(),
            SETUP_TIMEOUT.as_millis(),
            COMPLETION_TIMEOUT.as_millis(),
            device_response_at.map(|at| at.saturating_duration_since(started).as_millis()),
        );
        let mut observations = observations.lock().unwrap();
        observations.sort_by_key(|observation| observation.at);
        for observation in observations.iter() {
            eprintln!(
                "auth-ui phase: elapsed_ms={} connection={} phase={} endpoint={} center={} status={:?} write_ok={:?}",
                observation
                    .at
                    .saturating_duration_since(started)
                    .as_millis(),
                observation.connection,
                observation.phase,
                observation.endpoint,
                observation.center,
                observation.status,
                observation.write_ok,
            );
        }
    }
    let outcome = outcome
        .expect("approved device code must leave the shared authorization TUI")
        .unwrap();

    assert_eq!(outcome, LoginOutcome::Authorized(Region::Global));
    assert!(
        std::fs::read_to_string(config_path)
            .unwrap()
            .contains("region = \"global\"")
    );
    assert_eq!(storage.token().unwrap().unwrap().access_token, "ap_access");
    assert!(!stub.requests.lock().unwrap().is_empty());
    let screen = screen(&terminal);
    let compact_screen: String = screen.split_whitespace().collect();
    assert!(compact_screen.contains("授权成功"), "{screen}");
    assert!(!screen.contains("example.com"), "{screen}");
    assert!(!screen.contains("ABCD"), "{screen}");
    assert!(!screen.contains("device-secret"), "{screen}");
    assert!(!screen.contains("ap_access"), "{screen}");
}
