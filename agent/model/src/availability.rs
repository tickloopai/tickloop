//! Account directory evidence stays separate from Model Cards and current recommendations.
use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use reqwest::{
    Url,
    header::{AUTHORIZATION, HeaderValue},
    redirect::Policy,
};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use super::{credential_fingerprint, credentials};
use crate::{ModelConfigExt, auth::CodexAuth};
use tickloop_agent_base::network::{HttpPolicy, build_http_client, is_loopback};
use tickloop_agent_protocol::{
    AccountModelDirectory, DirectorySemantics, ModelAvailability, ModelConfig, ModelDiscovery,
};

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(8);
const DISCOVERY_TIMED_OUT: &str = "Model discovery timed out";
const MAX_DIRECTORY_BYTES: usize = 1024 * 1024;
const MAX_DIRECTORY_MODELS: usize = 10000;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct DiscoveryIdentity {
    pub provider: String,
    pub origin: String,
    pub resource: String,
    pub account: String,
}

#[derive(Clone)]
struct CachedDirectory {
    directory: Result<AccountModelDirectory, String>,
}

#[derive(Default)]
struct CacheState {
    generations: HashMap<String, u64>,
    identity_generations: HashMap<DiscoveryIdentity, u64>,
    entries: HashMap<DiscoveryIdentity, CachedDirectory>,
}

#[derive(Clone, Default)]
pub struct AccountModelDiscovery {
    state: Arc<Mutex<CacheState>>,
}

impl AccountModelDiscovery {
    pub fn invalidate(&self, provider: &str) {
        let mut state = self.state.lock().unwrap();
        *state.generations.entry(provider.to_owned()).or_default() += 1;
        state
            .entries
            .retain(|identity, _| identity.provider != provider);
        state
            .identity_generations
            .retain(|identity, _| identity.provider != provider);
    }

    pub async fn discover(
        &self,
        config: &ModelConfig,
        root: Option<&Path>,
        force: bool,
        cancel: &CancellationToken,
    ) -> Result<AccountModelDirectory, String> {
        let adapter = directory_adapter(config, root);
        self.discover_with(adapter.as_ref(), force, cancel).await
    }

    pub async fn cached_directory(
        &self,
        config: &ModelConfig,
        root: Option<&Path>,
        cancel: &CancellationToken,
    ) -> Option<Result<AccountModelDirectory, String>> {
        let adapter = directory_adapter(config, root);
        let initial_generations = self.state.lock().unwrap().generations.clone();
        let identity = tokio::select! {
            biased;
            () = cancel.cancelled() => return Some(Err("Model discovery cancelled".into())),
            result = tokio::time::timeout(DISCOVERY_TIMEOUT, adapter.identity(cancel)) => match result {
                Ok(Ok(identity)) => identity,
                Ok(Err(error)) => return Some(Err(error)),
                Err(_) => return Some(Err(DISCOVERY_TIMED_OUT.into())),
            },
        };
        let state = self.state.lock().unwrap();
        if state
            .generations
            .get(&identity.provider)
            .copied()
            .unwrap_or(0)
            != initial_generations
                .get(&identity.provider)
                .copied()
                .unwrap_or(0)
        {
            return Some(Err(
                "Model discovery superseded by an authentication change".into(),
            ));
        }
        state
            .entries
            .get(&identity)
            .map(|cached| cached.directory.clone())
    }

