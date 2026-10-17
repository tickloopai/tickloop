use super::super::style::Role;
use super::*;
use sha2::{Digest, Sha256};
use tickloop_agent_protocol::{
    ApiKeyStatus, SearchConfig, SearchProviderKind, SearchTestResult, SearchTestStatus,
};

#[derive(Default)]
pub(super) struct SearchSession {
    tests: Vec<SearchVerification>,
    pub(super) flow: Option<SearchTestFlow>,
}

#[derive(Clone, PartialEq, Eq)]
struct CredentialStamp {
    root: Option<PathBuf>,
    saved: Option<[u8; 32]>,
    environment: bool,
}

struct SearchVerification {
    config: SearchConfig,
    credential: CredentialStamp,
    result: SearchTestResult,
}

pub(super) struct SearchTestFlow {
    pub(super) id: Uuid,
    target: ConfigTarget,
    config: SearchConfig,
    credential: CredentialStamp,
    return_to_settings: bool,
    task: tokio::task::JoinHandle<()>,
    pub(super) receiver: tokio::sync::oneshot::Receiver<Result<SearchTestResult, String>>,
}

impl Drop for SearchTestFlow {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn test_role(status: SearchTestStatus) -> Role {
    match status {
        SearchTestStatus::Available => Role::SuccessMark,
        SearchTestStatus::AuthenticationFailed | SearchTestStatus::ConfigurationFailed => {
            Role::ErrorMark
        }
        _ => Role::WarningMark,
    }
}

impl Menus {
    pub(super) async fn choose_search(
        &mut self,
        agent: &Agent,
        console: &Console,
        selection: Selection,
        index: Option<usize>,
        comment: Option<String>,
    ) -> Result<(), String> {
        match selection {
            Selection::Search {
                target,
                return_to_settings,
            } => {
                if let Some(index) = index {
                    if index == tickloop_agent_protocol::SearchProviderKind::ALL.len() {
                        agent
                            .change_configuration(
                                target,
                                ConfigurationChange::Search { config: None },
                            )
                            .await
                            .map_err(|error| error.to_string())?;
                        self.search_settings(agent, console, target, return_to_settings)
                            .await?;
                    } else {
                        let kind = *tickloop_agent_protocol::SearchProviderKind::ALL
                            .get(index)
                            .ok_or("search provider does not exist")?;
                        let current = agent
                            .configuration(target)
                            .await
                            .map_err(|error| error.to_string())?
                            .search_config;
                        let config = current
                            .filter(|config| config.provider == kind)
                            .unwrap_or_else(|| {
                                tickloop_agent_protocol::SearchConfig::for_provider(kind)
                            });
                        let needs_key = config
                            .api_key_env
                            .as_deref()
                            .or(kind.credential_env())
                            .is_some_and(|name| {
                                self.search_key_status(name) == Ok(ApiKeyStatus::Missing)
                            });
                        if needs_key {
                            self.search_key(console, target, config, return_to_settings)?;
                        } else {
                            self.search_provider(console, target, config, return_to_settings)?;
                        }
                    }
                } else if return_to_settings {
                    self.settings(agent, console, target).await?;
                }
            }
            Selection::SearchProvider {
                target,
                config,
                return_to_settings,
            } => {
                self.provider.search.flow.take();
                match index {
                    None => {
                        self.search_settings(agent, console, target, return_to_settings)
                            .await?
                    }
                    Some(0) => {
                        if let Some(name) = config
                            .api_key_env
                            .as_deref()
                            .or(config.provider.credential_env())
                            && self.search_key_status(name)?
                                == tickloop_agent_protocol::ApiKeyStatus::Missing
                        {
                            self.search_key(console, target, config, return_to_settings)?;
                        } else if config.provider
                            == tickloop_agent_protocol::SearchProviderKind::Searxng
                            && config.endpoint.is_empty()
                        {
                            self.search_endpoint(console, target, config, return_to_settings)?;
                        } else {
                            agent
                                .change_configuration(
                                    target,
                                    ConfigurationChange::Search {
                                        config: Some(config),
                                    },
                                )
                                .await
                                .map_err(|error| error.to_string())?;
                            self.search_settings(agent, console, target, return_to_settings)
                                .await?;
                        }
                    }
                    Some(1)
                        if config.api_key_env.is_some()
                            || config.provider.credential_env().is_some() =>
                    {
                        self.search_key(console, target, config, return_to_settings)?
                    }
                    Some(1) if config.provider == SearchProviderKind::Free => {
                        self.start_search_test(agent, console, target, config, return_to_settings)?;
                    }
                    Some(1) => self.search_endpoint(console, target, config, return_to_settings)?,
                    Some(2)
                        if config.api_key_env.is_none()
                            && config.provider.credential_env().is_none() =>
                    {
                        self.start_search_test(agent, console, target, config, return_to_settings)?;
                    }
                    Some(2) => {
                        let root = self
                            .provider_root()
                            .ok_or("Agent settings directory is unavailable")?;
                        let name = config
                            .api_key_env
                            .as_deref()
                            .or(config.provider.credential_env())
                            .ok_or("This provider has no API key")?;
                        tickloop_agent_model::credentials::remove_api_key(root, name)
                            .map_err(|error| error.to_string())?;
                        self.invalidate_search_test(config.provider);
                        self.search_provider(console, target, config, return_to_settings)?;
                    }
                    Some(3) => {
                        self.start_search_test(agent, console, target, config, return_to_settings)?
                    }
                    _ => return Err("search action does not exist".into()),
                }
            }
            Selection::SearchKey {
                target,
                config,
                return_to_settings,
            } => {
                if index == Some(0) {
                    let root = self
                        .provider_root()
                        .ok_or("Agent settings directory is unavailable")?;
                    let name = config
                        .api_key_env
                        .as_deref()
                        .or(config.provider.credential_env())
                        .ok_or("This provider has no API key")?;
                    if let Err(error) = tickloop_agent_model::credentials::save_api_key(
                        root,
                        name,
                        comment.as_deref().unwrap_or(""),
                    ) {
                        self.search_key(console, target, config, return_to_settings)?;
                        return Err(error.to_string());
                    }
                    self.invalidate_search_test(config.provider);
                    self.start_search_test(agent, console, target, config, return_to_settings)?;
                } else {
                    self.search_provider(console, target, config, return_to_settings)?;
                }
            }
            Selection::SearchEndpoint {
                target,
                mut config,
                return_to_settings,
            } => {
                if index == Some(0) {
                    config.endpoint = comment.as_deref().unwrap_or("").trim().to_owned();
                    if let Err(error) = tickloop_agent_tools::web_search::validate_config(&config) {
                        self.search_endpoint(console, target, config, return_to_settings)?;
                        return Err(error);
                    }
                }
                self.search_provider(console, target, config, return_to_settings)?;
            }

            _ => unreachable!("only search selections reach the search handler"),
        }
        Ok(())
    }

