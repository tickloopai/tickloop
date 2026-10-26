use reqwest::StatusCode;
use serde_json::json;
use tick_oauth_test_server::{REGISTRATION_PATH, TOKEN_PATH};

use tickloop_harness_connector::CallbackError;

use crate::support::{Fixture, http_client, query_field};

#[tokio::test(flavor = "multi_thread")]
async fn login_through_the_loopback_callback_registers_tickloop_and_saves_a_usable_token() {
    let fixture = Fixture::new();
    let login = fixture.start_login().await;

    let authorization_url = login.authorization_url().to_owned();
    assert_eq!(
        query_field(&authorization_url, "code_challenge_method").as_deref(),
        Some("S256")
    );
    let redirect_uri = query_field(&authorization_url, "redirect_uri").unwrap();
    let port = reqwest::Url::parse(&redirect_uri).unwrap().port().unwrap();
    assert_eq!(redirect_uri, format!("http://localhost:{port}/callback"));
    let registration = &fixture.server.requests_to(REGISTRATION_PATH)[0];
    let registration = registration.json.as_ref().unwrap();
    assert_eq!(registration["client_name"], json!("Tickloop"));
    assert_eq!(registration["redirect_uris"], json!([redirect_uri]));
    assert_eq!(registration["token_endpoint_auth_method"], json!("none"));

    let redirect = fixture.approve(&login).await;
    let browser = http_client().get(&redirect).send().await.unwrap();
    assert_eq!(browser.status(), StatusCode::OK);
    login.finish(&fixture.cancellation).await.unwrap();

    let exchange = &fixture.server.requests_to(TOKEN_PATH)[0];
    assert_eq!(exchange.form["grant_type"], "authorization_code");
    let token = fixture
        .connector
        .access_token(&fixture.cancellation)
        .await
        .unwrap();
    assert!(fixture.resource_accepts(&token).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn pasting_the_redirect_address_completes_the_login_and_a_foreign_state_is_refused() {
    let fixture = Fixture::new();
    let login = fixture.start_login().await;
    let paste = login.paste();
    let redirect = fixture.approve(&login).await;
    let state = query_field(&redirect, "state").unwrap();
    let code = query_field(&redirect, "code").unwrap();
    let foreign = redirect.replace(&format!("state={state}"), "state=another-login");

    assert_eq!(paste.submit(&foreign), Err(CallbackError::StateMismatch));
    let browser = http_client().get(&foreign).send().await.unwrap();
    assert_eq!(browser.status(), StatusCode::BAD_REQUEST);
    assert!(!browser.text().await.unwrap().contains(&code));
    assert_eq!(
        paste.submit(&format!(
            "https://attacker.example/callback?code={code}&state={state}"
        )),
        Err(CallbackError::InvalidCallback)
    );

    paste.submit(&redirect).unwrap();
    assert_eq!(paste.submit(&redirect), Err(CallbackError::Ended));
    let late_browser = http_client().get(&redirect).send().await.unwrap();
    assert_eq!(late_browser.status(), StatusCode::CONFLICT);
    login.finish(&fixture.cancellation).await.unwrap();
    let token = fixture
        .connector
        .access_token(&fixture.cancellation)
        .await
        .unwrap();
    assert!(fixture.resource_accepts(&token).await);
}