    pub async fn discover_with(
        &self,
        adapter: &dyn AccountDirectoryAdapter,
        force: bool,
        cancel: &CancellationToken,
    ) -> Result<AccountModelDirectory, String> {
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err("Model discovery cancelled".into()),
            result = self.discover_inner(adapter, force, cancel) => result,
        }
    }

    async fn discover_inner(
        &self,
        adapter: &dyn AccountDirectoryAdapter,
        force: bool,
        cancel: &CancellationToken,
    ) -> Result<AccountModelDirectory, String> {
        let deadline = tokio::time::Instant::now() + DISCOVERY_TIMEOUT;
        let initial_generations = self.state.lock().unwrap().generations.clone();
        let identity = tokio::time::timeout_at(deadline, adapter.identity(cancel))
            .await
            .map_err(|_| DISCOVERY_TIMED_OUT.to_owned())??;
        if identity.provider.is_empty()
            || identity.account.is_empty()
            || identity.resource.is_empty()
            || Url::parse(&identity.origin).is_err()
        {
            return Err("Invalid account directory identity".into());
        }
        let generation = {
            let mut state = self.state.lock().unwrap();
            if state
                .generations
                .get(&identity.provider)
                .copied()
                .unwrap_or(0)
                != initial_generations
                    .get(&identity.provider)
                    .copied()
                    .unwrap_or(0)
            {
                return Err("Model discovery superseded by an authentication change".into());
            }
            if force {
                state.entries.remove(&identity);
                *state
                    .identity_generations
                    .entry(identity.clone())
                    .or_default() += 1;
            }
            if let Some(cached) = state.entries.get(&identity) {
                return cached.directory.clone();
            }
            *state
                .identity_generations
                .entry(identity.clone())
                .or_default()
        };
        let (directory, timed_out) =
            match tokio::time::timeout_at(deadline, adapter.fetch(cancel)).await {
                Ok(directory) => (directory, false),
                Err(_) => (Err(DISCOVERY_TIMED_OUT.to_owned()), true),
            };
        let current = if timed_out {
            identity.clone()
        } else {
            tokio::time::timeout_at(deadline, adapter.identity(cancel))
                .await
                .map_err(|_| DISCOVERY_TIMED_OUT.to_owned())??
        };
        let mut state = self.state.lock().unwrap();
        if cancel.is_cancelled()
            || current != identity
            || state
                .generations
                .get(&identity.provider)
                .copied()
                .unwrap_or(0)
                != initial_generations
                    .get(&identity.provider)
                    .copied()
                    .unwrap_or(0)
            || state
                .identity_generations
                .get(&identity)
                .copied()
                .unwrap_or(0)
                != generation
        {
            return Err("Model discovery superseded by an authentication change".into());
        }
        state.entries.insert(
            identity,
            CachedDirectory {
                directory: directory.clone(),
            },
        );
        directory
    }
}

/// Implementations resolve one real connection's credentials and return only sanitized metadata.
/// `account` is a stable opaque identity, never a bearer token; normal token refresh preserves it.
#[async_trait::async_trait]
pub trait AccountDirectoryAdapter: Send + Sync {
    async fn identity(&self, cancel: &CancellationToken) -> Result<DiscoveryIdentity, String>;
    async fn fetch(&self, cancel: &CancellationToken) -> Result<AccountModelDirectory, String>;
}

fn directory_adapter<'a>(
    config: &'a ModelConfig,
    root: Option<&'a Path>,
) -> Box<dyn AccountDirectoryAdapter + 'a> {
    match config
        .connection()
        .map_or(ModelDiscovery::Unsupported, |connection| {
            connection.discovery
        }) {
        ModelDiscovery::ProviderApi => {
            Box::new(super::provider_directory::ProviderDirectoryAdapter { config, root })
        }
        ModelDiscovery::GithubCopilot => {
            Box::new(super::copilot::CopilotDirectoryAdapter { config, root })
        }
        _ => Box::new(OpenAiDirectoryAdapter { config, root }),
    }
}

struct OpenAiDirectoryAdapter<'a> {
    config: &'a ModelConfig,
    root: Option<&'a Path>,
}

