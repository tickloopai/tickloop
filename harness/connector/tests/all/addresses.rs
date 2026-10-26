use std::sync::Arc;

use tick_oauth_test_server::{AuthorizationServer, REVOCATION_PATH, TOKEN_PATH};
use tickloop_harness_connector::{
    ClientRegistration, Connector, ConnectorCredential, InMemoryCredentials, Logout, OAuthSettings,
    OAuthToken, TokenError,
};
use tokio_util::sync::CancellationToken;

const REMOTE_SERVER: &str = "https://mcp.example.test/mcp";

/// A remote server's credential whose endpoints, as its metadata could claim, are services on
/// this machine.
fn credential_pointing_at(local: &AuthorizationServer) -> ConnectorCredential {
    let issuer = local.issuer().to_owned();
    ConnectorCredential {
        server_url: REMOTE_SERVER.into(),
        registration: ClientRegistration {
            authorization_server: issuer.clone(),
            client_id: "client-1".into(),
            redirect_uri: "http://localhost:33418/callback".into(),
        },
        resource: Some(REMOTE_SERVER.into()),
        token: Some(OAuthToken {
            access_token: "access-remote".into(),
            token_type: "Bearer".into(),
            refresh_token: Some("refresh-remote".into()),
            scope: None,
            token_endpoint: format!("{issuer}{TOKEN_PATH}"),
            revocation_endpoint: Some(format!("{issuer}{REVOCATION_PATH}")),
            issuer,
            client_id: "client-1".into(),
            expires_at: 0,
        }),
    }
}

#[tokio::test]
async fn a_remote_servers_login_never_sends_requests_to_this_machine() {
    let local = AuthorizationServer::start();
    let credentials = Arc::new(InMemoryCredentials::default());
    credentials.replace(Some(credential_pointing_at(&local)));
    let connector =
        Connector::new(REMOTE_SERVER, OAuthSettings::default(), credentials.clone()).unwrap();
    let cancellation = CancellationToken::new();

    assert_eq!(
        connector.access_token(&cancellation).await,
        Err(TokenError::RenewalFailed)
    );
    assert_eq!(
        connector.logout(&cancellation).await,
        Ok(Logout::RevocationFailed)
    );
    assert!(local.requests().is_empty());
    assert_eq!(credentials.current().unwrap().token, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_metadata_address_on_this_machine_lets_the_login_stay_here() {
    let local = AuthorizationServer::start();
    let credentials = Arc::new(InMemoryCredentials::default());
    credentials.replace(Some(ConnectorCredential {
        token: None,
        ..credential_pointing_at(&local)
    }));
    let connector = Connector::new(
        REMOTE_SERVER,
        OAuthSettings {
            authorization_server_metadata_url: Some(format!(
                "{}{}",
                local.issuer(),
                tick_oauth_test_server::METADATA_PATH
            )),
            ..OAuthSettings::default()
        },
        credentials,
    )
    .unwrap();

    connector
        .start_login(None, &CancellationToken::new())
        .await
        .unwrap();
}

#[tokio::test]
async fn an_expired_token_without_renewal_needs_a_login() {
    let local = AuthorizationServer::start();
    let mut credential = credential_pointing_at(&local);
    credential.server_url = local.resource();
    if let Some(token) = credential.token.as_mut() {
        token.refresh_token = None;
    }
    let credentials = Arc::new(InMemoryCredentials::default());
    credentials.replace(Some(credential));
    let connector =
        Connector::new(&local.resource(), OAuthSettings::default(), credentials).unwrap();

    assert_eq!(
        connector.access_token(&CancellationToken::new()).await,
        Err(TokenError::NeedsLogin)
    );
    assert!(local.requests().is_empty());
}
