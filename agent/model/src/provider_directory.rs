use std::{
    collections::{BTreeMap, HashSet},
    path::Path,
    time::Duration,
};

use reqwest::{Url, header::HeaderValue};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::{
    availability::{AccountDirectoryAdapter, DiscoveryIdentity},
    credential_fingerprint, credentials,
};
use crate::auth::{
    device_subscriptions::{DeviceSubscription, SubscriptionAuth},
    google::GoogleAuth,
};
use tickloop_agent_base::network::{HttpPolicy, build_http_client, is_loopback};
use tickloop_agent_protocol::{
    AccountModelDirectory, DirectorySemantics, ModelAuthSource, ModelAvailability, ModelConfig,
};

pub(super) struct ProviderDirectoryAdapter<'a> {
    pub config: &'a ModelConfig,
    pub root: Option<&'a Path>,
}

struct Access {
    token: HeaderValue,
    identity: String,
    project: Option<String>,
}

impl ProviderDirectoryAdapter<'_> {
    fn endpoint(&self) -> Result<Url, String> {
        let mut url =
            Url::parse(&self.config.base_url).map_err(|_| "Invalid provider directory endpoint")?;
        let valid = match self.config.provider_id.as_str() {
            "anthropic-api" => self.config.base_url == "https://api.anthropic.com",
            "kimi-cn-api" => self.config.base_url == "https://api.moonshot.cn/v1",
            "kimi-global-api" => self.config.base_url == "https://api.moonshot.ai/v1",
            "kimi-code-oauth" => self.config.base_url == "https://api.kimi.com/coding/v1",
            "google-gemini-api" | "google-gemini-oauth" => {
                self.config.base_url == "https://generativelanguage.googleapis.com/v1beta"
            }
            "xai-api" => self.config.base_url == "https://api.x.ai/v1",
            _ => false,
        };
        if !(valid || cfg!(debug_assertions) && url.scheme() == "http" && is_loopback(&url)) {
            return Err("Provider directory requires its registered resource origin".into());
        }
        if !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err("Unsafe provider directory endpoint".into());
        }
        let suffix = if self.config.provider_id == "anthropic-api" {
            "/v1/models"
        } else {
            "/models"
        };
        url.set_path(&format!("{}{suffix}", url.path().trim_end_matches('/')));
        Ok(url)
    }

    async fn access(&self, cancel: &CancellationToken) -> Result<Access, String> {
        let (token, identity, project) = match self.config.auth_source {
            ModelAuthSource::GoogleOAuth => {
                let access = GoogleAuth::new(self.root.ok_or("Agent state directory is required")?)
                    .resolve(self.config, cancel, None)
                    .await
                    .map_err(|error| error.to_string())?;
                (access.token, access.identity, Some(access.project))
            }
            ModelAuthSource::KimiOAuth => {
                let access = SubscriptionAuth::new(
                    self.root.ok_or("Agent state directory is required")?,
                    DeviceSubscription::Kimi,
                )
                .resolve(cancel, None)
                .await
                .map_err(|error| error.to_string())?;
                (access.token, access.identity.to_string(), None)
            }
            ModelAuthSource::ApiKeyEnv => {
                let saved = self
                    .root
                    .map(|root| credentials::load_api_key(root, &self.config.api_key_env))
                    .transpose()
                    .map_err(|_| "Provider API key could not be read safely")?
                    .flatten();
                let token = saved
                    .or_else(|| std::env::var(&self.config.api_key_env).ok())
                    .filter(|token| !token.is_empty())
                    .ok_or("Provider API key is required")?;
                let identity = credential_fingerprint(token.as_bytes());
                (token, identity, None)
            }
            _ => return Err("Provider directory authentication is unsupported".into()),
        };
        let mut token =
            HeaderValue::from_str(&token).map_err(|_| "Invalid provider directory credential")?;
        token.set_sensitive(true);
        Ok(Access {
            token,
            identity,
            project,
        })
    }
}