#[async_trait::async_trait]
impl AccountDirectoryAdapter for OpenAiDirectoryAdapter<'_> {
    async fn identity(&self, cancel: &CancellationToken) -> Result<DiscoveryIdentity, String> {
        let strategy = self
            .config
            .connection()
            .map_or(ModelDiscovery::Unsupported, |connection| {
                connection.discovery
            });
        let endpoint = discovery_endpoint(self.config, strategy)?;
        let auth = resolve_auth(self.config, self.root, strategy, cancel).await?;
        Ok(DiscoveryIdentity {
            provider: self.config.provider_id.clone(),
            origin: endpoint.origin().ascii_serialization(),
            resource: credential_fingerprint(endpoint.as_str().as_bytes()),
            account: auth.identity,
        })
    }

    async fn fetch(&self, cancel: &CancellationToken) -> Result<AccountModelDirectory, String> {
        let strategy = self
            .config
            .connection()
            .map_or(ModelDiscovery::Unsupported, |connection| {
                connection.discovery
            });
        let endpoint = discovery_endpoint(self.config, strategy)?;
        let auth = resolve_auth(self.config, self.root, strategy, cancel).await?;
        fetch_directory(&endpoint, strategy, &auth).await
    }
}

struct DiscoveryAuth {
    bearer: HeaderValue,
    account_id: Option<String>,
    identity: String,
}

async fn resolve_auth(
    config: &ModelConfig,
    root: Option<&Path>,
    strategy: ModelDiscovery,
    cancel: &CancellationToken,
) -> Result<DiscoveryAuth, String> {
    if cancel.is_cancelled() {
        return Err("Model discovery cancelled".into());
    }
    let (token, account_id, identity) = match strategy {
        ModelDiscovery::Codex => {
            let credentials = CodexAuth::new(root.ok_or("Agent state directory is required")?)
                .resolve_credentials(cancel)
                .await
                .map_err(|_| "Codex credentials could not be resolved".to_owned())?;
            let identity = credential_fingerprint(credentials.account_id.as_bytes());
            (
                credentials.access_token,
                Some(credentials.account_id),
                identity,
            )
        }
        ModelDiscovery::OpenAiApi => {
            let saved = root
                .map(|root| credentials::load_api_key(root, &config.api_key_env))
                .transpose()
                .map_err(|_| "API key could not be read safely")?
                .flatten();
            let key = saved
                .or_else(|| std::env::var(&config.api_key_env).ok())
                .filter(|value| !value.trim().is_empty())
                .ok_or("API key is required for model discovery")?;
            let identity = credential_fingerprint(key.as_bytes());
            (key, None, identity)
        }
        ModelDiscovery::Unsupported
        | ModelDiscovery::GithubCopilot
        | ModelDiscovery::ProviderApi => {
            return Err("Model discovery is not provided".into());
        }
    };
    let mut bearer = HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| "Invalid discovery credentials")?;
    bearer.set_sensitive(true);
    Ok(DiscoveryAuth {
        bearer,
        account_id,
        identity,
    })
}

fn discovery_endpoint(config: &ModelConfig, strategy: ModelDiscovery) -> Result<Url, String> {
    #[cfg(any(test, feature = "test-support"))]
    let test_endpoint = if strategy == ModelDiscovery::Codex {
        std::env::var("TICKLOOP_TEST_CODEX_RESPONSES").ok()
    } else {
        None
    };
    #[cfg(any(test, feature = "test-support"))]
    let base_url = test_endpoint.as_deref().unwrap_or(&config.base_url);
    #[cfg(not(any(test, feature = "test-support")))]
    let base_url = config.base_url.as_str();
    let mut endpoint = Url::parse(base_url).map_err(|_| "Invalid model discovery endpoint")?;
    if !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
        || !(endpoint.scheme() == "https" || endpoint.scheme() == "http" && is_loopback(&endpoint))
    {
        return Err("Unsafe model discovery endpoint".into());
    }
    match strategy {
        ModelDiscovery::OpenAiApi => {
            if !is_loopback(&endpoint) && endpoint.host_str() != Some("api.openai.com") {
                return Err("OpenAI API discovery requires the OpenAI API origin".into());
            }
            endpoint.set_path(&format!("{}/models", endpoint.path().trim_end_matches('/')));
        }
        ModelDiscovery::Codex => {
            if !is_loopback(&endpoint) && base_url != super::CODEX_RESPONSES_URL {
                return Err("Codex discovery requires the subscription resource origin".into());
            }
            endpoint.set_path(&format!(
                "{}/models",
                endpoint
                    .path()
                    .trim_end_matches('/')
                    .trim_end_matches("/responses")
            ));
            endpoint
                .query_pairs_mut()
                .append_pair("client_version", env!("CARGO_PKG_VERSION"));
        }
        ModelDiscovery::Unsupported
        | ModelDiscovery::GithubCopilot
        | ModelDiscovery::ProviderApi => {
            return Err("Model discovery is not provided".into());
        }
    }
    Ok(endpoint)
}

