use super::super::style::Role;
use super::*;
use sha2::{Digest, Sha256};
use tickloop_agent_protocol::{
    ApiKeyStatus, SearchConfig, SearchProviderKind, SearchTestResult, SearchTestStatus,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SearchAction {
    Use,
    ConfigureKey,
    RemoveKey,
    ConfigureEndpoint,
}

impl SearchAction {
    fn label(self) -> &'static str {
        match self {
            Self::Use => "Use this search provider",
            Self::ConfigureKey => "Set or replace API key",
            Self::RemoveKey => "Remove saved API key",
            Self::ConfigureEndpoint => "Set endpoint URL",
        }
    }
}

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
                            self.open_search_provider(
                                agent,
                                console,
                                target,
                                config,
                                return_to_settings,
                            )?;
                        }
                    }
                } else if return_to_settings {
                    self.settings(agent, console, target).await?;
                }
            }
            Selection::SearchProvider {
                target,
                config,
                actions,
                return_to_settings,
            } => {
                let action = index
                    .map(|index| {
                        actions
                            .get(index)
                            .copied()
                            .ok_or("search action does not exist")
                    })
                    .transpose()?;
                let checking = self
                    .provider
                    .search
                    .flow
                    .as_ref()
                    .is_some_and(|flow| flow.config == config);
                if action != Some(SearchAction::Use) || !checking {
                    self.provider.search.flow.take();
                }
                match action {
                    None => {
                        self.search_settings(agent, console, target, return_to_settings)
                            .await?
                    }
                    Some(SearchAction::Use) => {
                        if checking {
                            self.search_provider(console, target, config, return_to_settings)?;
                        } else if let Some(name) = config
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
                        } else if !self.search_provider_ready(&config) {
                            self.search_provider(console, target, config, return_to_settings)?;
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
                    Some(SearchAction::ConfigureKey) => {
                        self.search_key(console, target, config, return_to_settings)?
                    }
                    Some(SearchAction::ConfigureEndpoint) => {
                        self.search_endpoint(console, target, config, return_to_settings)?
                    }
                    Some(SearchAction::RemoveKey) => {
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
                        self.open_search_provider(
                            agent,
                            console,
                            target,
                            config,
                            return_to_settings,
                        )?;
                    }
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
                    self.open_search_provider(agent, console, target, config, return_to_settings)?;
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
                self.open_search_provider(agent, console, target, config, return_to_settings)?;
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
                    ""
                } else {
                    "custom endpoint"
                };
                let config = current
                    .as_ref()
                    .filter(|config| config.provider == *kind)
                    .cloned()
                    .unwrap_or_else(|| SearchConfig::for_provider(*kind));
                let tested = self.search_verification(&config);
                let active = current
                    .as_ref()
                    .is_some_and(|config| config.provider == *kind);
                let role = match tested.map(|result| result.status) {
                    Some(SearchTestStatus::Available) if active => Role::SuccessMark,
                    Some(SearchTestStatus::Available) => Role::Text,
                    Some(SearchTestStatus::Unavailable) => Role::MutedText,
                    None if status == "credential unavailable" => Role::MutedText,
                    None if status == "key needed" => Role::MutedText,
                    None if active => Role::SuccessMark,
                    None => Role::Text,
                };
                row_roles.push(Some(role));
                let status = if tested
                    .is_some_and(|result| result.status == SearchTestStatus::Unavailable)
                {
                    if status.is_empty() {
                        "unavailable".into()
                    } else {
                        format!("{status} · unavailable")
                    }
                } else {
                    status.to_owned()
                };
                format!(
                    "{}{}{}",
                    kind.label(),
                    if status.is_empty() {
                        String::new()
                    } else {
                        format!(" · {status}")
                    },
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
                context: Vec::new(),
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

    fn open_search_provider(
        &mut self,
        agent: &Agent,
        console: &Console,
        target: ConfigTarget,
        config: SearchConfig,
        return_to_settings: bool,
    ) -> Result<(), String> {
        let has_key = config
            .api_key_env
            .as_deref()
            .or(config.provider.credential_env())
            .is_none_or(|name| {
                matches!(
                    self.search_key_status(name),
                    Ok(ApiKeyStatus::Saved | ApiKeyStatus::Environment)
                )
            });
        let has_endpoint =
            config.provider != SearchProviderKind::Searxng || !config.endpoint.is_empty();
        if has_key && has_endpoint {
            self.start_search_test(agent, console, target, config, return_to_settings)
        } else {
            self.invalidate_search_test(config.provider);
            self.search_provider(console, target, config, return_to_settings)
        }
    }

    fn search_provider_ready(&self, config: &SearchConfig) -> bool {
        self.search_verification(config)
            .map_or(config.provider == SearchProviderKind::Free, |result| {
                result.status == SearchTestStatus::Available
            })
    }

    pub(super) fn search_provider(
        &mut self,
        console: &Console,
        target: ConfigTarget,
        config: SearchConfig,
        return_to_settings: bool,
    ) -> Result<(), String> {
        let mut actions = vec![SearchAction::Use];
        let mut context = config
            .provider
            .website()
            .map(|website| vec![format!("Website: {website}")])
            .unwrap_or_default();
        if !config.endpoint.is_empty()
            && (config.provider == SearchProviderKind::Searxng
                || config.endpoint != config.provider.default_endpoint())
        {
            context.push(format!("Endpoint: {}", safe_text(&config.endpoint)));
        }
        if let Some(name) = config
            .api_key_env
            .as_deref()
            .or(config.provider.credential_env())
        {
            actions.push(SearchAction::ConfigureKey);
            if self.search_key_status(name) == Ok(ApiKeyStatus::Saved) {
                actions.push(SearchAction::RemoveKey);
            }
        } else if config.provider != SearchProviderKind::Free {
            actions.push(SearchAction::ConfigureEndpoint);
        }
        let checking = self
            .provider
            .search
            .flow
            .as_ref()
            .is_some_and(|flow| flow.config == config);
        let ready = !checking && self.search_provider_ready(&config);
        let options = actions
            .iter()
            .map(|action| {
                if *action == SearchAction::Use && checking {
                    format!("{} (checking…)", action.label())
                } else if *action == SearchAction::Use && !ready {
                    format!("{} (unavailable)", action.label())
                } else if *action == SearchAction::Use {
                    format!("{} (available)", action.label())
                } else {
                    action.label().to_owned()
                }
            })
            .collect();
        let row_role = if ready { Role::Text } else { Role::MutedText };
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
                    actions
                        .iter()
                        .position(|action| *action == SearchAction::ConfigureKey)
                        .unwrap_or(0)
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
                actions,
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
            status: SearchTestStatus::Unavailable,
            detail: "Search check could not complete. Reopen this provider to retry.".into(),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn available() -> SearchTestResult {
        SearchTestResult {
            status: SearchTestStatus::Available,
            detail: "Controlled successful test".into(),
        }
    }

    fn pending(root: &std::path::Path) -> (Menus, Console, tokio::task::AbortHandle) {
        let chat = Uuid::new_v4();
        let console = Console::test_interactive_for_chat(chat);
        let mut menus = Menus::new(None, Some(root.into()));
        menus.sync_provider_chat(Some(chat));
        let config = SearchConfig::for_provider(SearchProviderKind::Tavily);
        let credential = menus.search_credential_stamp(&config).unwrap();
        let (_sender, receiver) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(std::future::pending());
        let abort = task.abort_handle();
        menus.provider.search.flow = Some(SearchTestFlow {
            id: Uuid::new_v4(),
            target: ConfigTarget::Agent(chat),
            config: config.clone(),
            credential,
            return_to_settings: false,
            task,
            receiver,
        });
        menus
            .search_provider(&console, ConfigTarget::Agent(chat), config, false)
            .unwrap();
        (menus, console, abort)
    }

    #[tokio::test]
    async fn search_test_results_are_invalidated_by_replaced_credentials_and_isolated_by_agent() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        tickloop_agent_model::credentials::save_api_key(
            &root,
            "TAVILY_API_KEY",
            "first-private-key",
        )
        .unwrap();
        let (mut menus, console, _) = pending(&root);
        assert!(menus.search_test_active());
        tickloop_agent_model::credentials::save_api_key(
            &root,
            "TAVILY_API_KEY",
            "replacement-private-key",
        )
        .unwrap();
        menus.finish_search_test(&console, Ok(available())).unwrap();
        let config = SearchConfig::for_provider(SearchProviderKind::Tavily);
        assert!(
            menus.search_verification(&config).is_none(),
            "a late result cannot verify a replacement key"
        );
        let id = menus.id().unwrap();
        assert!(
            console
                .menu_snapshot(id)
                .unwrap()
                .unwrap()
                .context
                .iter()
                .all(|line| !line.starts_with("Connection:") && !line.contains("untested"))
        );
        menus.provider.search.tests.push(SearchVerification {
            config: config.clone(),
            credential: menus.search_credential_stamp(&config).unwrap(),
            result: available(),
        });
        assert!(menus.search_verification(&config).is_some());
        let original_chat = menus.provider_chat.unwrap();
        menus.sync_provider_chat(Some(Uuid::new_v4()));
        assert!(menus.search_verification(&config).is_none());
        menus.sync_provider_chat(original_chat);
        assert!(menus.search_verification(&config).is_some());
        let mut different_endpoint = config;
        different_endpoint.endpoint = "https://example.org/search".into();
        assert!(menus.search_verification(&different_endpoint).is_none());
    }

    #[tokio::test]
    async fn search_tests_cancel_on_dismiss_and_cannot_repaint_another_menu() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let (mut menus, console, abort) = pending(&root);
        menus.dismiss(&console).unwrap();
        tokio::task::yield_now().await;
        assert!(abort.is_finished());
        assert!(menus.provider.search.flow.is_none());
        let (mut menus, console, _) = pending(&root);
        let original = menus.id().unwrap();
        menus.provider.search.flow.as_mut().unwrap().id = Uuid::new_v4();
        assert!(!menus.search_test_active());
        menus.finish_search_test(&console, Ok(available())).unwrap();
        assert_eq!(menus.id(), Some(original));
        assert!(menus.provider.search.tests.is_empty());
    }
    #[tokio::test]
    async fn a_preempting_request_suspends_search_test_without_consuming_its_result() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let (mut menus, console, abort) = pending(&root);
        let chat = menus.provider_chat.unwrap().unwrap();
        let request = Uuid::new_v4();
        menus
            .workflow(
                &console,
                (
                    Menu {
                        id: request,
                        title: "Plan approval".into(),
                        context: Vec::new(),
                        options: vec!["Approve".into(), "Cancel".into()],
                        selected: 1,
                        confirmation: true,
                    },
                    workflows::Workflow::Plan {
                        chat,
                        goal: request,
                        version: 1,
                        request: None,
                    },
                ),
            )
            .unwrap();
        assert!(menus.provider.search.flow.is_some());
        assert!(!abort.is_finished());
        assert!(!menus.search_test_active());
        assert_eq!(menus.id(), Some(request));
    }
}
