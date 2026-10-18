//! Renewing a saved token and revoking it.

use reqwest::header::HeaderValue;
use serde_json::json;
use tick_oauth::{RefreshError, refresh, revoke};
use tick_packages::OAuthToken;
use tick_test_support::authorization_server::{
    AuthorizationServer, REVOCATION_PATH, TOKEN_PATH, TokenReply,
};

use crate::support::{http, now};

fn token(server: &AuthorizationServer, refresh_token: Option<&str>) -> OAuthToken {
    let issuer = server.issuer();
    OAuthToken {
        access_token: "access-old".into(),
        token_type: "Bearer".into(),
        refresh_token: refresh_token.map(str::to_owned),
        scope: Some("4".into()),
        issuer: issuer.to_owned(),
        client_id: "client-1".into(),
        token_endpoint: format!("{issuer}{TOKEN_PATH}"),
        revocation_endpoint: Some(format!("{issuer}{REVOCATION_PATH}")),
        expires_at: now() - 10,
    }
}

#[tokio::test]
async fn a_renewal_keeps_what_the_server_does_not_replace() {
    let server = AuthorizationServer::start();
    let saved = token(&server, Some("refresh-old"));
    server.answer_polls([TokenReply::Approve]);

    let renewed = refresh(&http(), &saved, "refresh-old", |request| {
        request.header("x-extra", HeaderValue::from_static("yes"))
    })
    .await
    .unwrap();
    assert_eq!(renewed.access_token, "access-1");
    assert_eq!(renewed.refresh_token.as_deref(), Some("refresh-1"));
    assert_eq!(renewed.scope.as_deref(), Some("4"));
    assert!(renewed.expires_at > now() + 3000);
    assert_eq!(
        (renewed.issuer.as_str(), renewed.client_id.as_str()),
        (saved.issuer.as_str(), "client-1")
    );
    let request = &server.requests_to(TOKEN_PATH)[0];
    assert_eq!(request.form["grant_type"], "refresh_token");
    assert_eq!(request.form["refresh_token"], "refresh-old");
    assert_eq!(request.form["client_id"], "client-1");

    server.answer_polls([TokenReply::Token(
        json!({"access_token": "access-2", "token_type": "Bearer", "expires_in": 60}),
    )]);
    let renewed = refresh(&http(), &saved, "refresh-old", |request| request)
        .await
        .unwrap();
    assert_eq!(renewed.refresh_token.as_deref(), Some("refresh-old"));
    assert!(renewed.expires_at <= now() + 60);
}

/// A 400 with an OAuth error, or any 401, even bare, refuses the refresh token; a server that
/// fails, even with an OAuth error in its body, leaves the login for a later renewal.
#[tokio::test]
async fn only_an_oauth_error_response_or_a_401_refuses_a_renewal() {
    let server = AuthorizationServer::start();
    let saved = token(&server, Some("refresh-old"));
    server.answer_polls([
        TokenReply::Error(400, "invalid_grant".into()),
        TokenReply::Error(401, "invalid_client".into()),
        TokenReply::Status(401),
        TokenReply::Error(503, "temporarily_unavailable".into()),
        TokenReply::Error(500, "invalid_grant".into()),
        TokenReply::Status(400),
        TokenReply::Token(json!({"token_type": "Bearer"})),
    ]);
    let issuer = server.issuer();
    let expected = [
        RefreshError::Refused("invalid_grant".into()),
        RefreshError::Refused("invalid_client".into()),
        RefreshError::Refused("HTTP 401 Unauthorized".into()),
        RefreshError::Failed(format!(
            "{issuer}{TOKEN_PATH} answered HTTP 503 Service Unavailable"
        )),
        RefreshError::Failed(format!(
            "{issuer}{TOKEN_PATH} answered HTTP 500 Internal Server Error"
        )),
        RefreshError::Failed(format!(
            "{issuer}{TOKEN_PATH} answered HTTP 400 Bad Request"
        )),
        RefreshError::Failed(format!("{issuer} sent an invalid token response")),
    ];
    for expected in expected {
        let error = refresh(&http(), &saved, "refresh-old", |request| request)
            .await
            .unwrap_err();
        assert_eq!(error, expected);
        assert!(!error.to_string().contains("refresh-old"), "{error}");
    }

    let mut elsewhere = saved.clone();
    elsewhere.token_endpoint = "http://example.com/token".into();
    assert!(matches!(
        refresh(&http(), &elsewhere, "refresh-old", |request| request).await,
        Err(RefreshError::Failed(reason)) if reason.contains("is refused: only https")
    ));
}

#[tokio::test]
async fn revoking_names_the_refresh_token_or_else_the_access_token() {
    let server = AuthorizationServer::start();
    revoke(&http(), &token(&server, Some("refresh-old")))
        .await
        .unwrap();
    revoke(&http(), &token(&server, None)).await.unwrap();
    let requests = server.requests_to(REVOCATION_PATH);
    assert_eq!(
        (
            requests[0].form["token"].as_str(),
            requests[0].form["token_type_hint"].as_str()
        ),
        ("refresh-old", "refresh_token")
    );
    assert_eq!(
        (
            requests[1].form["token"].as_str(),
            requests[1].form["token_type_hint"].as_str()
        ),
        ("access-old", "access_token")
    );
    assert_eq!(requests[1].form["client_id"], "client-1");

    server.answer_revocations(503);
    let failure = revoke(&http(), &token(&server, Some("refresh-old")))
        .await
        .unwrap_err();
    assert_eq!(
        failure,
        format!(
            "{}{REVOCATION_PATH} answered HTTP 503 Service Unavailable",
            server.issuer()
        )
    );
    let mut unrevocable = token(&server, Some("refresh-old"));
    unrevocable.revocation_endpoint = None;
    let before = server.requests_to(REVOCATION_PATH).len();
    revoke(&http(), &unrevocable).await.unwrap();
    assert_eq!(server.requests_to(REVOCATION_PATH).len(), before);
}

#[tokio::test]
async fn resource_refresh_rotates_the_refresh_token_and_keeps_the_selected_audience() {
    let server = AuthorizationServer::start_with_path("/accounts");
    server.answer_polls([TokenReply::Approve, TokenReply::Approve]);
    let saved = token(&server, Some("refresh-old"));
    let first = tick_oauth::refresh_for_resource(
        &http(),
        &saved,
        "refresh-old",
        Some("https://resource.example/artifact"),
        |request| request,
    )
    .await
    .unwrap();
    let second = tick_oauth::refresh_for_resource(
        &http(),
        &first,
        first.refresh_token().unwrap(),
        Some("https://resource.example/other"),
        |request| request,
    )
    .await
    .unwrap();
    assert_eq!(second.refresh_token(), Some("refresh-2"));
    let requests = server.requests_to(TOKEN_PATH);
    assert_eq!(
        requests[0].form["resource"],
        "https://resource.example/artifact"
    );
    assert_eq!(
        requests[1].form["resource"],
        "https://resource.example/other"
    );
    assert_eq!(requests[1].form["refresh_token"], "refresh-1");
}
