use super::super::login::SignInTarget;
use super::*;
use tickloop_agent::model::{
    ModelAuthSource,
    availability::{AccountModelDirectory, ModelAvailability},
    cards::Adapter,
    connection::ModelDiscovery,
    credentials::{ApiKeyStatus, api_key_status},
};

#[derive(Clone)]
pub(super) struct ProviderModels {
    pub(super) models: Vec<ModelConfig>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimax_connections_collapse_without_merging_provider_identities() {
        let choices =
            ProviderModels::group(tickloop_agent::model::cards::built_in_model_profiles());
        let rows = provider_rows(&choices, None);
        assert_eq!(
            rows.iter()
                .filter(|row| matches!(row, ProviderRow::Group(name) if name == "MiniMax"))
                .count(),
            1
        );
        assert!(rows.iter().all(|row| match row {
            ProviderRow::Provider(index) => provider_group(&choices[*index]).is_none(),
            ProviderRow::Group(_) => true,
        }));
        let connections = provider_rows(&choices, Some("MiniMax"));
        assert_eq!(connections.len(), 4);
        let ids = connections
            .iter()
            .map(|row| match row {
                ProviderRow::Provider(index) => choices[*index].models[0].provider_id.clone(),
                ProviderRow::Group(_) => panic!("connections must reference actual Providers"),
            })
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(ids.len(), 4);
    }

