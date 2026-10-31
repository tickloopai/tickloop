use std::sync::Arc;

use tick_oauth_test_server::{AuthorizationServer, REVOCATION_PATH, TOKEN_PATH};
use tickloop_harness_connector::{
    ClientRegistration, ConfigurationError, Connector, ConnectorCredential, InMemoryCredentials,
    LoginError, Logout, OAuthSettings, OAuthToken, PendingServerLogin, Reach, TokenError,
};
use tokio_util::sync::CancellationToken;

const REMOTE_SERVER: &str = "https://mcp.example.test/mcp";

#[tokio::test]
async fn login_never_reads_cross_origin_protected_resource_metadata() {
    for supplied_challenge in [true, false] {
        let server = AuthorizationServer::start();
        let foreign = AuthorizationServer::start();
        server.edit_metadata(|metadata| metadata["issuer"] = foreign.issuer().into());
        let credentials = Arc::new(InMemoryCredentials::default());
        let connector = Connector::new(
            &server.resource(),
            OAuthSettings::default(),
            credentials.clone(),
        )
        .unwrap();
        let challenge = format!(
            "Bearer resource_metadata=\"{}{}{}\"",
            foreign.issuer(),
            tick_oauth_test_server::PROTECTED_RESOURCE_METADATA_PATH,
            tick_oauth_test_server::RESOURCE_PATH,
        );
        let result = connector
            .start_login(
                supplied_challenge.then_some(challenge.as_str()),
                &CancellationToken::new(),
            )
            .await;
        let Err(LoginError::Discovery(cause)) = result else {
            panic!("cross-origin metadata was accepted (supplied challenge: {supplied_challenge})");
        };
        assert!(
            cause.contains("protected resource metadata must have the MCP server's origin"),
            "{cause}"
        );
        assert!(
            foreign.requests().is_empty(),
            "cross-origin metadata was requested (supplied challenge: {supplied_challenge})"
        );
        assert!(credentials.current().is_none());
    }
}

#[tokio::test]
async fn an_unreachable_server_names_the_failure_without_its_path_or_query() {
    let closed_port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let connector = Connector::new(
        &format!("http://127.0.0.1:{closed_port}/secret-path/mcp?key=secret-key"),
        OAuthSettings::default(),
        Arc::new(InMemoryCredentials::default()),
    )
    .unwrap();

    let Err(error) = connector.start_login(None, &CancellationToken::new()).await else {
        panic!("a login started without a reachable server");
    };
    let message = error.to_string();
    assert!(
        message.starts_with("the server publishes no usable OAuth authorization server metadata: "),
        "{message}"
    );
    assert!(
        message.contains(&format!("http://127.0.0.1:{closed_port}")),
        "{message}"
    );
    assert!(message.contains("client error (Connect)"), "{message}");
    assert!(!message.contains("secret"), "{message}");
    assert!(!message.contains('\n'), "{message}");
}

