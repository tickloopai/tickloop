#![cfg(feature = "live-api")]

use crossterm::event::Event;
use futures_util::StreamExt;

/// Logs in to Longbridge with the login screens, saving where Tickloop keeps the login.
#[tokio::test]
#[ignore = "interactive device login; run explicitly in your own terminal"]
async fn device_login_against_longbridge() {
    let settings =
        tick_market::config::Settings::load(&tick_market::config::default_path().unwrap(), false)
            .unwrap();
    let credentials = tick_longbridge::auth::credentials().unwrap();
    let mut session = tick_market::terminal::TerminalSession::enter().unwrap();
    let input = tick_market::terminal::input_events()
        .unwrap()
        .map(|event: std::io::Result<Event>| event);
    let outcome = tick_longbridge::login::run(
        &settings,
        &credentials,
        &tick_longbridge::region::Issuers::default(),
        &mut session.terminal,
        input,
    )
    .await
    .unwrap();
    drop(session);
    println!("{outcome:?}");
}
