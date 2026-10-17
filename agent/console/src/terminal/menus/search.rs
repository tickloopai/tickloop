use super::*;
use tickloop_agent_protocol::{ApiKeyStatus, SearchConfig, SearchProviderKind};

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
                        if kind == tickloop_agent_protocol::SearchProviderKind::Free {
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
            } => match index {
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
                Some(1) => self.search_endpoint(console, target, config, return_to_settings)?,
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
                    self.search_provider(console, target, config, return_to_settings)?;
                }
                _ => return Err("search action does not exist".into()),
            },
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
                    console.write(
                        "Search API key saved privately; select Use to enable this source.\n",
                    )?;
                }
                self.search_provider(console, target, config, return_to_settings)?;
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
        self.show(console, Menu {
            id: Uuid::new_v4(), title: "Web search".into(),
            context: vec!["Search works without a Tickloop account. Select a source to configure it.".into(), "Free search requests Bing, then DuckDuckGo on failure; no browser is started.".into(), "Saved credentials stay on this computer. Explicit providers never switch automatically.".into()],
            options, selected, confirmation: false,
        }, Selection::Search { target, return_to_settings })
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
        } else {
            options.push("Set endpoint URL".into());
        }
        context
            .push("Saving a key does not verify access or change the active search source.".into());
        self.show(
            console,
            Menu {
                id: Uuid::new_v4(),
                title: format!("Web search · {}", config.provider.label()),
                context,
                options,
                selected: 0,
                confirmation: false,
            },
            Selection::SearchProvider {
                target,
                config,
                return_to_settings,
            },
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
            ],
            options: vec!["Save API key".into(), "Cancel".into()],
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
