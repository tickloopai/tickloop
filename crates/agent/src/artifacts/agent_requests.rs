//! The host lifecycle of @tickloop feedback requests: an account's Agent link to an artifact and
//! the pending, claim, renew and release calls under `/api/v1/agent/*`.

use reqwest::Method;
use serde::{Deserialize, Serialize};
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
}

#[derive(Clone, Debug, Deserialize)]
pub struct AgentLink {
    pub artifact_id: String,
    pub binding_id: Uuid,
    pub epoch: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PendingRequest {
    pub request_id: String,
    pub created_at: String,
}

#[derive(Debug, Deserialize)]
pub struct PendingRequestPage {
    pub items: Vec<PendingRequest>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ClaimedRequest {
    pub request_id: String,
    pub claim_id: String,
    pub fencing_token: String,
    pub lease_expires_at: String,
    pub input: Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseOutcome {
    /// The request returns to pending and is delivered again.
    Retry,
    Failed,
}

#[derive(Deserialize)]
struct RenewedLease {
    lease_expires_at: String,
}

fn is_identifier(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_IDENTIFIER_BYTES
}

fn request_path(request_id: &str, action: &str) -> Result<String, ArtifactError> {
    Ok(format!(
        "/api/v1/agent/requests/{}/{action}",
        path_segment(request_id)?
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

    pub async fn pending_requests(
        &self,
        account_id: &str,
        link: &AgentLink,
        cursor: Option<&str>,
    ) -> Result<PendingRequestPage, ArtifactError> {
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
        let page: PendingRequestPage = self
            .catalog_request_with_credential(
                Method::GET,
                "/api/v1/agent/requests",
                &query,
                None,
                MAX_RESPONSE_BYTES,
                &credential,
            )
            .await?;
        if page.items.len() > MAX_REQUESTS_PER_PAGE
            || page.items.iter().any(|item| {
                !is_identifier(&item.request_id) || path_segment(&item.request_id).is_err()
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

    pub async fn claim_request(
        &self,
        account_id: &str,
        link: &AgentLink,
        client_instance_id: Uuid,
        request_id: &str,
    ) -> Result<ClaimedRequest, ArtifactError> {
        let credential = self.account_credential(account_id)?;
        let claim: ClaimedRequest = self
            .catalog_request_with_credential(
                Method::POST,
                &request_path(request_id, "claim")?,
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
        if claim.request_id != request_id
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

    /// Extends the claim's lease and returns its new expiry.
    pub async fn renew_request(
        &self,
        account_id: &str,
        artifact_id: &str,
        claim: &ClaimedRequest,
    ) -> Result<String, ArtifactError> {
        let credential = self.account_credential(account_id)?;
        let renewed: RenewedLease = self
            .catalog_request_with_credential(
                Method::POST,
                &request_path(&claim.request_id, "renew")?,
                &[],
                Some(json!({
                    "artifact_id": artifact_id,
                    "claim_id": claim.claim_id,
                    "fencing_token": claim.fencing_token,
                })),
                MAX_RESPONSE_BYTES,
                &credential,
            )
            .await?;
        if renewed.lease_expires_at.is_empty() || renewed.lease_expires_at.len() > MAX_LEASE_BYTES {
            return Err(ArtifactError::InvalidResponse);
        }
        Ok(renewed.lease_expires_at)
    }

    pub async fn release_request(
        &self,
        account_id: &str,
        artifact_id: &str,
        claim: &ClaimedRequest,
        outcome: ReleaseOutcome,
    ) -> Result<(), ArtifactError> {
        let credential = self.account_credential(account_id)?;
        self.catalog_request_with_credential::<Value>(
            Method::POST,
            &request_path(&claim.request_id, "release")?,
            &[],
            Some(json!({
                "artifact_id": artifact_id,
                "claim_id": claim.claim_id,
                "fencing_token": claim.fencing_token,
                "outcome": outcome,
            })),
            MAX_RESPONSE_BYTES,
            &credential,
        )
        .await
        .map(|_| ())
    }
}
