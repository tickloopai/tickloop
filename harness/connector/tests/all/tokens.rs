use std::sync::Arc;

use tick_oauth_test_server::{REVOCATION_PATH, TOKEN_PATH, TokenReply};
use tickloop_harness_connector::{
    Connector, InMemoryCredentials, Logout, OAuthSettings, TokenError,
};

use crate::support::Fixture;

#[tokio::test(flavor = "multi_thread")]
async fn a_token_expiring_within_thirty_seconds_is_renewed_and_saved() {
    let fixture = Fixture::new();
    fixture.server.issue_tokens_lasting(20);
    fixture.log_in().await;
    let login = fixture.credentials.current().unwrap().login;
    assert!(login.is_some(), "a login names itself");
    let first = fixture.credentials.current().unwrap().token.unwrap();

    let token = fixture
        .connector
        .access_token(&fixture.cancellation)
        .await
        .unwrap();

    assert_ne!(token.secret(), first.access_token);
    assert!(fixture.resource_accepts(&token).await);
    let renewal = &fixture.server.requests_to(TOKEN_PATH)[1];
    assert_eq!(renewal.form["grant_type"], "refresh_token");
    assert_eq!(
        renewal.form.get("refresh_token"),
        first.refresh_token.as_ref()
    );
    assert_eq!(
        renewal.form.get("resource"),
        Some(&fixture.server.resource())
    );
    let saved = fixture.credentials.current().unwrap();
    assert_eq!(saved.token.unwrap().access_token, token.secret());
    assert_eq!(saved.login, login, "a renewed token keeps its login");
}