    pub(in crate::terminal) async fn search_settings(
        &mut self,
        agent: &Agent,
        console: &Console,
        target: ConfigTarget,
        return_to_settings: bool,
    ) -> Result<(), String> {
        Box::pin(self.search_settings_on_heap(agent, console, target, return_to_settings)).await
    }

    async fn search_settings_on_heap(
        &mut self,
        agent: &Agent,
        console: &Console,
        target: ConfigTarget,
        return_to_settings: bool,
    ) -> Result<(), String> {
        self.refresh_provider_scope(agent, target).await?;
        let current = agent
            .configuration(target)
            .await
            .map_err(|error| error.to_string())?
            .search_config;
        let mut row_roles = Vec::new();
        let mut options = SearchProviderKind::ALL
            .iter()
            .map(|kind| {
                let credential = current
                    .as_ref()
                    .filter(|config| config.provider == *kind)
                    .and_then(|config| config.api_key_env.as_deref())
                    .or(kind.credential_env());
                let status = if let Some(name) = credential {
                    match self.search_key_status(name) {
                        Ok(ApiKeyStatus::Saved) => "key saved",
                        Ok(ApiKeyStatus::Environment) => "key from environment",
                        Ok(ApiKeyStatus::Missing) => "key needed",
                        Err(_) => "credential unavailable",
                    }
                } else if *kind == SearchProviderKind::Free {
                    "no key needed"
                } else {
                    "custom endpoint"
                };
                let config = current
                    .as_ref()
                    .filter(|config| config.provider == *kind)
                    .cloned()
                    .unwrap_or_else(|| SearchConfig::for_provider(*kind));
                let tested = self.search_verification(&config);
                row_roles.push(Some(tested.map_or_else(
                    || {
                        if status == "credential unavailable" {
                            Role::ErrorMark
                        } else {
                            Role::MutedText
                        }
                    },
                    |result| test_role(result.status),
                )));
                let test_status = tested.map_or("", |result| result.status.label());
                let status = if test_status.is_empty() {
                    format!("{status} · untested")
                } else {
                    format!("{status} · {test_status}")
                };
                format!(
                    "{} · {status}{}",
                    kind.label(),
                    if current
                        .as_ref()
                        .is_some_and(|config| config.provider == *kind)
                    {
                        " · active"
                    } else {
                        ""
                    }
                )
            })
            .collect::<Vec<_>>();
        options.push(format!(
            "Off{}",
            if current.is_none() { " · active" } else { "" }
        ));
        let selected = current
            .as_ref()
            .and_then(|config| {
                SearchProviderKind::ALL
                    .iter()
                    .position(|kind| *kind == config.provider)
            })
            .unwrap_or(SearchProviderKind::ALL.len());
        self.show_with_roles(
            console,
            Menu {
                id: Uuid::new_v4(),
                title: "Web search".into(),
                context: vec![
                    format!(
                        "Web search: {}",
                        current
                            .as_ref()
                            .map_or("Off", |config| config.provider.label())
                    ),
                    "Select a source to set up, test or enable it. No Tickloop account needed."
                        .into(),
                    "Colors show the latest test in this View; active marks the selected source."
                        .into(),
                ],
                options,
                selected,
                confirmation: false,
            },
            Selection::Search {
                target,
                return_to_settings,
            },
            row_roles,
        )
    }

