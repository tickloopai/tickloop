use std::time::Duration;

use reqwest::StatusCode;
use tickloop_harness_connector::{LoginError, OAuthSettings};
use tokio_util::sync::CancellationToken;

use crate::support::{Fixture, http_client};

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_a_login_keeps_the_previous_credential_and_frees_the_port() {
    let fixture = Fixture::new();
    fixture.log_in().await;
    let before = fixture.credentials.current();
    let cancellation = CancellationToken::new();
    let challenge = fixture.challenge().await;
    let login = fixture
        .connector
        .start_login(Some(&challenge), &cancellation)
        .await
        .unwrap();
    let redirect = fixture.approve(&login).await;
    let port = reqwest::Url::parse(login.redirect_uri())
        .unwrap()
        .port()
        .unwrap();
    cancellation.cancel();

    assert_eq!(
        login.finish(&cancellation).await,
        Err(LoginError::Cancelled)
    );
    assert!(http_client().get(&redirect).send().await.is_err());
    assert_eq!(fixture.credentials.current(), before);
    std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
}

#[tokio::test]
async fn a_login_without_an_answer_times_out_after_five_minutes() {
    let fixture = Fixture::new();
    let login = fixture.start_login().await;
    tokio::time::pause();
    let started = tokio::time::Instant::now();

    assert_eq!(
        login.finish(&fixture.cancellation).await,
        Err(LoginError::TimedOut)
    );
    assert!(started.elapsed() >= Duration::from_secs(4 * 60 + 59));
    assert_eq!(fixture.credentials.current(), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_denied_authorization_saves_nothing() {
    let fixture = Fixture::new();
    fixture.server.deny_authorizations();
    let login = fixture.start_login().await;
    let redirect = fixture.approve(&login).await;

    let browser = http_client().get(&redirect).send().await.unwrap();
    assert_eq!(browser.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        login.finish(&fixture.cancellation).await,
        Err(LoginError::Denied)
    );
    assert_eq!(fixture.credentials.current(), None);
}

#[tokio::test]
async fn an_occupied_callback_port_is_reported_by_number() {
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = occupied.local_addr().unwrap().port();
    let fixture = Fixture::with_settings(OAuthSettings {
        callback_port: Some(port),
        ..OAuthSettings::default()
    });
    let challenge = fixture.challenge().await;

    let error = fixture
        .connector
        .start_login(Some(&challenge), &fixture.cancellation)
        .await
        .unwrap_err();
    assert_eq!(error, LoginError::CallbackPortUnavailable(port));
    assert!(error.to_string().contains(&port.to_string()));
}
