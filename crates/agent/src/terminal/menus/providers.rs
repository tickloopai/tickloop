use super::*;
use tickloop_agent::model::{
    ModelAuthSource,
    cards::Adapter,
    credentials::{ApiKeyStatus, api_key_status},
};

#[derive(Clone)]
pub(super) struct ProviderModels {
    pub(super) models: Vec<ModelConfig>,
    pub(super) adapter: Adapter,
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
                .filter(|row| matches!(row, ProviderRow::Group("MiniMax")))
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
}

#[derive(Clone, Copy)]
pub(super) enum ProviderRow {
    Provider(usize),
    Group(&'static str),
}

fn provider_group(provider: &ProviderModels) -> Option<&'static str> {
    crate::model::cards::is_minimax_provider(&provider.models[0].provider_id).then_some("MiniMax")
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
                rows.push(ProviderRow::Group(group));
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
                    adapter: model.adapter(),
                    models: vec![model],
                });
            }
        }
        choices
    }

    fn name(&self) -> &str {
        let model = &self.models[0];
        if model.provider_name.is_empty() {
            &model.provider_id
        } else {
            &model.provider_name
        }
    }

    pub(super) fn adapters(&self) -> Vec<Adapter> {
        let mut adapters = Vec::new();
        for model in &self.models {
            if !adapters.contains(&model.adapter()) {
                adapters.push(model.adapter());
            }
        }
        adapters
    }
}

fn adapter_label(adapter: Adapter) -> &'static str {
    match adapter {
        Adapter::SystemOne => "System One",
        Adapter::Messages => "Messages",
        Adapter::DeepseekMessages => "DeepSeek Messages",
        Adapter::MinimaxMessages => "MiniMax Messages",
        Adapter::Responses => "Responses",
        Adapter::CodexResponses => "Codex Responses",
        Adapter::ChatCompletions => "Chat Completions",
        Adapter::DeepseekChatCompletions => "DeepSeek Chat Completions",
    }
}

#[derive(Clone)]
pub(super) struct ModelChoice {
    pub(super) config: ModelConfig,
    pub(super) thinking: Option<ReasoningEffort>,
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
        let group = self.provider_group.map(|(name, _)| name);
        let rows = provider_rows(&choices, group);
        let mut options: Vec<String> = rows
            .iter()
            .map(|row| match row {
                ProviderRow::Group(name) => format!("{name} ›"),
                ProviderRow::Provider(index) => {
                    let provider = &choices[*index];
                    safe_text(&format!(
                        "{} · {} · {}",
                        provider.name(),
                        adapter_label(provider.adapter),
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
                    "Choose the region and authentication method. Esc returns to Providers.".into()
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
                .map_or(self.provider_selected, |(_, selected)| selected),
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
        let snapshot = agent
            .snapshot(chat)
            .await
            .map_err(|error| error.to_string())?;
        let choices: Vec<_> = provider
            .models
            .iter()
            .filter(|model| model.adapter() == provider.adapter)
            .map(|model| {
                let preferred = if snapshot.thinking_enabled {
                    snapshot.reasoning_effort
                } else {
                    Some(ReasoningEffort::Off)
                };
                ModelChoice {
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
        let options = choices
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
                safe_text(&format!(
                    "{}    Thinking ‹ {thinking} ›",
                    choice.config.name
                ))
            })
            .collect();
        let mut context = vec![
            format!("Adapter: {}", adapter_label(provider.adapter)),
            "Enter saves the model and Thinking. Esc discards changes.".into(),
        ];
        if provider.models[0].adapter() == Adapter::MinimaxMessages
            && provider.models[0].auth_source != ModelAuthSource::ApiKeyEnv
        {
            context.push("Flash Preview requires eligible M Plan access; availability is checked by MiniMax.".into());
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
                mut choices,
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
                let provider_index = match rows[index] {
                    ProviderRow::Group(name) => {
                        self.provider_group = Some((name, 0));
                        self.show_providers(console, chat, choices, 0, menu_id)?;
                        return Ok(MenuOutcome::default());
                    }
                    ProviderRow::Provider(provider_index) => provider_index,
                };
                let provider = choices
                    .get_mut(provider_index)
                    .ok_or("provider option does not exist")?;
                match action {
                    PickerAction::CycleAdapter => {
                        let adapters = provider.adapters();
                        let position = adapters
                            .iter()
                            .position(|adapter| *adapter == provider.adapter)
                            .unwrap_or(0);
                        provider.adapter = adapters[(position + 1) % adapters.len()];
                    }
                    PickerAction::Reauthenticate
                    | PickerAction::DeviceLogin
                    | PickerAction::SignOut => {
                        let provider = provider.clone();
                        self.provider_choices = choices.clone();
                        // Keep the selection recoverable if clearing a credential fails.
                        self.show_providers(console, chat, choices, index, menu_id)?;
                        if let Some(region) = crate::auth::minimax::MiniMaxRegion::from_auth(
                            provider.models[0].auth_source,
                        ) {
                            if matches!(
                                action,
                                PickerAction::Reauthenticate | PickerAction::SignOut
                            ) {
                                self.dismiss(console)?;
                                self.login_return = Some((chat, provider));
                                return Ok(MenuOutcome {
                                    provider_logout: true,
                                    logout_region: Some(region),
                                    reauthenticate_after_logout: action
                                        == PickerAction::Reauthenticate,
                                    ..Default::default()
                                });
                            }
                            return self.begin_provider_login(
                                console,
                                chat,
                                provider,
                                super::super::login::LoginMethod::MiniMax(region),
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
        if let Some(region) =
            crate::auth::minimax::MiniMaxRegion::from_auth(provider.models[0].auth_source)
        {
            return self.begin_provider_login(
                console,
                chat,
                provider,
                super::super::login::LoginMethod::MiniMax(region),
            );
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
                self.provider_models(agent, console, chat, provider).await?;
            } else {
                self.back_to_providers(console, chat)?;
            }
        }
        Ok(())
    }
}
