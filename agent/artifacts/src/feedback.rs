//! Independent Artifact feedback delivery for each receiving Agent; acknowledgements confirm transport, not model work.

use reqwest::Method;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::{ArtifactClient, ArtifactError, CredentialSnapshot, MAX_RESPONSE_BYTES, path_segment};

const MAX_CLAIM_RESPONSE_BYTES: usize = 256 * 1024;
const MAX_REQUESTS_PER_PAGE: usize = 50;
const MAX_IDENTIFIER_BYTES: usize = 256;
const MAX_LEASE_BYTES: usize = 64;
const MAX_SAFE_EPOCH: u64 = 9_007_199_254_740_991;

/// The Chat and Loop that an account links to an artifact; the service shows them, it never
/// routes by them.
pub struct AgentLinkRequest {
    pub binding_id: Uuid,
    pub client_instance_id: Uuid,
    pub chat_id: Uuid,
    pub loop_id: Uuid,
    pub history: tickloop_agent_protocol::ArtifactFeedbackHistory,
}

#[derive(Clone, Debug, Deserialize)]
pub struct AgentLink {
    pub artifact_id: String,
    pub binding_id: Uuid,
    pub epoch: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct FeedbackDelivery {
    pub delivery_id: String,
    pub feedback_id: String,
    pub thread_id: String,
    pub created_at: String,
}

#[derive(Debug, Deserialize)]
pub struct FeedbackPage {
    pub items: Vec<FeedbackDelivery>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ClaimedFeedback {
    pub delivery_id: String,
    pub feedback_id: String,
    pub claim_id: String,
    pub fencing_token: String,
    pub lease_expires_at: String,
    pub input: Value,
}

fn is_identifier(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_IDENTIFIER_BYTES
}

fn delivery_path(delivery_id: &str, action: &str) -> Result<String, ArtifactError> {
    Ok(format!(
        "/api/v1/agent/feedback/{}/{action}",
        path_segment(delivery_id)?
    ))
}

impl ArtifactClient {
    /// The current credential when it still belongs to `account_id`.
    fn account_credential(&self, account_id: &str) -> Result<CredentialSnapshot, ArtifactError> {
        let credential = self
            .credential_snapshot()?
            .ok_or(ArtifactError::AccessDenied)?;
        if credential.account_id != account_id {
            return Err(ArtifactError::AccountChanged);
        }
        Ok(credential)
    }

    /// Creates or replaces this account's link to the artifact; a replacement invalidates the
    /// claims of the link it replaces.
    pub async fn link_agent(
        &self,
        account_id: &str,
        artifact_id: &str,
        request: &AgentLinkRequest,
    ) -> Result<AgentLink, ArtifactError> {
        let credential = self.account_credential(account_id)?;
        let link: AgentLink = self
            .catalog_request_with_credential(
                Method::PUT,
                &format!("/api/v1/agent/links/{}", path_segment(artifact_id)?),
                &[],
                Some(json!({
                    "binding_id": request.binding_id,
                    "client_instance_id": request.client_instance_id,
                    "chat_id": request.chat_id,
                    "loop_id": request.loop_id,
                    "history": request.history,
                })),
                MAX_RESPONSE_BYTES,
                &credential,
            )
            .await?;
        if link.artifact_id != artifact_id
            || link.binding_id != request.binding_id
            || !(1..=MAX_SAFE_EPOCH).contains(&link.epoch)
        {
            return Err(ArtifactError::InvalidResponse);
        }
        Ok(link)
    }