#[tokio::test(flavor = "multi_thread")]
async fn each_login_is_named_anew() {
    let fixture = Fixture::new();
    fixture.log_in().await;
    let first = fixture.credentials.current().unwrap().login;
    fixture.log_in().await;
    let second = fixture.credentials.current().unwrap().login;
    assert!(first.is_some() && second.is_some());
    assert_ne!(first, second);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fresh_token_is_used_without_asking_the_server() {
    let fixture = Fixture::new();
    fixture.log_in().await;
    let exchanges = fixture.server.requests_to(TOKEN_PATH).len();

    let token = fixture
        .connector
        .access_token(&fixture.cancellation)
        .await
        .unwrap();

    assert!(fixture.resource_accepts(&token).await);
    assert_eq!(fixture.server.requests_to(TOKEN_PATH).len(), exchanges);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rejected_token_is_renewed_once_for_all_agents() {
    let fixture = Fixture::new();
    fixture.log_in().await;
    let rejected = fixture
        .connector
        .access_token(&fixture.cancellation)
        .await
        .unwrap();
    let other_agent = fixture.another_connector();

    let (first, second) = tokio::join!(
        fixture
            .connector
            .renew_rejected(&rejected, &fixture.cancellation),
        other_agent.renew_rejected(&rejected, &fixture.cancellation),
    );

    let (first, second) = (first.unwrap(), second.unwrap());
    assert_eq!(first, second);
    assert_ne!(first, rejected);
    assert!(fixture.resource_accepts(&first).await);
    assert_eq!(fixture.server.requests_to(TOKEN_PATH).len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn agents_renewing_an_expiring_token_together_renew_it_once() {
    let fixture = Fixture::new();
    fixture.server.issue_tokens_lasting(10);
    fixture.log_in().await;
    fixture.server.issue_tokens_lasting(3600);
    let other_agent = fixture.another_connector();

    let (first, second) = tokio::join!(
        fixture.connector.access_token(&fixture.cancellation),
        other_agent.access_token(&fixture.cancellation),
    );

    assert_eq!(first.unwrap(), second.unwrap());
    assert_eq!(fixture.server.requests_to(TOKEN_PATH).len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_invalid_grant_forgets_the_token_and_keeps_the_registration() {
    let fixture = Fixture::new();
    fixture.log_in().await;
    let rejected = fixture
        .connector
        .access_token(&fixture.cancellation)
        .await
        .unwrap();
    fixture
        .server
        .answer_polls([TokenReply::Error(400, "invalid_grant".into())]);

    let error = fixture
        .connector
        .renew_rejected(&rejected, &fixture.cancellation)
        .await
        .unwrap_err();

    assert_eq!(error, TokenError::NeedsLogin);
    let credential = fixture.credentials.current().unwrap();
    assert_eq!(credential.token, None);
    assert_eq!(credential.registration.client_id, "client-1");
    assert_eq!(
        fixture.connector.access_token(&fixture.cancellation).await,
        Err(TokenError::NeedsLogin)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_failure_during_renewal_keeps_the_login() {
    let fixture = Fixture::new();
    fixture.log_in().await;
    let rejected = fixture
        .connector
        .access_token(&fixture.cancellation)
        .await
        .unwrap();
    fixture.server.answer_polls([TokenReply::Status(503)]);

    assert_eq!(
        fixture
            .connector
            .renew_rejected(&rejected, &fixture.cancellation)
            .await,
        Err(TokenError::RenewalFailed)
    );
    assert!(fixture.credentials.current().unwrap().token.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn logout_revokes_the_refresh_token_and_forgets_it() {
    let fixture = Fixture::new();
    fixture.log_in().await;
    let token = fixture.credentials.current().unwrap().token.unwrap();

    let outcome = fixture
        .connector
        .logout(&fixture.cancellation)
        .await
        .unwrap();

    assert_eq!(outcome, Logout::Revoked);
    let revocation = &fixture.server.requests_to(REVOCATION_PATH)[0];
    assert_eq!(revocation.form.get("token"), token.refresh_token.as_ref());
    assert_eq!(revocation.form["token_type_hint"], "refresh_token");
    assert_eq!(fixture.credentials.current().unwrap().token, None);
    assert_eq!(
        fixture.connector.access_token(&fixture.cancellation).await,
        Err(TokenError::NeedsLogin)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refusal_other_than_invalid_grant_keeps_the_login() {
    let fixture = Fixture::new();
    fixture.log_in().await;
    let rejected = fixture
        .connector
        .access_token(&fixture.cancellation)
        .await
        .unwrap();
    fixture
        .server
        .answer_polls([TokenReply::Error(400, "invalid_request".into())]);

    assert_eq!(
        fixture
            .connector
            .renew_rejected(&rejected, &fixture.cancellation)
            .await,
        Err(TokenError::RenewalFailed)
    );
    assert!(fixture.credentials.current().unwrap().token.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_revocation_still_logs_out() {
    let fixture = Fixture::new();
    fixture.log_in().await;
    fixture.server.answer_revocations(503);

    assert_eq!(
        fixture.connector.logout(&fixture.cancellation).await,
        Ok(Logout::RevocationFailed)
    );
    assert_eq!(fixture.credentials.current().unwrap().token, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_token_is_not_used_for_a_changed_server_address() {
    let fixture = Fixture::new();
    fixture.log_in().await;
    let moved = Connector::new(
        &format!("{}/moved", fixture.server.resource()),
        OAuthSettings::default(),
        fixture.credentials.clone(),
    )
    .unwrap();

    assert_eq!(
        moved.access_token(&fixture.cancellation).await,
        Err(TokenError::NeedsLogin)
    );
}

#[tokio::test]
async fn without_a_login_no_token_is_available() {
    let connector = Connector::new(
        "https://mcp.example.test/mcp",
        OAuthSettings::default(),
        Arc::new(InMemoryCredentials::default()),
    )
    .unwrap();

    assert_eq!(
        connector.access_token(&Default::default()).await,
        Err(TokenError::NeedsLogin)
    );
    assert_eq!(
        connector.logout(&Default::default()).await,
        Ok(Logout::NotLoggedIn)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn tokens_never_appear_in_debug_output_or_errors() {
    let fixture = Fixture::new();
    fixture.log_in().await;
    let token = fixture
        .connector
        .access_token(&fixture.cancellation)
        .await
        .unwrap();
    let credential = fixture.credentials.current().unwrap();
    let refresh_token = credential
        .token
        .as_ref()
        .unwrap()
        .refresh_token
        .clone()
        .unwrap();
    fixture
        .server
        .answer_polls([TokenReply::Error(400, "invalid_grant".into())]);
    let error = fixture
        .connector
        .renew_rejected(&token, &fixture.cancellation)
        .await
        .unwrap_err();

    for text in [
        format!("{token:?}"),
        format!("{credential:?}"),
        format!("{:?}", fixture.connector),
        format!("{error:?} {error}"),
    ] {
        assert!(!text.contains(token.secret()), "{text}");
        assert!(!text.contains(&refresh_token), "{text}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn renewing_a_login_saved_without_a_name_names_it_once() {
    let fixture = Fixture::new();
    fixture.server.issue_tokens_lasting(20);
    fixture.log_in().await;
    let mut saved = fixture.credentials.current().unwrap();
    saved.login = None;
    fixture.credentials.replace(Some(saved));

    fixture
        .connector
        .access_token(&fixture.cancellation)
        .await
        .unwrap();
    let named = fixture.credentials.current().unwrap().login;
    assert!(named.is_some(), "the renewal names the login");
    fixture
        .connector
        .access_token(&fixture.cancellation)
        .await
        .unwrap();
    assert_eq!(fixture.server.requests_to(TOKEN_PATH).len(), 3);
    assert_eq!(
        fixture.credentials.current().unwrap().login,
        named,
        "later renewals keep the name"
    );
}