    pub(super) fn search_key_status(&self, name: &str) -> Result<ApiKeyStatus, String> {
        if let Some(root) = self.provider_root()
            && tickloop_agent_model::credentials::load_api_key(root, name)
                .map_err(|error| error.to_string())?
                .is_some()
        {
            return Ok(ApiKeyStatus::Saved);
        }
        Ok(if self.provider_environment_key(name) {
            ApiKeyStatus::Environment
        } else {
            ApiKeyStatus::Missing
        })
    }

    pub(super) fn search_provider(
        &mut self,
        console: &Console,
        target: ConfigTarget,
        config: SearchConfig,
        return_to_settings: bool,
    ) -> Result<(), String> {
        let mut options = vec!["Use this search provider".into()];
        let mut context = vec![
            format!("Provider: {}", config.provider.label()),
            format!("Endpoint: {}", safe_text(&config.endpoint)),
        ];
        if let Some(name) = config
            .api_key_env
            .as_deref()
            .or(config.provider.credential_env())
        {
            context.push(format!(
                "Credential: {}",
                match self.search_key_status(name) {
                    Ok(ApiKeyStatus::Saved) => "saved privately",
                    Ok(ApiKeyStatus::Environment) => "inherited environment",
                    Ok(ApiKeyStatus::Missing) => "not configured",
                    Err(_) => "unavailable or unsafe",
                }
            ));
            options.extend([
                "Set or replace API key".into(),
                "Remove saved API key".into(),
            ]);
        } else if config.provider != SearchProviderKind::Free {
            options.push("Set endpoint URL".into());
        }
        options.push("Test search connection".into());
        let checking = self
            .provider
            .search
            .flow
            .as_ref()
            .is_some_and(|flow| flow.config == config);
        let result = self.search_verification(&config);
        context.push(format!(
            "Latest test: {}",
            if checking {
                "testing…"
            } else {
                result.map_or("untested", |result| result.status.label())
            }
        ));
        if let Some(result) = result {
            context.push(result.detail.clone());
        }
        context.push("Testing sends one real search and may use provider credits. Saving or testing does not enable this source.".into());
        let row_role = if checking {
            Role::MutedText
        } else {
            result.map_or(Role::MutedText, |result| test_role(result.status))
        };
        let previous = self.active.as_ref().filter(|(_, selection)| {
            matches!(selection,
            Selection::SearchProvider { target: previous_target, config: previous, .. }
                if *previous_target == target && *previous == config)
        });
        let selected = previous
            .and_then(|(id, _)| console.picker_selection(*id).ok().flatten())
            .unwrap_or_else(|| {
                if config
                    .api_key_env
                    .as_deref()
                    .or(config.provider.credential_env())
                    .is_some_and(|name| self.search_key_status(name) == Ok(ApiKeyStatus::Missing))
                {
                    1
                } else {
                    0
                }
            });
        let id = self
            .provider
            .search
            .flow
            .as_ref()
            .filter(|flow| flow.config == config)
            .map(|flow| flow.id)
            .or_else(|| previous.map(|(id, _)| *id))
            .unwrap_or_else(Uuid::new_v4);
        self.show_with_roles(
            console,
            Menu {
                id,
                title: format!("Web search · {}", config.provider.label()),
                context,
                options,
                selected,
                confirmation: false,
            },
            Selection::SearchProvider {
                target,
                config,
                return_to_settings,
            },
            vec![Some(row_role)],
        )
    }