    pub async fn pending_feedback(
        &self,
        account_id: &str,
        link: &AgentLink,
        cursor: Option<&str>,
    ) -> Result<FeedbackPage, ArtifactError> {
        let credential = self.account_credential(account_id)?;
        let binding_id = link.binding_id.to_string();
        let epoch = link.epoch.to_string();
        let mut query = vec![
            ("artifact_id", link.artifact_id.as_str()),
            ("binding_id", binding_id.as_str()),
            ("epoch", epoch.as_str()),
        ];
        if let Some(cursor) = cursor {
            query.push(("cursor", cursor));
        }
        let page: FeedbackPage = self
            .catalog_request_with_credential(
                Method::GET,
                "/api/v1/agent/feedback",
                &query,
                None,
                MAX_RESPONSE_BYTES,
                &credential,
            )
            .await?;
        if page.items.len() > MAX_REQUESTS_PER_PAGE
            || page.items.iter().any(|item| {
                Uuid::parse_str(&item.delivery_id).is_err()
                    || !is_identifier(&item.feedback_id)
                    || !is_identifier(&item.thread_id)
            })
            || page
                .next_cursor
                .as_deref()
                .is_some_and(|cursor| !is_identifier(cursor))
        {
            return Err(ArtifactError::InvalidResponse);
        }
        Ok(page)
    }

    pub async fn claim_feedback(
        &self,
        account_id: &str,
        link: &AgentLink,
        client_instance_id: Uuid,
        delivery_id: &str,
    ) -> Result<ClaimedFeedback, ArtifactError> {
        let credential = self.account_credential(account_id)?;
        let claim: ClaimedFeedback = self
            .catalog_request_with_credential(
                Method::POST,
                &delivery_path(delivery_id, "claim")?,
                &[],
                Some(json!({
                    "artifact_id": link.artifact_id,
                    "binding_id": link.binding_id,
                    "epoch": link.epoch,
                    "client_instance_id": client_instance_id,
                })),
                MAX_CLAIM_RESPONSE_BYTES,
                &credential,
            )
            .await?;
        if claim.delivery_id != delivery_id
            || !is_identifier(&claim.claim_id)
            || !is_identifier(&claim.fencing_token)
            || claim.lease_expires_at.is_empty()
            || claim.lease_expires_at.len() > MAX_LEASE_BYTES
            || claim.input.get("artifact_id").and_then(Value::as_str)
                != Some(link.artifact_id.as_str())
        {
            return Err(ArtifactError::InvalidResponse);
        }
        Ok(claim)
    }

    pub async fn release_feedback(
        &self,
        account_id: &str,
        artifact_id: &str,
        claim: &ClaimedFeedback,
    ) -> Result<(), ArtifactError> {
        let credential = self.account_credential(account_id)?;
        self.catalog_request_with_credential::<Value>(
            Method::POST,
            &delivery_path(&claim.delivery_id, "release")?,
            &[],
            Some(json!({
                "artifact_id": artifact_id,
                "claim_id": claim.claim_id,
                "fencing_token": claim.fencing_token,
            })),
            MAX_RESPONSE_BYTES,
            &credential,
        )
        .await
        .map(|_| ())
    }
    pub async fn acknowledge_feedback(
        &self,
        account_id: &str,
        artifact_id: &str,
        claim: &ClaimedFeedback,
    ) -> Result<(), ArtifactError> {
        let credential = self.account_credential(account_id)?;
        let value: Value = self.catalog_request_with_credential(
            Method::POST, &delivery_path(&claim.delivery_id, "ack")?, &[],
            Some(json!({"artifact_id": artifact_id, "claim_id": claim.claim_id, "fencing_token": claim.fencing_token})),
            MAX_RESPONSE_BYTES, &credential,
        ).await?;
        if value.get("delivery_id").and_then(Value::as_str) != Some(claim.delivery_id.as_str())
            || value.get("accepted").and_then(Value::as_bool) != Some(true)
        {
            return Err(ArtifactError::InvalidResponse);
        }
        Ok(())
    }

    pub async fn unlink_agent(
        &self,
        account_id: &str,
        link: &AgentLink,
    ) -> Result<(), ArtifactError> {
        let credential = self.account_credential(account_id)?;
        let value: Value = self
            .catalog_request_with_credential(
                Method::DELETE,
                &format!("/api/v1/agent/links/{}", path_segment(&link.artifact_id)?),
                &[],
                Some(json!({"binding_id": link.binding_id, "epoch": link.epoch})),
                MAX_RESPONSE_BYTES,
                &credential,
            )
            .await?;
        if value.get("removed").and_then(Value::as_bool) != Some(true) {
            return Err(ArtifactError::InvalidResponse);
        }
        Ok(())
    }
}
