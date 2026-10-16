use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use futures_util::StreamExt;
use ratatui::{Terminal, backend::TestBackend, buffer::CellDiffOption};
use serde_json::json;
use tick_longbridge::{
    login::{self, LoginOutcome},
    region::{GLOBAL_ISSUER, Issuers},
};
use tick_market::config::Settings;
use tickloop_agent_test_support::authorization_server::{AuthorizationServer, TokenReply};

use crate::support::login::{credentials, now, save, saved, token};

/// Bounds a login whose failure would otherwise wait for its device code to expire.
const HANG_GUARD: std::time::Duration = std::time::Duration::from_secs(30);

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

/// Servers no test reaches: a login there fails at once.
fn unreachable_issuers() -> Issuers {
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let issuer = format!("http://{}", closed.local_addr().unwrap());
    Issuers {
        china_mainland: issuer.clone(),
        global: issuer,
    }
}

#[tokio::test]
async fn a_valid_login_is_kept_with_enter_and_never_reaches_the_network() {
    let root = tempfile::tempdir().unwrap();
    save(
        root.path(),
        &token(GLOBAL_ISSUER, "ap_token", None, 4102445800),
    );
    let input = futures_util::stream::iter([key(KeyCode::Enter)]);
    let mut terminal = Terminal::new(TestBackend::new(72, 16)).unwrap();

    let outcome = login::run(
        &Settings::default(),
        &credentials(root.path()),
        &unreachable_issuers(),
        &mut terminal,
        input,
    )
    .await
    .unwrap();

    assert_eq!(outcome, LoginOutcome::Authorized);
    let screen = screen(&terminal);
    assert!(screen.contains("Longbridge"), "{screen}");
    assert!(screen.contains("global"), "{screen}");
    assert!(
        !screen.contains("Global"),
        "the region list is not shown: {screen}"
    );
}

#[tokio::test]
async fn r_on_the_signed_in_screen_starts_a_fresh_login() {
    let root = tempfile::tempdir().unwrap();
    save(
        root.path(),
        &token(GLOBAL_ISSUER, "ap_token", None, 4102445800),
    );
    let input = futures_util::stream::iter([key(KeyCode::Char('r'))]);
    let mut terminal = Terminal::new(TestBackend::new(64, 16)).unwrap();

    let outcome = login::run(
        &Settings::default(),
        &credentials(root.path()),
        &unreachable_issuers(),
        &mut terminal,
        input,
    )
    .await
    .unwrap();

    assert_eq!(outcome, LoginOutcome::Cancelled);
    let screen = screen(&terminal);
    assert!(
        screen.contains("> Global"),
        "the region list starts at the saved login's: {screen}"
    );
}

#[tokio::test]
async fn arrow_keys_select_the_region_without_writing_escape_sequences() {
    let root = tempfile::tempdir().unwrap();
    let input = futures_util::stream::iter([key(KeyCode::Down)]);
    let mut terminal = Terminal::new(TestBackend::new(64, 16)).unwrap();

    let outcome = login::run(
        &Settings::default(),
        &credentials(root.path()),
        &unreachable_issuers(),
        &mut terminal,
        input,
    )
    .await
    .unwrap();

    assert_eq!(outcome, LoginOutcome::Cancelled);
    let screen = screen(&terminal);
    assert!(screen.contains("> Global"), "{screen}");
    assert!(!screen.contains("^[["), "{screen}");
    assert!(
        !credentials(root.path()).directory().exists(),
        "choosing writes nothing"
    );
}

/// The region chosen picks the server the login is made with; the approved login is saved
/// where Tickloop keeps it, and the screen shows no secret.
#[tokio::test]
async fn the_chosen_region_logs_in_with_its_server_and_saves_the_login() {
    let root = tempfile::tempdir().unwrap();
    let server = AuthorizationServer::start();
    server.answer_polls([TokenReply::Approve]);
    let issuers = Issuers {
        global: server.issuer().to_owned(),
        ..unreachable_issuers()
    };
    let input = futures_util::stream::iter([key(KeyCode::Down), key(KeyCode::Enter)])
        .chain(futures_util::stream::pending());
    let mut terminal = Terminal::new(TestBackend::new(76, 18)).unwrap();
    let opened_pages = std::sync::Mutex::new(Vec::new());

    let outcome = tokio::time::timeout(
        HANG_GUARD,
        login::run_with_page_access(
            &Settings::default(),
            &credentials(root.path()),
            &issuers,
            &mut terminal,
            input,
            login::PageAccess {
                // A browser that does not open leaves the login going.
                open: |page: &str| {
                    opened_pages.lock().unwrap().push(page.to_owned());
                    false
                },
                links: false,
            },
        ),
    )
    .await
    .expect("the approved login leaves the screens")
    .unwrap();

    assert_eq!(outcome, LoginOutcome::Authorized);
    assert_eq!(
        *opened_pages.lock().unwrap(),
        [format!("{}/verify?user_code=WDJB-MJHT", server.issuer())],
        "the verification page opens in the browser once"
    );
    let saved = saved(root.path()).unwrap();
    assert_eq!(saved.access_token, "access-1");
    assert_eq!(saved.issuer, server.issuer());
    assert!(saved.expires_at > now());
    let screen = screen(&terminal);
    let compact_screen: String = screen.split_whitespace().collect();
    assert!(compact_screen.contains("授权成功"), "{screen}");
    for secret in ["WDJB", "device-secret", "access-1", "refresh-1"] {
        assert!(!screen.contains(secret), "{screen}");
    }
}

