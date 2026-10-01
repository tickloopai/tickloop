#[path = "support/http.rs"]
mod http;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use futures_util::StreamExt;
use longbridge::oauth::{StoredToken, TokenStorage};
use ratatui::{Terminal, backend::TestBackend, buffer::CellDiffOption};
use serde_json::json;
use tickloop_ticks::{
    auth::{AuthEndpoints, LoginOutcome, run_tui, storage::AuthStorage},
    config::{Region, Settings},
};

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

    let outcome = run_tui(
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

    let outcome = run_tui(
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

    let outcome = run_tui(
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
    let stub = http::Stub::start(|request| {
        if request.contains("/register ") {
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
        }
    })
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

    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(4),
        run_tui(
            &Settings::default(),
            &config_path,
            &storage,
            Some(endpoints),
            &mut terminal,
            input,
        ),
    )
    .await
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