    pub(super) fn search_key(
        &mut self,
        console: &Console,
        target: ConfigTarget,
        config: SearchConfig,
        return_to_settings: bool,
    ) -> Result<(), String> {
        self.provider_root().ok_or("Cannot save a key without an Agent settings directory; use the provider environment variable instead.")?;
        let name = config
            .api_key_env
            .as_deref()
            .or(config.provider.credential_env())
            .ok_or("This provider does not need an API key")?;
        if !console.is_interactive() {
            return Err(format!(
                "Secret entry requires an interactive terminal; set {name} in the Agent .env file."
            ));
        }
        let menu = Menu {
            id: Uuid::new_v4(),
            title: format!("API key · {}", config.provider.label()),
            context: vec![
                format!("Saved privately as {name}; the existing key is kept until Save succeeds."),
                "The key is hidden and is never added to command history.".into(),
                "Saving starts one test search, which may use provider credits; it does not enable the source.".into(),
            ],
            options: vec!["Save and test API key".into(), "Cancel".into()],
            selected: 0,
            confirmation: false,
        };
        self.dismiss(console)?;
        let id = menu.id;
        console.show_secret_menu_with_value(menu, "")?;
        self.active = Some((
            id,
            Selection::SearchKey {
                target,
                config,
                return_to_settings,
            },
        ));
        Ok(())
    }

    pub(super) fn search_endpoint(
        &mut self,
        console: &Console,
        target: ConfigTarget,
        config: SearchConfig,
        return_to_settings: bool,
    ) -> Result<(), String> {
        let menu = Menu {
            id: Uuid::new_v4(),
            title: "Search endpoint".into(),
            context: vec![
                "HTTPS JSON search URL; loopback HTTP is also supported.".into(),
                "Do not put credentials, a query string, or a fragment in the URL.".into(),
            ],
            options: vec!["Save endpoint".into(), "Cancel".into()],
            selected: 0,
            confirmation: false,
        };
        self.dismiss(console)?;
        let id = menu.id;
        console.show_input_menu(menu, "Endpoint", &config.endpoint)?;
        self.active = Some((
            id,
            Selection::SearchEndpoint {
                target,
                config,
                return_to_settings,
            },
        ));
        Ok(())
    }
}

impl Menus {
    fn search_credential_stamp(&self, config: &SearchConfig) -> Result<CredentialStamp, String> {
        let name = config
            .api_key_env
            .as_deref()
            .or(config.provider.credential_env());
        let saved = match (self.provider_root(), name) {
            (Some(root), Some(name)) => tickloop_agent_model::credentials::load_api_key(root, name)
                .map_err(|error| error.to_string())?
                .map(|key| Sha256::digest(key.as_bytes()).into()),
            _ => None,
        };
        Ok(CredentialStamp {
            root: self.provider_root().map(PathBuf::from),
            saved,
            environment: name.is_some_and(|name| self.provider_environment_key(name)),
        })
    }

    fn search_verification(&self, config: &SearchConfig) -> Option<&SearchTestResult> {
        let stamp = self.search_credential_stamp(config).ok()?;
        self.provider
            .search
            .tests
            .iter()
            .find(|test| test.config == *config && test.credential == stamp)
            .map(|test| &test.result)
    }

    fn invalidate_search_test(&mut self, kind: SearchProviderKind) {
        self.provider
            .search
            .tests
            .retain(|test| test.config.provider != kind);
    }

    fn start_search_test(
        &mut self,
        agent: &Agent,
        console: &Console,
        target: ConfigTarget,
        config: SearchConfig,
        return_to_settings: bool,
    ) -> Result<(), String> {
        self.provider.search.flow.take();
        self.invalidate_search_test(config.provider);
        let credential = self.search_credential_stamp(&config)?;
        let id = Uuid::new_v4();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let agent = agent.clone();
        let test_config = config.clone();
        let task = tokio::spawn(async move {
            let result = agent.test_search(target, test_config).await;
            let _ = sender.send(result);
        });
        self.provider.search.flow = Some(SearchTestFlow {
            id,
            target,
            config: config.clone(),
            credential,
            return_to_settings,
            task,
            receiver,
        });
        self.search_provider(console, target, config, return_to_settings)
    }

    pub(in crate::terminal) fn search_test_active(&self) -> bool {
        self.provider.search.flow.as_ref().is_some_and(|flow|
            matches!(self.active, Some((id, Selection::SearchProvider { .. })) if id == flow.id))
    }

    pub(in crate::terminal) fn finish_search_test(
        &mut self,
        console: &Console,
        result: Result<SearchTestResult, String>,
    ) -> Result<(), String> {
        let Some(flow) = self.provider.search.flow.take() else {
            return Ok(());
        };
        if self.id() != Some(flow.id) {
            return Ok(());
        }
        let result = result.unwrap_or(SearchTestResult {
            status: SearchTestStatus::ServiceFailed,
            detail: "Search test could not complete. The key remains saved; retry the test.".into(),
        });
        if self.search_credential_stamp(&flow.config).ok().as_ref() == Some(&flow.credential) {
            self.provider.search.tests.push(SearchVerification {
                config: flow.config.clone(),
                credential: flow.credential.clone(),
                result,
            });
        }
        self.search_provider(
            console,
            flow.target,
            flow.config.clone(),
            flow.return_to_settings,
        )?;
        Ok(())
    }
}