    #[test]
    fn glm_regions_share_one_entry_with_separate_providers() {
        let choices =
            ProviderModels::group(tickloop_agent::model::cards::built_in_model_profiles());
        let rows = provider_rows(&choices, None);
        assert_eq!(
            rows.iter()
                .filter(|row| matches!(row, ProviderRow::Group(name) if name == "Zhipu GLM"))
                .count(),
            1
        );
        let connections = provider_rows(&choices, Some("Zhipu GLM"))
            .into_iter()
            .map(|row| match row {
                ProviderRow::Provider(index) => choices[index].models[0].api_key_env.clone(),
                ProviderRow::Group(_) => panic!("connections must reference actual Providers"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            connections,
            [
                "ZHIPU_API_KEY",
                "ZAI_API_KEY",
                "ZHIPU_CODING_API_KEY",
                "ZAI_CODING_API_KEY"
            ]
        );
    }

    #[test]
    fn connection_metadata_drives_order_labels_and_old_snapshot_compatibility() {
        let mut profiles = tickloop_agent::model::cards::built_in_model_profiles();
        profiles.reverse();
        let choices = ProviderModels::group(profiles);
        let ids = provider_rows(&choices, Some("Zhipu GLM"))
            .into_iter()
            .map(|row| match row {
                ProviderRow::Provider(index) => choices[index].models[0].provider_id.clone(),
                ProviderRow::Group(_) => unreachable!(),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            [
                "zhipu-cn-api",
                "zai-global-api",
                "zhipu-cn-coding",
                "zai-global-coding"
            ]
        );
        let openai = provider_rows(&choices, Some("OpenAI"));
        let names = openai
            .iter()
            .map(|row| match row {
                ProviderRow::Provider(index) => choices[*index].name(),
                ProviderRow::Group(_) => unreachable!(),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "Pay-as-you-go API (api.openai.com)",
                "Codex subscription (chatgpt.com)"
            ]
        );
        let mut legacy = tickloop_agent::model::cards::ModelCatalog::defaults()
            .get("openai-api", "gpt-6-astra-responses")
            .unwrap();
        legacy.connection = None;
        let rows = super::super::super::settings::model_choices_with_profiles(
            &legacy,
            tickloop_agent::model::cards::built_in_model_profiles(),
        );
        assert_eq!(
            rows.iter()
                .filter(|model| model.provider_id == legacy.provider_id
                    && model.model_id == legacy.model_id)
                .count(),
            1
        );
        let old_provider = ProviderModels {
            models: vec![legacy.clone()],
        };
        assert_eq!(old_provider.name(), "Pay-as-you-go API (api.openai.com)");
        assert_eq!(provider_group(&old_provider), Some("OpenAI"));
        legacy.base_url = "http://127.0.0.1:4321/v1".into();
        assert_eq!(
            ProviderModels {
                models: vec![legacy]
            }
            .name(),
            "Pay-as-you-go API (127.0.0.1:4321)"
        );
    }

    #[test]
    fn copilot_is_one_vendor_entry_named_by_product_and_service_host() {
        let choices =
            ProviderModels::group(tickloop_agent::model::cards::built_in_model_profiles());
        let rows = provider_rows(&choices, None);
        assert_eq!(
            rows.iter()
                .filter(|row| matches!(row, ProviderRow::Group(name) if name == "GitHub Copilot"))
                .count(),
            1
        );
        let connections = provider_rows(&choices, Some("GitHub Copilot"));
        let [ProviderRow::Provider(index)] = connections.as_slice() else {
            panic!("GitHub Copilot has exactly one connection");
        };
        assert_eq!(
            choices[*index].name(),
            "Copilot subscription (api.githubcopilot.com)"
        );
        assert_eq!(choices[*index].models.len(), 8);
        assert_eq!(
            SignInTarget::device_only(choices[*index].models[0].auth_source),
            Some(SignInTarget::Copilot)
        );
    }

    #[test]
    fn retired_glm_card_is_not_offered_even_as_the_current_model() {
        let defaults = tickloop_agent::model::cards::ModelCatalog::defaults();
        let retired = defaults
            .get("zhipu-cn-api", "glm-5.2-chat-completions")
            .unwrap();
        let profiles = tickloop_agent::model::cards::built_in_model_profiles();
        let choices =
            super::super::super::settings::model_choices_with_profiles(&retired, profiles);
        assert!(!choices.contains(&retired));
        let current = defaults
            .get("zhipu-cn-api", "glm-5.3-flash-chat-completions")
            .unwrap();
        let choices = super::super::super::settings::model_choices_with_profiles(
            &current,
            tickloop_agent::model::cards::built_in_model_profiles(),
        );
        assert_eq!(choices[0], current);
        assert_eq!(current.name, "GLM-5.3-Flash");
        assert_eq!(
            tickloop_agent::model::access::AccessCatalog::defaults()
                .series_for(&current)
                .unwrap()
                .label(),
            "GLM Flash (5.3)"
        );
    }
}

#[derive(Clone)]
pub(super) enum ProviderRow {
    Provider(usize),
    Group(String),
}

fn provider_group(provider: &ProviderModels) -> Option<&str> {
    let model = &provider.models[0];
    model
        .connection()
        .map(|connection| connection.vendor.as_str())
        .or_else(|| crate::model::cards::provider_group(&model.provider_id))
}

pub(super) fn provider_rows(choices: &[ProviderModels], group: Option<&str>) -> Vec<ProviderRow> {
    let mut rows = Vec::new();
    for (index, provider) in choices.iter().enumerate() {
        if let Some(group) = group {
            if provider_group(provider) == Some(group) {
                rows.push(ProviderRow::Provider(index));
            }
        } else if let Some(group) = provider_group(provider) {
            if !rows
                .iter()
                .any(|row| matches!(row, ProviderRow::Group(name) if *name == group))
            {
                rows.push(ProviderRow::Group(group.to_owned()));
            }
        } else {
            rows.push(ProviderRow::Provider(index));
        }
    }
    rows
}

impl ProviderModels {
    pub(super) fn group(models: Vec<ModelConfig>) -> Vec<Self> {
        let mut choices: Vec<Self> = Vec::new();
        for model in models {
            if let Some(provider) = choices
                .iter_mut()
                .find(|provider| provider.models[0].provider_id == model.provider_id)
            {
                provider.models.push(model);
            } else {
                choices.push(Self {
                    models: vec![model],
                });
            }
        }
        choices.sort_by_key(|provider| {
            provider.models[0]
                .connection()
                .map(|connection| connection.sort_key())
        });
        choices
    }

    fn name(&self) -> String {
        let model = &self.models[0];
        if let Some(connection) = model.connection() {
            connection.label(&model.base_url)
        } else if model.provider_name.is_empty() {
            model.provider_id.clone()
        } else {
            model.provider_name.clone()
        }
    }
}

#[derive(Clone)]
pub(super) struct ModelChoice {
    pub(super) config: ModelConfig,
    pub(super) thinking: Option<ReasoningEffort>,
    pub(super) availability: ModelAvailability,
}

pub(super) fn thinking_choices(model: &ModelConfig) -> Vec<Option<ReasoningEffort>> {
    let mut choices = vec![None];
    if tickloop_agent::model::cards::model_card(model)
        .is_some_and(|card| card.thinking == tickloop_agent::model::cards::ThinkingControl::Toggle)
    {
        choices.push(Some(ReasoningEffort::Off));
        if model.adapter() == Adapter::MinimaxMessages {
            choices.push(Some(ReasoningEffort::On));
        }
    }
    choices.extend(ReasoningEffort::choices(model).iter().copied().map(Some));
    choices
}

impl Menus {
    pub(super) fn show_providers(
        &mut self,
        console: &Console,
        chat: Uuid,
        choices: Vec<ProviderModels>,
        selected: usize,
        id: Uuid,
    ) -> Result<(), String> {
        let group = self.provider_group.as_ref().map(|(name, _)| name.as_str());
        let rows = provider_rows(&choices, group);
        let mut options: Vec<String> = rows
            .iter()
            .map(|row| match row {
                ProviderRow::Group(name) => format!("{name} ›"),
                ProviderRow::Provider(index) => {
                    let provider = &choices[*index];
                    safe_text(&format!(
                        "{} · {}",
                        provider.name(),
                        self.provider_auth_label(provider)
                    ))
                }
            })
            .collect();
        if group.is_none() {
            options.push("Judgment settings · Jev (TypeSafe)".into());
            options.push("Sign out · Codex subscription".into());
        }
        self.show(
            console,
            Menu {
                id,
                title: group.map_or_else(|| "Choose Provider".into(), |name| format!("Choose connection · {name}")),
                context: vec![if group.is_some() {
                    "Choose the product and service connection. Authentication is shown separately. Esc returns to Providers.".into()
                } else {
                    "Choose the model used by this Chat. Judgment settings configures a separate model.".into()
                }],
                options,
                selected,
                confirmation: false,
            },
            Selection::Provider { chat, choices, rows },
        )
    }

    pub(super) fn back_to_providers(
        &mut self,
        console: &Console,
        chat: Uuid,
    ) -> Result<(), String> {
        self.show_providers(
            console,
            chat,
            self.provider_choices.clone(),
            self.provider_group
                .as_ref()
                .map_or(self.provider_selected, |(_, selected)| *selected),
            Uuid::new_v4(),
        )
    }

    fn provider_auth_label(&self, provider: &ProviderModels) -> &'static str {
        let config = &provider.models[0];
        if config.auth_source != ModelAuthSource::ApiKeyEnv {
            return match self.provider_authenticated(provider) {
                Ok(true) => "Signed in",
                Ok(false) => "Sign in required",
                Err(_) => "Sign in needs attention",
            };
        }
        if config.api_key_env.is_empty() {
            return "Local connection";
        }
        match self
            .config_root
            .as_deref()
            .map(|root| api_key_status(root, &config.api_key_env))
        {
            Some(Ok(ApiKeyStatus::Saved)) => "API key saved",
            Some(Ok(ApiKeyStatus::Environment)) => "API key from environment",
            Some(Ok(ApiKeyStatus::Missing)) => "API key required",
            Some(Err(_)) => "API key needs attention",
            None if self.provider_authenticated(provider).unwrap_or(false) => {
                "API key from environment"
            }
            None => "API key required",
        }
    }

    pub(super) fn provider_authenticated(&self, provider: &ProviderModels) -> Result<bool, String> {
        let config = &provider.models[0];
        if let Some(region) = crate::auth::minimax::MiniMaxRegion::from_auth(config.auth_source) {
            return self
                .config_root
                .as_deref()
                .map(|root| {
                    crate::auth::minimax::MiniMaxAuth::new(root, region)
                        .has_saved_login()
                        .map_err(|error| error.to_string())
                })
                .unwrap_or(Ok(false));
        }
        if config.auth_source == ModelAuthSource::GithubCopilot {
            return self
                .config_root
                .as_deref()
                .map(|root| {
                    crate::auth::copilot::CopilotAuth::new(root)
                        .has_saved_login()
                        .map_err(|error| error.to_string())
                })
                .unwrap_or(Ok(false));
        }
        if config.auth_source == ModelAuthSource::CodexSubscription {
            let Some(root) = self.config_root.as_deref() else {
                return Ok(false);
            };
            return super::super::login::terminal_auth(root.to_path_buf())
                .has_saved_login()
                .map_err(|error| error.to_string());
        }
        if config.api_key_env.is_empty() {
            return Ok(true);
        }
        if let Some(root) = self.config_root.as_deref() {
            api_key_status(root, &config.api_key_env)
                .map(|status| status != ApiKeyStatus::Missing)
                .map_err(|error| error.to_string())
        } else {
            Ok(std::env::var(&config.api_key_env).is_ok_and(|value| {
                !value.trim().is_empty() && reqwest::header::HeaderValue::from_str(&value).is_ok()
            }))
        }
    }

    pub(super) async fn provider_models(
        &mut self,
        agent: &Agent,
        console: &Console,
        chat: Uuid,
        provider: ProviderModels,
    ) -> Result<(), String> {
        if provider.models[0]
            .connection()
            .is_some_and(|connection| connection.discovery != ModelDiscovery::Unsupported)
        {
            let id = Uuid::new_v4();
            self.show(console, Menu {
                id, title: "Discovering account models".into(),
                context: vec![format!("Querying {} with this connection's credentials. Esc cancels; login stays saved.", provider.name())],
                options: vec!["Cancel".into()], selected: 0, confirmation: false,
            }, Selection::Discovery { chat })?;
            let config = provider.models[0].clone();
            let root = self.config_root.clone();
            let cache = self.model_discovery.clone();
            let cancel = tokio_util::sync::CancellationToken::new();
            let task_cancel = cancel.clone();
            let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
            let task = tokio::spawn(async move {
                let result = cache
                    .discover(&config, root.as_deref(), false, &task_cancel)
                    .await;
                let _ = sender.send(result);
            });
            self.discovery_flow = Some(DiscoveryFlow {
                id,
                chat,
                provider,
                cancel,
                task,
                receiver,
            });
            return Ok(());
        }
        self.provider_model_choices(
            agent,
            console,
            chat,
            provider,
            Err("Model discovery is not provided for this connection".into()),
        )
        .await
    }

    async fn provider_model_choices(
        &mut self,
        agent: &Agent,
        console: &Console,
        chat: Uuid,
        provider: ProviderModels,
        directory: Result<AccountModelDirectory, String>,
    ) -> Result<(), String> {
        self.account_directory_status = match &directory {
            Ok(directory) if directory.complete => "Account directory checked.".into(),
            Ok(_) => "Account directory is incomplete; missing models remain unverified.".into(),
            Err(_) if provider.models[0].connection().is_none_or(|connection| connection.discovery == ModelDiscovery::Unsupported) => "Account model access has not been checked.".into(),
            Err(error) if error.contains("timed out") => "Account model check timed out. Use Refresh to retry.".into(),
            Err(error) if error.contains("HTTP ") => {
                let status = error.split("HTTP ").nth(1).unwrap_or("").chars().take(3).collect::<String>();
                format!("Account model check failed (HTTP {status}). Login remains saved; use Refresh to retry.")
            }
            Err(_) => "Account model access could not be checked. Login remains saved; use Refresh to retry.".into(),
        };
        let snapshot = agent
            .snapshot(chat)
            .await
            .map_err(|error| error.to_string())?;
        let access = tickloop_agent::model::access::AccessCatalog::defaults();
        let mut models = provider.models.clone();
        models.sort_by_key(|model| access.series_position(model).unwrap_or(usize::MAX));
        let choices: Vec<_> = models
            .iter()
            .map(|model| {
                let preferred = if snapshot.thinking_enabled {
                    snapshot.reasoning_effort
                } else {
                    Some(ReasoningEffort::Off)
                };
                ModelChoice {
                    availability: directory.as_ref().map_or_else(
                        |error| ModelAvailability::Unknown {
                            reason: format!("Unverified · {error}"),
                        },
                        |directory| directory.availability(&model.model),
                    ),
                    config: model.clone(),
                    thinking: if thinking_choices(model).contains(&preferred) {
                        preferred
                    } else {
                        None
                    },
                }
            })
            .collect();
        let selected = choices
            .iter()
            .position(|choice| choice.config == snapshot.model)
            .unwrap_or(0);
        self.show_model_choices(console, chat, provider, choices, selected, Uuid::new_v4())
    }

    pub(super) fn show_model_choices(
        &mut self,
        console: &Console,
        chat: Uuid,
        provider: ProviderModels,
        choices: Vec<ModelChoice>,
        selected: usize,
        id: Uuid,
    ) -> Result<(), String> {
        let mut options: Vec<String> = choices
            .iter()
            .map(|choice| {
                let thinking = if thinking_choices(&choice.config).len() == 1 {
                    "unavailable"
                } else {
                    choice.thinking.map_or_else(
                        || {
                            if choice.config.adapter() == Adapter::MinimaxMessages {
                                if choice
                                    .config
                                    .card
                                    .as_ref()
                                    .is_some_and(|card| card.adaptive_by_default)
                                {
                                    "max (default)"
                                } else {
                                    "off (default)"
                                }
                            } else {
                                "auto"
                            }
                        },
                        ReasoningEffort::label,
                    )
                };
                let name = tickloop_agent::model::access::AccessCatalog::defaults()
                    .series_for(&choice.config)
                    .map_or_else(|| choice.config.name.clone(), |series| series.label());
                let status = match &choice.availability {
                    ModelAvailability::Unavailable { reason } => {
                        format!(" · Unavailable: {reason}")
                    }
                    ModelAvailability::Unknown { .. }
                        if choices.iter().any(|choice| {
                            matches!(choice.availability, ModelAvailability::DirectoryListed)
                        }) =>
                    {
                        " · Not verified".into()
                    }
                    _ => String::new(),
                };
                safe_text(&format!("{name}    Thinking ‹ {thinking} ›{status}"))
            })
            .collect();
        if provider.models[0]
            .connection()
            .is_some_and(|connection| connection.discovery != ModelDiscovery::Unsupported)
        {
            options.push("Refresh account model directory".into());
        }
        let mut context = vec![
            "Enter saves the model and Thinking. Esc discards changes.".into(),
            self.account_directory_status.clone(),
        ];
        if let Some(notice) = tickloop_agent::model::access::AccessCatalog::defaults()
            .provider(&provider.models[0].provider_id)
            .and_then(|access| access.notice.as_deref())
        {
            context.push(safe_text(notice));
        }
        self.show(
            console,
            Menu {
                id,
                title: safe_text(&format!("Choose model · {}", provider.name())),
                context,
                options,
                selected,
                confirmation: false,
            },
            Selection::Model {
                chat,
                provider,
                choices,
            },
        )
    }

    pub(in crate::terminal) fn picker_action(
        &mut self,
        console: &Console,
        menu_id: Uuid,
        index: usize,
        action: PickerAction,
    ) -> Result<MenuOutcome, String> {
        if self.id() != Some(menu_id) {
            return Ok(MenuOutcome::default());
        }
        let (_, selection) = self.active.take().unwrap();
        match selection {
            Selection::Provider {
                chat,
                choices,
                rows,
            } => {
                if let Some((_, selected)) = &mut self.provider_group {
                    *selected = index;
                } else {
                    self.provider_selected = index;
                }
                if index >= rows.len() {
                    self.show_providers(console, chat, choices, index, menu_id)?;
                    return Ok(MenuOutcome::default());
                }
                let provider_index = match rows[index].clone() {
                    ProviderRow::Group(name) => {
                        self.provider_group = Some((name, 0));
                        self.show_providers(console, chat, choices, 0, menu_id)?;
                        return Ok(MenuOutcome::default());
                    }
                    ProviderRow::Provider(provider_index) => provider_index,
                };
                let provider = choices
                    .get(provider_index)
                    .ok_or("provider option does not exist")?;
                match action {
                    PickerAction::Reauthenticate
                    | PickerAction::DeviceLogin
                    | PickerAction::SignOut => {
                        let provider = provider.clone();
                        self.provider_choices = choices.clone();
                        // Keep the selection recoverable if clearing a credential fails.
                        self.show_providers(console, chat, choices, index, menu_id)?;
                        if let Some(target) =
                            SignInTarget::device_only(provider.models[0].auth_source)
                        {
                            if matches!(
                                action,
                                PickerAction::Reauthenticate | PickerAction::SignOut
                            ) {
                                self.dismiss(console)?;
                                self.login_return = Some((chat, provider));
                                return Ok(MenuOutcome {
                                    provider_logout: true,
                                    logout_target: target,
                                    reauthenticate_after_logout: action
                                        == PickerAction::Reauthenticate,
                                    ..Default::default()
                                });
                            }
                            return self.begin_provider_login(
                                console,
                                chat,
                                provider,
                                target.login_method(),
                            );
                        }
                        if provider.models[0].auth_source == ModelAuthSource::CodexSubscription {
                            if matches!(
                                action,
                                PickerAction::Reauthenticate | PickerAction::SignOut
                            ) {
                                self.dismiss(console)?;
                                self.login_return = Some((chat, provider));
                                return Ok(MenuOutcome {
                                    provider_logout: true,
                                    logout_target: SignInTarget::Codex,
                                    reauthenticate_after_logout: action
                                        == PickerAction::Reauthenticate,
                                    ..Default::default()
                                });
                            }
                            return self.begin_provider_login(
                                console,
                                chat,
                                provider,
                                super::super::login::LoginMethod::Device,
                            );
                        }
                        if action == PickerAction::SignOut {
                            return Ok(MenuOutcome {
                                status: Some("Sign out applies to OAuth login. API keys are managed with Reauthenticate or in the Agent environment.".into()),
                                ..Default::default()
                            });
                        }
                        if action == PickerAction::Reauthenticate
                            && !provider.models[0].api_key_env.is_empty()
                        {
                            let root = self
                                .config_root
                                .as_deref()
                                .ok_or("Cannot locate the Agent state directory.")?;
                            tickloop_agent::model::credentials::remove_api_key(
                                root,
                                &provider.models[0].api_key_env,
                            )
                            .map_err(|error| error.to_string())?;
                            self.provider_api_key(console, chat, provider)?;
                        }
                        return Ok(MenuOutcome::default());
                    }
                    _ => {}
                }
                self.provider_choices = choices.clone();
                self.show_providers(console, chat, choices, index, menu_id)?;
            }
            Selection::Model {
                chat,
                provider,
                mut choices,
            } => {
                if let Some(choice) = choices.get_mut(index) {
                    let levels = thinking_choices(&choice.config);
                    let position = levels
                        .iter()
                        .position(|level| *level == choice.thinking)
                        .unwrap_or(0);
                    let position = match action {
                        PickerAction::ThinkingPrevious => position.saturating_sub(1),
                        PickerAction::ThinkingNext => (position + 1).min(levels.len() - 1),
                        _ => position,
                    };
                    choice.thinking = levels[position];
                }
                self.show_model_choices(console, chat, provider, choices, index, menu_id)?;
            }
            other => self.active = Some((menu_id, other)),
        }
        Ok(MenuOutcome::default())
    }

    pub(super) fn provider_auth(
        &mut self,
        console: &Console,
        chat: Uuid,
        provider: ProviderModels,
    ) -> Result<MenuOutcome, String> {
        if let Some(target) = SignInTarget::device_only(provider.models[0].auth_source) {
            return self.begin_provider_login(console, chat, provider, target.login_method());
        }
        if provider.models[0].auth_source == ModelAuthSource::CodexSubscription {
            return self.begin_provider_login(
                console,
                chat,
                provider,
                super::super::login::LoginMethod::Browser,
            );
        }
        self.provider_api_key(console, chat, provider)?;
        Ok(MenuOutcome::default())
    }

    pub(super) fn begin_provider_login(
        &mut self,
        console: &Console,
        chat: Uuid,
        provider: ProviderModels,
        method: super::super::login::LoginMethod,
    ) -> Result<MenuOutcome, String> {
        self.dismiss(console)?;
        self.login_return = Some((chat, provider));
        Ok(MenuOutcome {
            provider_login: Some(method),
            ..Default::default()
        })
    }

    pub(super) fn provider_api_key(
        &mut self,
        console: &Console,
        chat: Uuid,
        provider: ProviderModels,
    ) -> Result<(), String> {
        let root = self
            .config_root
            .as_deref()
            .ok_or("Cannot locate the Agent state directory; set TICKLOOP_CONFIG_DIR.")?;
        if !console.is_interactive() {
            return Err(format!(
                "API key entry requires an interactive terminal. Configure {} in the Agent .env file.",
                provider.models[0].api_key_env
            ));
        }
        let menu = Menu {
            id: Uuid::new_v4(),
            title: safe_text(&format!("API key · {}", provider.name())),
            context: vec![
                safe_text(&format!(
                    "Saved as {} in {}. The saved key takes precedence over the shell environment.",
                    provider.models[0].api_key_env,
                    root.join(".env").display()
                )),
                "Enter your API key. Its contents stay hidden.".into(),
            ],
            options: vec!["Save API key".into(), "Cancel".into()],
            selected: 0,
            confirmation: false,
        };
        self.dismiss(console)?;
        let id = menu.id;
        console.show_secret_menu(menu)?;
        self.active = Some((id, Selection::ApiKey { chat, provider }));
        Ok(())
    }

    pub(in crate::terminal) async fn resume_provider_login(
        &mut self,
        agent: &Agent,
        console: &Console,
        succeeded: bool,
    ) -> Result<(), String> {
        if let Some((chat, provider)) = self.login_return.take() {
            if succeeded {
                self.model_discovery
                    .invalidate(&provider.models[0].provider_id);
                self.provider_models(agent, console, chat, provider).await?;
            } else {
                self.back_to_providers(console, chat)?;
            }
        }
        Ok(())
    }
    pub(in crate::terminal) fn discovery_active(&self) -> bool {
        self.discovery_flow.is_some()
    }

    pub(in crate::terminal) async fn next_discovery(
        &mut self,
    ) -> Option<Result<AccountModelDirectory, String>> {
        self.discovery_flow.as_mut()?.receiver.recv().await
    }

    pub(in crate::terminal) async fn finish_discovery(
        &mut self,
        agent: &Agent,
        console: &Console,
        result: Option<Result<AccountModelDirectory, String>>,
    ) -> Result<(), String> {
        let Some(flow) = self.discovery_flow.take() else {
            return Ok(());
        };
        if self.id() != Some(flow.id) {
            return Ok(());
        }
        self.provider_model_choices(
            agent,
            console,
            flow.chat,
            flow.provider.clone(),
            result.unwrap_or_else(|| Err("Model discovery ended without a result".into())),
        )
        .await
    }

    pub(in crate::terminal) fn invalidate_model_discovery(&mut self) {
        if let Some((_, provider)) = &self.login_return {
            self.model_discovery
                .invalidate(&provider.models[0].provider_id);
        }
    }
}

pub(super) struct DiscoveryFlow {
    id: Uuid,
    chat: Uuid,
    provider: ProviderModels,
    cancel: tokio_util::sync::CancellationToken,
    task: tokio::task::JoinHandle<()>,
    receiver: tokio::sync::mpsc::UnboundedReceiver<Result<AccountModelDirectory, String>>,
}

impl Drop for DiscoveryFlow {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.task.abort();
    }
}
