use reqwest::StatusCode;
use serde_json::json;
use tick_oauth_test_server::{AUTHORIZATION_PATH, REGISTRATION_PATH, TOKEN_PATH};

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

/// The shape Longbridge publishes: the issuer (without a trailing slash) and the token endpoint
/// on the MCP server's origin, authorization and registration on another host, public clients
/// only and numeric scopes.
#[tokio::test(flavor = "multi_thread")]
async fn authorization_and_registration_on_another_host_complete_the_login() {
    let fixture = Fixture::new();
    let issuer = fixture.server.issuer().to_owned();
    let other_host = issuer.replace("127.0.0.1", "localhost");
    let scopes = json!(["4", "6", "10", "11"]);
    fixture.server.edit_metadata(|metadata| {
        metadata["authorization_endpoint"] = format!("{other_host}{AUTHORIZATION_PATH}").into();
        metadata["registration_endpoint"] = format!("{other_host}{REGISTRATION_PATH}").into();
        metadata["jwks_uri"] = format!("{other_host}/.well-known/jwks.json").into();
        metadata["scopes_supported"] = scopes.clone();
        metadata["code_challenge_methods_supported"] = json!(["S256", "plain"]);
    });
    fixture
        .server
        .add_resource_metadata_fields(json!({"scopes_supported": scopes}));

    drop(fixture.start_login().await);
    let login = fixture
        .connector
        .start_login(None, &fixture.cancellation)
        .await
        .unwrap();

    let authorization_url = reqwest::Url::parse(login.authorization_url()).unwrap();
    assert_eq!(authorization_url.host_str(), Some("localhost"));
    assert_eq!(
        query_field(login.authorization_url(), "scope").as_deref(),
        Some("4 6 10 11")
    );
    assert_eq!(fixture.server.requests_to(REGISTRATION_PATH).len(), 2);
    let redirect = fixture.approve(&login).await;
    let browser = http_client().get(&redirect).send().await.unwrap();
    assert_eq!(browser.status(), StatusCode::OK);
    login.finish(&fixture.cancellation).await.unwrap();

    let credential = fixture.credentials.current().unwrap();
    assert_eq!(credential.registration.authorization_server, issuer);
    let token = credential.token.unwrap();
    assert_eq!(token.token_endpoint, format!("{issuer}{TOKEN_PATH}"));
    let token = fixture
        .connector
        .access_token(&fixture.cancellation)
        .await
        .unwrap();
    assert!(fixture.resource_accepts(&token).await);
}