#[async_trait::async_trait]
impl AccountDirectoryAdapter for ProviderDirectoryAdapter<'_> {
    async fn identity(&self, cancel: &CancellationToken) -> Result<DiscoveryIdentity, String> {
        let endpoint = self.endpoint()?;
        let access = self.access(cancel).await?;
        Ok(DiscoveryIdentity {
            provider: self.config.provider_id.clone(),
            origin: endpoint.origin().ascii_serialization(),
            resource: credential_fingerprint(endpoint.as_str().as_bytes()),
            account: access.identity,
        })
    }

    async fn fetch(&self, cancel: &CancellationToken) -> Result<AccountModelDirectory, String> {
        let mut endpoint = self.endpoint()?;
        let access = self.access(cancel).await?;
        let google = self.config.provider_id.starts_with("google-gemini");
        let anthropic = self.config.provider_id == "anthropic-api";
        let mut policy = HttpPolicy::new(reqwest::redirect::Policy::none());
        policy.timeout = Some(Duration::from_secs(8));
        let (client, _) = build_http_client(&endpoint, policy)
            .map_err(|_| "Provider discovery client unavailable")?;
        let mut models = BTreeMap::new();
        let mut cursors = HashSet::new();
        for _ in 0..100 {
            let mut request = client.get(endpoint.clone());
            if google && self.config.auth_source == ModelAuthSource::ApiKeyEnv {
                request = request.header("x-goog-api-key", access.token.clone());
            } else if anthropic {
                request = request
                    .header("x-api-key", access.token.clone())
                    .header("anthropic-version", "2023-06-01");
            } else {
                request = request.bearer_auth(
                    access
                        .token
                        .to_str()
                        .map_err(|_| "Invalid discovery credential")?,
                );
            }
            if let Some(project) = &access.project {
                request = request.header("x-goog-user-project", project);
            }
            let value = tokio::select! {
                biased;
                () = cancel.cancelled() => return Err("Model discovery cancelled".into()),
                value = async {
                    let mut response = request.send().await.map_err(|_| "Provider model discovery request failed")?;
                    let status = response.status().as_u16();
                    if !(200..300).contains(&status) { return Err(format!("Model discovery failed (HTTP {status})")); }
                    let mut bytes = Vec::new();
                    while let Some(chunk) = response.chunk().await.map_err(|_| "Provider discovery response interrupted")? {
                        if bytes.len().saturating_add(chunk.len()) > 1024 * 1024 { return Err("Provider directory exceeded size limit".into()); }
                        bytes.extend_from_slice(&chunk);
                    }
                    serde_json::from_slice::<Value>(&bytes).map_err(|_| "Invalid provider directory JSON".to_string())
                } => value?,
            };
            let entries = value[if google { "models" } else { "data" }]
                .as_array()
                .ok_or("Invalid provider model directory")?;
            for entry in entries {
                if google
                    && !entry["supportedGenerationMethods"]
                        .as_array()
                        .is_some_and(|methods| {
                            methods.iter().any(|method| method == "generateContent")
                        })
                {
                    continue;
                }
                let id = entry[if google { "name" } else { "id" }]
                    .as_str()
                    .filter(|id| !id.is_empty() && id.len() <= 512)
                    .ok_or("Invalid provider model ID")?;
                let id = if google {
                    id.strip_prefix("models/")
                        .ok_or("Invalid Google model name")?
                } else {
                    id
                };
                if models
                    .insert(id.into(), ModelAvailability::DirectoryListed)
                    .is_some()
                {
                    return Err("Provider directory repeated a model".into());
                }
                if models.len() > 10000 {
                    return Err("Provider directory exceeded model limit".into());
                }
            }
            let next = if google {
                value["nextPageToken"]
                    .as_str()
                    .filter(|value| !value.is_empty())
                    .map(|value| ("pageToken", value))
            } else if anthropic && value["has_more"] == true {
                Some((
                    "after_id",
                    value["last_id"]
                        .as_str()
                        .ok_or("Anthropic directory missing pagination cursor")?,
                ))
            } else {
                None
            };
            if let Some((name, cursor)) = next {
                if cursor.len() > 4096 || !cursors.insert(cursor.to_owned()) {
                    return Err("Provider directory repeated a cursor".into());
                }
                endpoint.set_query(None);
                endpoint.query_pairs_mut().append_pair(name, cursor);
            } else {
                return Ok(AccountModelDirectory {
                    models,
                    semantics: DirectorySemantics::Directory,
                    complete: value["has_more"] != true,
                });
            }
        }
        Err("Provider directory pagination exceeded limit".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        auth::provider_test_support::{issuer, root},
        availability::AccountModelDiscovery,
        cards::ModelCatalog,
    };
    use serde_json::json;

    #[tokio::test]
    async fn google_directory_paginates_filters_and_isolated_cache_tracks_key_rotation() {
        let root = root();
        let (origin, mut captured) = issuer(vec![
            (200, json!({"models":[{"name":"models/gemini-3.8-flash","supportedGenerationMethods":["generateContent"]},{"name":"models/embedding","supportedGenerationMethods":["embedContent"]}],"nextPageToken":"cursor"})),
            (200, json!({"models":[{"name":"models/gemini-3.5-flash-lite","supportedGenerationMethods":["generateContent"]}]})),
            (200, json!({"models":[{"name":"models/new-account-model","supportedGenerationMethods":["generateContent"]}]})),
        ]).await;
        let mut config = ModelCatalog::defaults()
            .get("google-gemini-api", "gemini-3.8-flash")
            .unwrap();
        config.base_url = origin;
        credentials::save_api_key(root.path(), &config.api_key_env, "fixture-first").unwrap();
        let cache = AccountModelDiscovery::default();
        let cancel = CancellationToken::new();
        let first = cache
            .discover(&config, Some(root.path()), false, &cancel)
            .await
            .unwrap();
        assert_eq!(first.models.len(), 2);
        assert_eq!(
            first.availability("gemini-3.8-flash"),
            ModelAvailability::DirectoryListed
        );
        assert!(matches!(
            first.availability("absent-model"),
            ModelAvailability::Unknown { .. }
        ));
        assert!(
            captured
                .recv()
                .await
                .unwrap()
                .to_lowercase()
                .contains("x-goog-api-key: fixture-first")
        );
        assert!(
            captured
                .recv()
                .await
                .unwrap()
                .starts_with("GET /models?pageToken=cursor ")
        );
        let cached = cache
            .discover(&config, Some(root.path()), false, &cancel)
            .await
            .unwrap();
        assert_eq!(cached.models, first.models);
        credentials::save_api_key(root.path(), &config.api_key_env, "fixture-second").unwrap();
        let next = cache
            .discover(&config, Some(root.path()), false, &cancel)
            .await
            .unwrap();
        assert_eq!(next.models.len(), 1);
        assert!(matches!(
            next.availability("gemini-3.8-flash"),
            ModelAvailability::Unknown { .. }
        ));
        assert!(
            captured
                .recv()
                .await
                .unwrap()
                .to_lowercase()
                .contains("x-goog-api-key: fixture-second")
        );
    }

    #[tokio::test]
    async fn anthropic_directory_cursor_failure_preserves_login_and_reports_unknown() {
        let root = root();
        let (origin, mut captured) = issuer(vec![
            (200, json!({"data":[{"id":"claude-sonnet-5-5"}], "has_more":true, "last_id":"claude-sonnet-5-5"})),
            (200, json!({"data":[{"id":"claude-opus-5-5"}], "has_more":false})),
            (403, json!({"error":{"message":"fixture-secret-not-for-logs"}})),
        ]).await;
        let mut config = ModelCatalog::defaults()
            .get("anthropic-api", "claude-sonnet-5-5")
            .unwrap();
        config.base_url = origin;
        credentials::save_api_key(root.path(), &config.api_key_env, "fixture-key").unwrap();
        let cache = AccountModelDiscovery::default();
        let cancel = CancellationToken::new();
        let first = cache
            .discover(&config, Some(root.path()), false, &cancel)
            .await
            .unwrap();
        assert_eq!(first.models.len(), 2);
        assert!(
            captured
                .recv()
                .await
                .unwrap()
                .starts_with("GET /v1/models ")
        );
        assert!(
            captured
                .recv()
                .await
                .unwrap()
                .starts_with("GET /v1/models?after_id=claude-sonnet-5-5 ")
        );
        let error = cache
            .discover(&config, Some(root.path()), true, &cancel)
            .await
            .unwrap_err();
        assert!(error.contains("403"));
        assert!(!error.contains("fixture-secret"));
        assert_eq!(
            credentials::load_api_key(root.path(), &config.api_key_env)
                .unwrap()
                .as_deref(),
            Some("fixture-key")
        );
    }

    #[tokio::test]
    async fn repeated_pagination_and_redirect_fail_closed() {
        for responses in [
            vec![
                (200, json!({"models":[],"nextPageToken":"repeat"})),
                (200, json!({"models":[],"nextPageToken":"repeat"})),
            ],
            vec![(302, json!({"models":[]}))],
        ] {
            let root = root();
            let (origin, _captured) = issuer(responses).await;
            let mut config = ModelCatalog::defaults()
                .get("google-gemini-api", "gemini-3.8-flash")
                .unwrap();
            config.base_url = origin;
            credentials::save_api_key(root.path(), &config.api_key_env, "fixture-key").unwrap();
            assert!(
                AccountModelDiscovery::default()
                    .discover(&config, Some(root.path()), true, &CancellationToken::new())
                    .await
                    .is_err()
            );
        }
    }
}
