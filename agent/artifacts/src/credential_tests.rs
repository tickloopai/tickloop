use super::tests::{request_body, server};
use super::*;
use async_trait::async_trait;
use serde_json::json;
use std::sync::{Arc, Mutex};

struct Tokens {
    token: Mutex<Option<String>>,
    rejected: Mutex<Vec<String>>,
}

#[async_trait]
impl ArtifactCredentials for Tokens {
    async fn bearer(&self) -> Result<Option<String>, ArtifactError> {
        Ok(self.token.lock().unwrap().clone())
    }
    async fn invalidate(&self, token: &str) -> Result<(), ArtifactError> {
        self.rejected.lock().unwrap().push(token.into());
        let mut current = self.token.lock().unwrap();
        if current.as_deref() == Some(token) {
            *current = Some("renewed-host-token".into());
        }
        Ok(())
    }
}

fn tokens() -> Arc<Tokens> {
    Arc::new(Tokens {
        token: Mutex::new(Some("old-host-token".into())),
        rejected: Mutex::new(Vec::new()),
    })
}

#[tokio::test]
async fn host_credentials_read_without_login_files_or_accounts_userinfo() {
    let (origin, requests) = server(|_, _, _| {
        (
            200,
            json!({"id":"doc","title":"Hosted","format":"markdown","access_role":"owner"})
                .to_string(),
        )
    })
    .await;
    let source = tokens();
    let client =
        ArtifactClient::with_credentials(&origin, "verified-user", source.clone()).unwrap();
    assert_eq!(client.read("doc").await.unwrap().title, "Hosted");
    assert_eq!(client.status().await.unwrap().unwrap().id, "verified-user");
    assert_eq!(requests.lock().unwrap().len(), 1);
    assert!(
        requests.lock().unwrap()[0]
            .to_ascii_lowercase()
            .contains("authorization: bearer old-host-token")
    );
    assert!(matches!(
        client.start_device_login().await,
        Err(ArtifactError::HostManagedCredentials)
    ));
    assert!(matches!(
        client.logout().await,
        Err(ArtifactError::HostManagedCredentials)
    ));
    assert!(matches!(
        client.credential_directory(),
        Err(ArtifactError::HostManagedCredentials)
    ));
    *source.token.lock().unwrap() = None;
    assert!(client.access_snapshot().await.unwrap().is_none());
    assert!(client.credential_snapshot().unwrap().is_none());
}

#[tokio::test]
async fn host_401_invalidates_and_retries_once_with_the_same_mutation_body() {
    let (origin, requests) = server(|request, _, _| {
        if request
            .to_ascii_lowercase()
            .contains("authorization: bearer old-host-token")
        {
            (401, json!({"error":{"code":"invalid_token"}}).to_string())
        } else {
            (
                200,
                json!({"operation_id":request_body(request)["operation_id"],"status":"committed"})
                    .to_string(),
            )
        }
    })
    .await;
    let source = tokens();
    let client =
        ArtifactClient::with_credentials(&origin, "verified-user", source.clone()).unwrap();
    assert_eq!(
        client.rename("doc", "Renamed", 1).await.unwrap().status,
        "committed"
    );
    assert_eq!(*source.rejected.lock().unwrap(), ["old-host-token"]);
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(request_body(&requests[0]), request_body(&requests[1]));
    assert!(
        requests[1]
            .to_ascii_lowercase()
            .contains("authorization: bearer renewed-host-token")
    );
    assert!(Uuid::parse_str(request_body(&requests[0])["operation_id"].as_str().unwrap()).is_ok());
}

#[tokio::test]
async fn host_401_retry_is_bounded_and_does_not_retry_other_statuses() {
    for status in [401, 403, 503] {
        let (origin, requests) =
            server(move |_, _, _| (status, json!({"error":{"code":"refused"}}).to_string())).await;
        let source = tokens();
        let client =
            ArtifactClient::with_credentials(&origin, "verified-user", source.clone()).unwrap();
        assert!(client.read("doc").await.is_err());
        let expected = if status == 401 { 2 } else { 1 };
        assert_eq!(requests.lock().unwrap().len(), expected);
        assert_eq!(
            source.rejected.lock().unwrap().len(),
            if status == 401 { 2 } else { 0 }
        );
    }
}

#[tokio::test]
async fn host_credential_cache_keys_are_private_to_the_constructed_chat_client() {
    let source = tokens();
    let first =
        ArtifactClient::with_credentials("https://tickloop.ai", "verified-user", source.clone())
            .unwrap();
    let second =
        ArtifactClient::with_credentials("https://tickloop.ai", "verified-user", source.clone())
            .unwrap();
    let credential = first.access_snapshot().await.unwrap().unwrap();
    let key = first.credential_key(&credential);
    let other = second.access_snapshot().await.unwrap().unwrap();
    assert_ne!(key, second.credential_key(&other));
    first
        .invalidate_credential(&credential.access_token)
        .await
        .unwrap();
    let renewed = first.access_snapshot().await.unwrap().unwrap();
    assert_eq!(key, first.credential_key(&renewed));
    assert!(first.credentials_match(&credential, &renewed));
    let foreign = reqwest::Client::new().get("https://other.example/artifact/doc");
    assert!(matches!(
        first.send_authenticated(foreign, &renewed).await,
        Err(ArtifactError::InvalidOrigin)
    ));
}

/// A source that scopes tokens by Chat, as a cloud host does for a family's members.
struct ChatTokens {
    chat: Option<Uuid>,
}

#[async_trait]
impl ArtifactCredentials for ChatTokens {
    async fn bearer(&self) -> Result<Option<String>, ArtifactError> {
        Ok(Some(match self.chat {
            Some(chat) => format!("chat-{chat}"),
            None => "family-token".into(),
        }))
    }

    fn for_chat(&self, chat_id: Uuid) -> Option<Arc<dyn ArtifactCredentials>> {
        Some(Arc::new(ChatTokens {
            chat: Some(chat_id),
        }))
    }
}

#[tokio::test]
async fn a_chat_scoped_client_links_with_that_chat_s_credentials() {
    let (origin, requests) = server(|request, _, _| {
        let body = request_body(request);
        (
            200,
            json!({"artifact_id":"doc","binding_id":body["binding_id"],"epoch":1}).to_string(),
        )
    })
    .await;
    let client = ArtifactClient::with_credentials(
        &origin,
        "verified-user",
        Arc::new(ChatTokens { chat: None }),
    )
    .unwrap();
    let member = Uuid::new_v4();
    let link = AgentLinkRequest {
        binding_id: Uuid::new_v4(),
        client_instance_id: Uuid::new_v4(),
        chat_id: member,
        loop_id: Uuid::new_v4(),
        history: tickloop_agent_protocol::ArtifactFeedbackHistory::NewOnly,
    };
    client
        .for_chat(member)
        .link_agent("verified-user", "doc", &link)
        .await
        .unwrap();
    assert!(
        requests.lock().unwrap()[0]
            .to_ascii_lowercase()
            .contains(&format!("authorization: bearer chat-{member}"))
    );

    // Without a Chat-scoped source the client is unchanged.
    let plain = ArtifactClient::with_credentials(&origin, "verified-user", tokens()).unwrap();
    plain
        .for_chat(member)
        .link_agent("verified-user", "doc", &link)
        .await
        .unwrap();
    assert!(
        requests.lock().unwrap()[1]
            .to_ascii_lowercase()
            .contains("authorization: bearer old-host-token")
    );
}
