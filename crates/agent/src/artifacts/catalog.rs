use reqwest::{Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::{ArtifactClient, ArtifactError, MAX_RESPONSE_BYTES, bearer};
const MAX_CATALOG_RESPONSE_BYTES: usize = 256 * 1024;

#[cfg(test)]
mod identity_tests {
    use super::*;

    #[test]
    fn artifact_tick_uses_the_canonical_subscription_identity() {
        let tick = ArtifactTick {
            artifact_id: "doc".into(),
            origin: "https://docs.example/".into(),
            title: "Document".into(),
            format: "markdown".into(),
            url: "https://docs.example/a/doc".into(),
            access_role: "owner".into(),
            added_at_ms: 1,
        };
        assert_eq!(
            tick.instance_id(),
            tick_connect::instance::InstanceRef::artifact(&tick.origin, &tick.artifact_id)
                .unwrap()
                .as_str()
        );
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ArtifactTick {
    pub artifact_id: String,
    pub origin: String,
    pub title: String,
    pub format: String,
    pub url: String,
    pub access_role: String,
    pub added_at_ms: u64,
}
impl ArtifactTick {
    pub fn instance_id(&self) -> String {
        tick_connect::instance::artifact_instance_id(&self.origin, &self.artifact_id)
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ArtifactMetadata {
    #[serde(default)]
    pub human_body_sequence: Option<u64>,
    #[serde(default)]
    pub human_body_updated_at: Option<String>,
    #[serde(default)]
    pub human_body_change: Option<String>,
    #[serde(default)]
    pub human_body_version_id: Option<String>,
    #[serde(default)]
    pub human_sequence: Option<u64>,
    #[serde(default)]
    pub human_updated_at: Option<String>,
    #[serde(default)]
    pub human_change: Option<String>,
    #[serde(default)]
    pub human_version_id: Option<String>,
    pub id: String,
    pub title: String,
    pub format: String,
    pub access_role: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub metadata_revision: u64,
    #[serde(default)]
    pub association: Option<Value>,
}
impl ArtifactMetadata {
    pub fn tick(&self, origin: &str, added_at_ms: u64) -> ArtifactTick {
        ArtifactTick {
            artifact_id: self.id.clone(),
            origin: origin.trim_end_matches('/').into(),
            title: self.title.clone(),
            format: self.format.clone(),
            url: self.url.clone(),
            access_role: self.access_role.clone(),
            added_at_ms,
        }
    }
}
#[derive(Clone, Debug, Deserialize)]
pub struct ArtifactPage {
    pub artifacts: Vec<ArtifactMetadata>,
    pub next_cursor: Option<String>,
}
#[derive(Clone, Debug, Deserialize)]
pub struct ArtifactOperation {
    pub operation_id: String,
    pub status: String,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<Value>,
    #[serde(default)]
    pub recovered: bool,
}
impl ArtifactClient {
    async fn catalog_request<T: for<'de> Deserialize<'de>>(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<Value>,
        limit: usize,
    ) -> Result<T, ArtifactError> {
        let credential = self
            .credential_snapshot()?
            .ok_or(ArtifactError::AccessDenied)?;
        let mut request = self
            .request(method, path)?
            .header(
                reqwest::header::AUTHORIZATION,
                bearer(&credential.access_token)?,
            )
            .query(query);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .map_err(|_| ArtifactError::Connection)?;
        if response.status() == StatusCode::UNAUTHORIZED {
            self.invalidate_credential(&credential.access_token).await?;
            return Err(ArtifactError::AccessDenied);
        }
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let envelope: Value = self
                .read_json_with_limit(response, MAX_RESPONSE_BYTES)
                .await?;
            let code = envelope
                .pointer("/error/code")
                .and_then(Value::as_str)
                .unwrap_or("invalid_response")
                .to_owned();
            return Err(ArtifactError::Service { status, code });
        }
        self.read_json_with_limit(response, limit).await
    }
    pub async fn list(
        &self,
        ownership: &str,
        q: &str,
        cursor: Option<&str>,
    ) -> Result<ArtifactPage, ArtifactError> {
        let mut query = vec![
            ("ownership", ownership),
            ("q", q),
            ("limit", "20"),
            ("sort", "updated"),
            ("direction", "desc"),
        ];
        if let Some(cursor) = cursor {
            query.push(("cursor", cursor));
        }
        self.catalog_request(
            Method::GET,
            "/api/v1/artifacts",
            &query,
            None,
            MAX_CATALOG_RESPONSE_BYTES,
        )
        .await
    }
    pub async fn pinned(&self) -> Result<ArtifactPage, ArtifactError> {
        self.catalog_request(
            Method::GET,
            "/api/v1/artifacts/pinned",
            &[],
            None,
            MAX_CATALOG_RESPONSE_BYTES,
        )
        .await
    }
    fn artifact_path(&self, id: &str) -> Result<String, ArtifactError> {
        if id.is_empty() || id.contains(['/', '?', '#', '%']) || id.chars().any(char::is_control) {
            return Err(ArtifactError::InvalidResponse);
        }
        Ok(format!("/api/v1/artifacts/{id}"))
    }
    pub async fn read(&self, id: &str) -> Result<ArtifactMetadata, ArtifactError> {
        let mut metadata: ArtifactMetadata = self
            .catalog_request(
                Method::GET,
                &self.artifact_path(id)?,
                &[],
                None,
                MAX_RESPONSE_BYTES,
            )
            .await?;
        if metadata.id != id {
            return Err(ArtifactError::InvalidResponse);
        }
        if metadata.url.is_empty() {
            metadata.url = format!("{}/a/{}", self.origin(), metadata.id);
        }
        Ok(metadata)
    }
    pub async fn pin(&self, id: &str, pinned: bool) -> Result<(), ArtifactError> {
        let operation_id = Uuid::new_v4().to_string();
        let path = format!("{}/pin", self.artifact_path(id)?);
        let outcome = self
            .catalog_request::<Value>(
                if pinned { Method::PUT } else { Method::DELETE },
                &path,
                &[],
                Some(json!({"operation_id": operation_id})),
                MAX_RESPONSE_BYTES,
            )
            .await;
        match outcome {
            Ok(_) => Ok(()),
            Err(ArtifactError::Connection | ArtifactError::InvalidResponse)
            | Err(ArtifactError::Service {
                status: 500..=599, ..
            }) => {
                let query = format!("{}/operations/{operation_id}", self.artifact_path(id)?);
                let outcome = self
                    .catalog_request::<ArtifactOperation>(
                        Method::GET,
                        &query,
                        &[],
                        None,
                        MAX_RESPONSE_BYTES,
                    )
                    .await;
                match outcome {
                    Ok(result) if result.status == "committed" => Ok(()),
                    Ok(result) => Err(ArtifactError::OutcomeUnknown {
                        operation_id,
                        outcome: result.status,
                    }),
                    Err(error) => Err(ArtifactError::OutcomeUnknown {
                        operation_id,
                        outcome: error.to_string(),
                    }),
                }
            }
            Err(error) => Err(error),
        }
    }
    async fn check_operation(
        &self,
        base: &str,
        operation_id: String,
    ) -> Result<ArtifactOperation, ArtifactError> {
        let query = format!("{base}/operations/{operation_id}");
        match self
            .catalog_request::<ArtifactOperation>(
                Method::GET,
                &query,
                &[],
                None,
                MAX_RESPONSE_BYTES,
            )
            .await
        {
            Ok(mut result) if result.operation_id == operation_id => {
                result.recovered = true;
                Ok(result)
            }
            Ok(_) => Err(ArtifactError::OutcomeUnknown {
                operation_id,
                outcome: "operation receipt did not match".into(),
            }),
            Err(error) => Err(ArtifactError::OutcomeUnknown {
                operation_id,
                outcome: error.to_string(),
            }),
        }
    }
    async fn modify(
        &self,
        id: &str,
        method: Method,
        action: &str,
        mut body: Value,
    ) -> Result<ArtifactOperation, ArtifactError> {
        let operation_id = Uuid::new_v4().to_string();
        body["operation_id"] = json!(operation_id);
        let base = self.artifact_path(id)?;
        let path = format!("{base}{action}");
        let outcome = self
            .catalog_request::<ArtifactOperation>(
                method,
                &path,
                &[],
                Some(body),
                MAX_RESPONSE_BYTES,
            )
            .await;
        match outcome {
            Ok(operation) if operation.status == "unknown" => {
                self.check_operation(&base, operation_id).await
            }
            Err(ArtifactError::Connection | ArtifactError::InvalidResponse)
            | Err(ArtifactError::Service {
                status: 500..=599, ..
            }) => self.check_operation(&base, operation_id).await,
            other => other,
        }
    }
    pub async fn rename(
        &self,
        id: &str,
        title: &str,
        expected_metadata_revision: u64,
    ) -> Result<ArtifactOperation, ArtifactError> {
        self.modify(
            id,
            Method::POST,
            "/metadata",
            json!({"title": title, "expected_metadata_revision": expected_metadata_revision}),
        )
        .await
    }
    pub async fn delete(&self, id: &str) -> Result<ArtifactOperation, ArtifactError> {
        self.modify(id, Method::DELETE, "", json!({})).await
    }
}