async fn fetch_directory(
    endpoint: &Url,
    strategy: ModelDiscovery,
    auth: &DiscoveryAuth,
) -> Result<AccountModelDirectory, String> {
    let mut policy = HttpPolicy::new(Policy::none());
    policy.timeout = Some(DISCOVERY_TIMEOUT);
    let (client, _) = build_http_client(endpoint, policy)
        .map_err(|_| "Could not create discovery HTTP client")?;
    let mut request = client
        .get(endpoint.clone())
        .header(AUTHORIZATION, auth.bearer.clone());
    if let Some(account) = &auth.account_id {
        request = request
            .header("chatgpt-account-id", account)
            .header("originator", "tickloop")
            .header(
                reqwest::header::USER_AGENT,
                concat!("tickloop-agent/", env!("CARGO_PKG_VERSION")),
            );
    }
    let mut response = request
        .send()
        .await
        .map_err(|_| "Model discovery connection failed")?;
    if !response.status().is_success() {
        return Err(format!(
            "Model discovery failed (HTTP {}); login is unchanged",
            response.status().as_u16()
        ));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "Model discovery response interrupted")?
    {
        if body.len().saturating_add(chunk.len()) > MAX_DIRECTORY_BYTES {
            return Err("Model directory exceeds the response size limit".into());
        }
        body.extend_from_slice(&chunk);
    }
    parse_directory(strategy, &body)
}

#[derive(Deserialize)]
struct ApiModels {
    data: Vec<ApiModel>,
    #[serde(default)]
    has_more: bool,
}
#[derive(Deserialize)]
struct ApiModel {
    id: String,
}
#[derive(Deserialize)]
struct CodexModels {
    models: Vec<CodexModel>,
    #[serde(default)]
    has_more: bool,
}
#[derive(Deserialize)]
struct CodexModel {
    slug: String,
}