const PLAIN_PAGE: &str = "https://example.com/device";
const PAGE_WITH_CODE: &str =
    "https://example.com/oauth2/device/verify?user_code=ABCD-EFGH&client_id=tickloop-device";

/// The waiting screen of an English login on a `columns`-wide terminal, whose browser opens a
/// page when `opens` and which shows OSC 8 links when `links`, and the pages it was asked to
/// open. The login is cancelled once the page was asked for.
async fn waiting_screen(columns: u16, opens: bool, links: bool) -> (String, Vec<String>) {
    use std::sync::{Arc, Mutex};
    let server = AuthorizationServer::start();
    server.edit_device_authorization(|device| {
        device["user_code"] = json!("ABCD-EFGH");
        device["verification_uri"] = json!(PLAIN_PAGE);
        device["verification_uri_complete"] = json!(PAGE_WITH_CODE);
    });
    let root = tempfile::tempdir().unwrap();
    let issuers = Issuers {
        china_mainland: server.issuer().to_owned(),
        ..unreachable_issuers()
    };
    let page_asked = Arc::new(tokio::sync::Notify::new());
    let input = Box::pin(futures_util::stream::iter([key(KeyCode::Enter)]).chain(
        futures_util::stream::once({
            let page_asked = Arc::clone(&page_asked);
            async move {
                page_asked.notified().await;
                key(KeyCode::Char('q'))
            }
        }),
    ));
    let opened_pages = Mutex::new(Vec::new());
    let mut terminal = Terminal::new(TestBackend::new(columns, 18)).unwrap();
    let settings = Settings {
        language: tick_provider::preferences::Language::En,
        ..Settings::default()
    };

    let outcome = login::run_with_page_access(
        &settings,
        &credentials(root.path()),
        &issuers,
        &mut terminal,
        input,
        login::PageAccess {
            open: |page: &str| {
                opened_pages.lock().unwrap().push(page.to_owned());
                page_asked.notify_one();
                opens
            },
            links,
        },
    )
    .await
    .unwrap();

    assert_eq!(outcome, LoginOutcome::Cancelled);
    (screen(&terminal), opened_pages.into_inner().unwrap())
}

#[tokio::test]
async fn the_verification_page_opens_and_its_link_stays_on_one_line() {
    let (screen, opened) = waiting_screen(120, true, false).await;
    assert_eq!(opened, [PAGE_WITH_CODE]);
    assert!(
        screen.lines().any(|line| line.contains(PAGE_WITH_CODE)),
        "the card widens for the whole link:\n{screen}"
    );
    assert!(
        screen.contains("1  Opened in your browser; if it did not open, visit"),
        "{screen}"
    );
    assert!(
        screen.contains("2  Confirm and approve this code"),
        "{screen}"
    );
    assert!(screen.contains("ABCD-EFGH"), "{screen}");
}

#[tokio::test]
async fn without_links_a_page_too_long_for_the_terminal_gives_way_to_the_plain_page() {
    let (screen, opened) = waiting_screen(70, false, false).await;
    assert_eq!(
        opened,
        [PAGE_WITH_CODE],
        "the browser still gets the page with the code"
    );
    assert!(
        !screen.contains("user_code"),
        "no link is broken across lines:\n{screen}"
    );
    assert!(
        screen.lines().any(|line| line.contains(PLAIN_PAGE)),
        "{screen}"
    );
    assert!(screen.contains("1  Open in your browser"), "{screen}");
    assert!(
        screen.contains("2  Enter this code and approve"),
        "{screen}"
    );
    assert!(screen.contains("ABCD-EFGH"), "{screen}");
}

#[tokio::test]
async fn every_row_of_a_wrapped_page_links_to_the_whole_page() {
    let (screen, opened) = waiting_screen(70, true, true).await;
    assert_eq!(opened, [PAGE_WITH_CODE]);
    // The card fills the 70 columns; its borders leave 68 for the link.
    let rows = tickloop_ui::hyperlink::split_rows(PAGE_WITH_CODE, 68);
    assert!(rows.len() > 1, "the page wraps at 70 columns");
    for row in &rows {
        let linked = tickloop_ui::hyperlink::linked(PAGE_WITH_CODE, row);
        assert_eq!(
            screen.matches(&linked).count(),
            1,
            "{row:?} links to the whole page:\n{screen:?}"
        );
    }
    assert!(
        screen.contains("2  Confirm and approve this code"),
        "{screen}"
    );
}

#[test]
fn only_https_pages_open_in_the_browser() {
    for page in [
        "http://example.com/device",
        "file:///etc/passwd",
        "javascript:alert(1)",
        "example.com/device",
    ] {
        assert!(!login::open_in_browser(page), "{page}");
    }
}