/// A remote server's credential whose endpoints, as its metadata could claim, are services on
/// this machine.
fn credential_pointing_at(local: &AuthorizationServer) -> ConnectorCredential {
    let issuer = local.issuer().to_owned();
    ConnectorCredential {
        login: None,
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
        login: None,
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

#[test]
fn a_public_only_connector_needs_public_https_addresses() {
    let local = AuthorizationServer::start();
    let connector = |server: &str, metadata: Option<&str>| {
        Connector::new(
            server,
            OAuthSettings {
                authorization_server_metadata_url: metadata.map(str::to_owned),
                ..OAuthSettings::default()
            },
            Arc::new(InMemoryCredentials::default()),
        )
        .unwrap()
        .with_reach(Reach::PublicAddresses)
        .map(|_| ())
    };

    for server in [
        local.resource(),
        "https://localhost/mcp".into(),
        "https://127.0.0.1/mcp".into(),
        "https://10.0.0.1/mcp".into(),
        "https://169.254.169.254/mcp".into(),
        "https://[::1]/mcp".into(),
        "https://[fd00::1]/mcp".into(),
    ] {
        assert_eq!(
            connector(&server, None),
            Err(ConfigurationError::PublicServerAddress),
            "{server}"
        );
    }
    assert_eq!(
        connector(
            REMOTE_SERVER,
            Some("https://192.168.0.1/.well-known/oauth-authorization-server")
        ),
        Err(ConfigurationError::PublicMetadataAddress)
    );
    assert_eq!(connector(REMOTE_SERVER, None), Ok(()));
    assert_eq!(
        connector(
            REMOTE_SERVER,
            Some("https://auth.example.test/.well-known/x")
        ),
        Ok(())
    );
}

/// A credential of a public-only remote server whose endpoints are private addresses.
fn credential_pointing_at_private_addresses() -> ConnectorCredential {
    let issuer = "https://auth.example.test".to_owned();
    ConnectorCredential {
        login: None,
        server_url: REMOTE_SERVER.into(),
        registration: ClientRegistration {
            authorization_server: issuer.clone(),
            client_id: "client-1".into(),
            redirect_uri: "https://chat.example.test/connectors/callback".into(),
        },
        resource: Some(REMOTE_SERVER.into()),
        token: Some(OAuthToken {
            access_token: "access-remote".into(),
            token_type: "Bearer".into(),
            refresh_token: Some("refresh-remote".into()),
            scope: None,
            token_endpoint: "https://10.0.0.7/token".into(),
            revocation_endpoint: Some("https://169.254.169.254/revoke".into()),
            issuer,
            client_id: "client-1".into(),
            expires_at: 0,
        }),
    }
}

#[tokio::test]
async fn a_public_only_connector_renews_and_revokes_only_at_public_addresses() {
    let credentials = Arc::new(InMemoryCredentials::default());
    credentials.replace(Some(credential_pointing_at_private_addresses()));
    let connector = Connector::new(REMOTE_SERVER, OAuthSettings::default(), credentials.clone())
        .unwrap()
        .with_reach(Reach::PublicAddresses)
        .unwrap();
    let cancellation = CancellationToken::new();

    assert_eq!(
        connector.access_token(&cancellation).await,
        Err(TokenError::RenewalFailed)
    );
    assert_eq!(
        connector.logout(&cancellation).await,
        Ok(Logout::RevocationFailed)
    );
    assert_eq!(credentials.current().unwrap().token, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_public_only_server_login_exchanges_the_code_only_at_a_public_address() {
    let local = AuthorizationServer::start();
    let local_connector = Connector::new(
        &local.resource(),
        OAuthSettings::default(),
        Arc::new(InMemoryCredentials::default()),
    )
    .unwrap();
    let login = local_connector
        .start_server_login(
            "https://chat.example.test/connectors/callback",
            None,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let state = login.state().to_owned();
    let mut saved = serde_json::to_value(&login).unwrap();
    saved["server_url"] = REMOTE_SERVER.into();
    let mut private_token_endpoints = Vec::new();
    for token_endpoint in [local.issuer().to_owned(), "https://10.0.0.7".into()] {
        saved["endpoints"]["token"] = format!("{token_endpoint}{TOKEN_PATH}").into();
        private_token_endpoints
            .push(serde_json::from_value::<PendingServerLogin>(saved.clone()).unwrap());
    }
    let credentials = Arc::new(InMemoryCredentials::default());
    let connector = Connector::new(REMOTE_SERVER, OAuthSettings::default(), credentials.clone())
        .unwrap()
        .with_reach(Reach::PublicAddresses)
        .unwrap();

    for pending in private_token_endpoints {
        assert_eq!(
            connector
                .finish_server_login(
                    pending,
                    &format!("code=code-1&state={state}"),
                    &CancellationToken::new()
                )
                .await,
            Err(LoginError::TokenExchange)
        );
    }
    assert!(local.requests_to(TOKEN_PATH).is_empty());
    assert_eq!(credentials.current(), None);
}