fn parse_directory(strategy: ModelDiscovery, body: &[u8]) -> Result<AccountModelDirectory, String> {
    let (ids, complete): (Vec<String>, bool) = match strategy {
        ModelDiscovery::OpenAiApi => {
            let response: ApiModels =
                serde_json::from_slice(body).map_err(|_| "Invalid OpenAI API model directory")?;
            (
                response.data.into_iter().map(|model| model.id).collect(),
                !response.has_more,
            )
        }
        ModelDiscovery::Codex => {
            let response: CodexModels =
                serde_json::from_slice(body).map_err(|_| "Invalid Codex model directory")?;
            (
                response
                    .models
                    .into_iter()
                    .map(|model| model.slug)
                    .collect(),
                !response.has_more,
            )
        }
        ModelDiscovery::Unsupported
        | ModelDiscovery::GithubCopilot
        | ModelDiscovery::ProviderApi => {
            return Err("Model discovery is not provided".into());
        }
    };
    if ids.len() > MAX_DIRECTORY_MODELS
        || ids.iter().any(|id| id.trim().is_empty() || id.len() > 512)
    {
        return Err("Invalid model directory entries".into());
    }
    let mut models = BTreeMap::new();
    for id in ids {
        if models
            .insert(id, ModelAvailability::DirectoryListed)
            .is_some()
        {
            return Err("Duplicate model directory entry".into());
        }
    }
    Ok(AccountModelDirectory {
        models,
        semantics: DirectorySemantics::Directory,
        complete,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::{Notify, Semaphore};

    struct ControlledAdapter {
        identity: Mutex<DiscoveryIdentity>,
        calls: AtomicUsize,
        started: Notify,
        release: Semaphore,
        directory: AccountModelDirectory,
        failure: Option<String>,
    }

    impl ControlledAdapter {
        fn new(account: &str, blocked: bool) -> Self {
            Self {
                identity: Mutex::new(DiscoveryIdentity {
                    provider: "test-provider".into(),
                    origin: "https://api.example.com".into(),
                    resource: "test-resource".into(),
                    account: account.into(),
                }),
                failure: None,
                calls: AtomicUsize::new(0),
                started: Notify::new(),
                release: Semaphore::new(if blocked { 0 } else { 10 }),
                directory: AccountModelDirectory {
                    models: BTreeMap::from([(account.into(), ModelAvailability::DirectoryListed)]),
                    semantics: DirectorySemantics::Directory,
                    complete: true,
                },
            }
        }
    }

    #[async_trait::async_trait]
    impl AccountDirectoryAdapter for ControlledAdapter {
        async fn identity(&self, _: &CancellationToken) -> Result<DiscoveryIdentity, String> {
            Ok(self.identity.lock().unwrap().clone())
        }
        async fn fetch(&self, _: &CancellationToken) -> Result<AccountModelDirectory, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.started.notify_one();
            self.release.acquire().await.unwrap().forget();
            self.failure.as_ref().map_or_else(
                || Ok(self.directory.clone()),
                |failure| Err(failure.clone()),
            )
        }
    }

    #[tokio::test]
    async fn session_cache_keeps_distinct_resources_under_the_same_provider_and_refreshes_only_one()
    {
        let cache = AccountModelDiscovery::default();
        let a = ControlledAdapter::new("same-account", false);
        let b = ControlledAdapter::new("same-account", false);
        a.identity.lock().unwrap().resource = "endpoint-a/models".into();
        b.identity.lock().unwrap().resource = "endpoint-b/models".into();
        let cancel = CancellationToken::new();
        for adapter in [&a, &b, &a, &b] {
            cache.discover_with(adapter, false, &cancel).await.unwrap();
        }
        assert_eq!(
            a.calls.load(Ordering::SeqCst),
            1,
            "Returning to A must reuse its session directory"
        );
        assert_eq!(
            b.calls.load(Ordering::SeqCst),
            1,
            "Returning to B must reuse its session directory"
        );
        cache.discover_with(&a, true, &cancel).await.unwrap();
        cache.discover_with(&b, false, &cancel).await.unwrap();
        assert_eq!(a.calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            b.calls.load(Ordering::SeqCst),
            1,
            "Refreshing A must retain B's evidence"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn session_cache_reuses_identity_until_invalidation_or_explicit_refresh() {
        let cache = AccountModelDiscovery::default();
        let adapter = ControlledAdapter::new("account-a", false);
        let cancel = CancellationToken::new();
        for _ in 0..2 {
            cache.discover_with(&adapter, false, &cancel).await.unwrap();
        }
        assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_secs(86400)).await;
        cache.discover_with(&adapter, false, &cancel).await.unwrap();
        assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
        cache.invalidate("test-provider");
        cache.discover_with(&adapter, false, &cancel).await.unwrap();
        cache.discover_with(&adapter, true, &cancel).await.unwrap();
        assert_eq!(adapter.calls.load(Ordering::SeqCst), 3);
        let renewed_token_same_account = ControlledAdapter::new("account-a", false);
        cache
            .discover_with(&renewed_token_same_account, false, &cancel)
            .await
            .unwrap();
        assert_eq!(renewed_token_same_account.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn failed_directory_check_is_reused_until_invalidation() {
        let cache = AccountModelDiscovery::default();
        let mut adapter = ControlledAdapter::new("account-a", false);
        adapter.failure = Some("Model discovery failed (HTTP 503)".into());
        for _ in 0..3 {
            assert!(
                cache
                    .discover_with(&adapter, false, &CancellationToken::new())
                    .await
                    .is_err()
            );
        }
        assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
        cache.invalidate("test-provider");
        assert!(
            cache
                .discover_with(&adapter, false, &CancellationToken::new())
                .await
                .is_err()
        );
        assert_eq!(adapter.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn directory_timeout_is_cached_without_repeating_network_requests() {
        let cache = AccountModelDiscovery::default();
        let adapter = ControlledAdapter::new("account-a", true);
        let cancel = CancellationToken::new();
        assert_eq!(
            cache
                .discover_with(&adapter, false, &cancel)
                .await
                .unwrap_err(),
            DISCOVERY_TIMED_OUT
        );
        assert_eq!(
            cache
                .discover_with(&adapter, false, &cancel)
                .await
                .unwrap_err(),
            DISCOVERY_TIMED_OUT
        );
        assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
        adapter.release.add_permits(1);
        cache.invalidate("test-provider");
        cache.discover_with(&adapter, false, &cancel).await.unwrap();
        assert_eq!(adapter.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn first_request_cannot_overwrite_a_new_identity_or_origin() {
        for change in ["account", "origin", "resource"] {
            let cache = AccountModelDiscovery::default();
            let older = Arc::new(ControlledAdapter::new("account-a", true));
            let pending_cache = cache.clone();
            let pending_adapter = older.clone();
            let pending = tokio::spawn(async move {
                pending_cache
                    .discover_with(pending_adapter.as_ref(), false, &CancellationToken::new())
                    .await
            });
            older.started.notified().await;
            let newer = ControlledAdapter::new(
                if change == "account" {
                    "account-b"
                } else {
                    "account-a"
                },
                false,
            );
            if change == "origin" {
                newer.identity.lock().unwrap().origin = "https://other.example.com".into();
            }
            if change == "resource" {
                newer.identity.lock().unwrap().resource = "other-resource".into();
            }
            cache
                .discover_with(&newer, false, &CancellationToken::new())
                .await
                .unwrap();
            *older.identity.lock().unwrap() = newer.identity.lock().unwrap().clone();
            older.release.add_permits(1);
            assert!(pending.await.unwrap().is_err());
            let directory = cache
                .discover_with(&newer, false, &CancellationToken::new())
                .await
                .unwrap();
            assert_eq!(directory.models, newer.directory.models);
            assert_eq!(newer.calls.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn logout_and_cancel_reject_late_results_even_for_same_account() {
        for logout in [false, true] {
            let cache = AccountModelDiscovery::default();
            let adapter = Arc::new(ControlledAdapter::new("account-a", true));
            let cancel = CancellationToken::new();
            let pending_cache = cache.clone();
            let pending_adapter = adapter.clone();
            let pending_cancel = cancel.clone();
            let pending = tokio::spawn(async move {
                pending_cache
                    .discover_with(pending_adapter.as_ref(), false, &pending_cancel)
                    .await
            });
            adapter.started.notified().await;
            if logout {
                cache.invalidate("test-provider");
            } else {
                cancel.cancel();
            }
            adapter.release.add_permits(1);
            assert!(pending.await.unwrap().is_err());
            assert!(cache.state.lock().unwrap().entries.is_empty());
        }
    }

    #[tokio::test]
    async fn logout_during_initial_identity_resolution_cannot_restore_old_evidence() {
        struct GatedIdentity {
            adapter: ControlledAdapter,
            started: Notify,
            release: Semaphore,
        }
        #[async_trait::async_trait]
        impl AccountDirectoryAdapter for GatedIdentity {
            async fn identity(
                &self,
                cancel: &CancellationToken,
            ) -> Result<DiscoveryIdentity, String> {
                self.started.notify_one();
                self.release.acquire().await.unwrap().forget();
                self.adapter.identity(cancel).await
            }
            async fn fetch(
                &self,
                cancel: &CancellationToken,
            ) -> Result<AccountModelDirectory, String> {
                self.adapter.fetch(cancel).await
            }
        }
        let cache = AccountModelDiscovery::default();
        let old = Arc::new(GatedIdentity {
            adapter: ControlledAdapter::new("account-a", false),
            started: Notify::new(),
            release: Semaphore::new(0),
        });
        let pending_cache = cache.clone();
        let pending_old = old.clone();
        let pending = tokio::spawn(async move {
            pending_cache
                .discover_with(pending_old.as_ref(), false, &CancellationToken::new())
                .await
        });
        old.started.notified().await;
        cache.invalidate("test-provider");
        let relogged = ControlledAdapter::new("account-a", false);
        cache
            .discover_with(&relogged, false, &CancellationToken::new())
            .await
            .unwrap();
        old.release.add_permits(1);
        assert!(pending.await.unwrap().is_err());
        assert_eq!(old.adapter.calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            cache
                .discover_with(&relogged, false, &CancellationToken::new())
                .await
                .unwrap()
                .models,
            relogged.directory.models
        );
    }

    #[test]
    fn authorization_semantics_and_contracts_remain_separate() {
        for strategy in [ModelDiscovery::OpenAiApi, ModelDiscovery::Codex] {
            let body = if strategy == ModelDiscovery::OpenAiApi {
                br#"{"data":[{"id":"known"},{"id":"not-integrated"}],"has_more":true}"#.as_slice()
            } else {
                br#"{"models":[{"slug":"known","supported_in_api":false,"visibility":"hide"},{"slug":"not-integrated"}]}"#.as_slice()
            };
            let directory = parse_directory(strategy, body).unwrap();
            assert_eq!(
                directory.availability("known"),
                ModelAvailability::DirectoryListed
            );
            assert!(matches!(
                directory.availability("absent"),
                ModelAvailability::Unknown { .. }
            ));
            assert!(directory.models.contains_key("not-integrated"));
        }
        let mut directory = AccountModelDirectory {
            models: BTreeMap::new(),
            semantics: DirectorySemantics::ExclusiveAuthorization,
            complete: false,
        };
        assert!(directory.availability("absent").selectable());
        directory.complete = true;
        assert!(!directory.availability("absent").selectable());
        directory.models.insert(
            "disabled".into(),
            ModelAvailability::Unavailable {
                reason: "Organization policy disabled this model".into(),
            },
        );
        assert!(!directory.availability("disabled").selectable());
        for body in [
            br#"{"data":[{"id":"a"},{"id":"a"}]}"#.as_slice(),
            br#"{"data":[{}]}"#.as_slice(),
            br#"{"models":[]}"#.as_slice(),
        ] {
            assert!(parse_directory(ModelDiscovery::OpenAiApi, body).is_err());
        }
    }

    #[test]
    fn endpoints_use_the_declared_product_and_reject_other_origins() {
        let catalog = super::super::cards::ModelCatalog::defaults();
        let mut api = catalog.get("openai-api", "gpt-6-astra").unwrap();
        assert_eq!(
            discovery_endpoint(&api, ModelDiscovery::OpenAiApi)
                .unwrap()
                .as_str(),
            "https://api.openai.com/v1/models"
        );
        let codex = catalog.get("openai-codex", "gpt-5.5").unwrap();
        assert_eq!(
            discovery_endpoint(&codex, ModelDiscovery::Codex)
                .unwrap()
                .path(),
            "/backend-api/codex/models"
        );
        api.base_url = "https://unrelated.example/v1".into();
        assert!(discovery_endpoint(&api, ModelDiscovery::OpenAiApi).is_err());
        api.base_url = "https://api.openai.com/v1?token=secret".into();
        assert!(discovery_endpoint(&api, ModelDiscovery::OpenAiApi).is_err());
    }
}
