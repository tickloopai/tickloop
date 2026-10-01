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
pub(in crate::terminal) struct ProviderModels {
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
            "Copilot subscription (api.individual.githubcopilot.com)"
        );
        assert_eq!(choices[*index].models.len(), 8);
        assert_eq!(
            SignInTarget::from_auth(choices[*index].models[0].auth_source),
            Some(SignInTarget::Copilot)
        );
    }

    #[test]
    fn capability_order_is_stable_and_separate_from_default_recommendations() {
        let mut profiles = tickloop_agent::model::cards::built_in_model_profiles();
        let forward = ProviderModels::group(profiles.clone());
        profiles.reverse();
        let reversed = ProviderModels::group(profiles);
        let identities = |providers: Vec<ProviderModels>| {
            providers
                .into_iter()
                .map(|provider| {
                    (
                        provider.models[0].provider_id.clone(),
                        provider
                            .models
                            .iter()
                            .map(|model| model.model_id.clone())
                            .collect::<Vec<_>>(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(identities(forward.clone()), identities(reversed));
        let deepseek = forward
            .iter()
            .find(|provider| provider.models[0].provider_id == "deepseek-api")
            .unwrap();
        let access = tickloop_agent::model::access::AccessCatalog::defaults();
        assert_eq!(deepseek.models[0].model_id, "deepseek-v4-pro");
        assert_eq!(
            access.recommended_model("deepseek-api"),
            Some("deepseek-flash")
        );
        let copilot = forward
            .iter()
            .find(|provider| provider.models[0].provider_id == "github-copilot")
            .unwrap();
        assert_eq!(
            copilot
                .models
                .iter()
                .map(|model| model.model_id.as_str())
                .collect::<Vec<_>>(),
            [
                "gpt-6-astra",
                "gpt-6.1-sol",
                "gpt-5.6-terra",
                "gpt-6-luna",
                "claude-opus-5.5",
                "claude-fable-5.1",
                "claude-sonnet-5.5",
                "claude-haiku-4.5"
            ]
        );
        assert_eq!(
            access.recommended_model("github-copilot"),
            Some("claude-sonnet-5.5")
        );
    }

    #[test]
    fn codex_menu_uses_current_families_and_keeps_retired_cards_loadable() {
        let catalog = tickloop_agent::model::cards::ModelCatalog::defaults();
        let retired = catalog.get("openai-codex", "gpt-5.5-responses").unwrap();
        let old_sol = catalog
            .get("openai-codex", "gpt-5.6-sol-responses")
            .unwrap();
        let profiles = super::super::super::settings::model_choices_with_profiles(
            &old_sol,
            tickloop_agent::model::cards::built_in_model_profiles(),
        );
        let grouped = ProviderModels::group(profiles);
        let codex = grouped
            .iter()
            .find(|provider| provider.models[0].provider_id == "openai-codex")
            .unwrap();
        assert_eq!(
            codex
                .models
                .iter()
                .map(|model| model.name.as_str())
                .collect::<Vec<_>>(),
            ["GPT-6 Astra", "GPT-6.1 Sol", "GPT-5.6 Terra", "GPT-6 Luna"]
        );
        assert!(!codex.models.contains(&old_sol));
        assert!(!codex.models.contains(&retired));
        assert_eq!(
            tickloop_agent::model::access::AccessCatalog::defaults()
                .recommended_model("openai-codex"),
            Some("gpt-6.1-sol")
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
        assert!(choices.contains(&current));
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

pub(super) fn provider_group(provider: &ProviderModels) -> Option<&str> {
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
    fn model_page_name(&self) -> String {
        match provider_group(self) {
            Some(vendor) => format!("{vendor} · {}", self.name()),
            None => self.name(),
        }
    }

    fn supports_discovery(&self) -> bool {
        self.models[0]
            .connection()
            .is_some_and(|connection| connection.discovery != ModelDiscovery::Unsupported)
    }

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
        const VENDOR_ORDER: &[&str] = &[
            "DeepSeek",
            "OpenAI",
            "MiniMax",
            "Zhipu GLM",
            "GitHub Copilot",
            "Anthropic",
            "Kimi",
            "Alibaba Bailian",
            "Google",
            "xAI",
        ];
        let mut vendors: Vec<String> = VENDOR_ORDER
            .iter()
            .map(|vendor| (*vendor).to_owned())
            .collect();
        for provider in &choices {
            let vendor = provider_group(provider).unwrap_or("");
            if !vendors.iter().any(|known| known == vendor) {
                vendors.push(vendor.to_owned());
            }
        }
        choices.sort_by_key(|provider| {
            let vendor = provider_group(provider).unwrap_or("");
            (
                vendors
                    .iter()
                    .position(|known| known == vendor)
                    .unwrap_or(usize::MAX),
                provider.models[0]
                    .connection()
                    .map(|connection| connection.sort_key()),
            )
        });
        let access = tickloop_agent::model::access::AccessCatalog::defaults();
        for provider in &mut choices {
            provider.models.sort_by_key(|model| {
                (
                    access
                        .series_for(model)
                        .and_then(|series| series.family_order)
                        .unwrap_or(u16::MAX),
                    access.capability_order(model).unwrap_or(u16::MAX),
                    access.series_position(model).unwrap_or(usize::MAX),
                )
            });
        }
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
        target: ConfigTarget,
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
                title: group.map_or_else(
                    || "Choose Provider".into(),
                    |name| format!("Choose connection · {name}"),
                ),
                context: vec![if group.is_some() {
                    "Choose the product and service connection. Authentication is shown separately."
                        .into()
                } else {
                    "Configure connections and credentials. This does not change the Chat model."
                        .into()
                }],
                options,
                selected,
                confirmation: false,
            },
            Selection::Provider {
                target,
                choices,
                rows,
            },
        )
    }

    pub(super) fn back_to_providers(
        &mut self,
        console: &Console,
        target: ConfigTarget,
    ) -> Result<(), String> {
        self.show_providers(
            console,
            target,
            self.provider_choices.clone(),
            self.provider_group
                .as_ref()
                .map_or(self.provider_selected, |(_, selected)| *selected),
            Uuid::new_v4(),
        )
    }

    fn provider_auth_label(&self, provider: &ProviderModels) -> &'static str {
        let config = &provider.models[0];
        if config.auth_source == ModelAuthSource::GoogleCloud {
            return match self.provider_authenticated(provider) {
                Ok(true) => "Service account configured",
                Ok(false) => "Service account required",
                Err(_) => "Service account needs attention",
            };
        }
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
        if matches!(
            config.auth_source,
            ModelAuthSource::GoogleOAuth | ModelAuthSource::GoogleCloud
        ) {
            let Some(root) = self.config_root.as_deref() else {
                return Ok(false);
            };
            let auth = crate::auth::google::GoogleAuth::new(root);
            return if config.auth_source == ModelAuthSource::GoogleCloud {
                auth.service_account_configured()
            } else {
                auth.has_saved_login()
            }
            .map_err(|error| error.to_string());
        }
        if let Some(kind) =
            crate::auth::device_subscriptions::DeviceSubscription::from_auth(config.auth_source)
        {
            let Some(root) = self.config_root.as_deref() else {
                return Ok(false);
            };
            return crate::auth::device_subscriptions::SubscriptionAuth::new(root, kind)
                .has_saved_login()
                .map_err(|error| error.to_string());
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

    pub(in crate::terminal) fn warm_model_directory(&mut self, config: ModelConfig) {
        let provider = ProviderModels {
            models: vec![config.clone()],
        };
        if !provider.supports_discovery()
            || !self.provider_authenticated(&provider).unwrap_or(false)
        {
            return;
        }
        let cache = self.model_discovery.clone();
        let root = self.config_root.clone();
        let provider_id = config.provider_id.clone();
        let task = tokio::spawn(async move {
            let _ = cache
                .discover(
                    &config,
                    root.as_deref(),
                    false,
                    &tokio_util::sync::CancellationToken::new(),
                )
                .await;
        });
        self.model_warmup = Some(ModelWarmup {
            provider: provider_id,
            task,
        });
    }

    pub(in crate::terminal) fn invalidate_failed_directory(
        &self,
        snapshot: &ChatSnapshot,
        run: Uuid,
    ) {
        use tickloop_agent::runtime::{RequestFailure, RequestOutcome};
        let Some(request) = snapshot
            .request_usage
            .entries()
            .iter()
            .rev()
            .find(|request| request.run_id == Some(run))
        else {
            return;
        };
        if !matches!(
            request.outcome,
            RequestOutcome::Failed(
                RequestFailure::Authentication
                    | RequestFailure::Http {
                        status: 401 | 402 | 403 | 404 | 429
                    }
            )
        ) {
            return;
        }
        if request.model == snapshot.model.identity() {
            self.model_discovery.invalidate(&snapshot.model.provider_id);
        } else {
            for provider in &self.provider_choices {
                if provider
                    .models
                    .iter()
                    .any(|model| model.identity() == request.model)
                {
                    self.model_discovery
                        .invalidate(&provider.models[0].provider_id);
                }
            }
        }
    }

    pub(super) async fn configured_models(
        &mut self,
        agent: &Agent,
        console: &Console,
        target: ConfigTarget,
    ) -> Result<(), String> {
        let providers: Vec<_> = self
            .provider_choices
            .iter()
            .filter(|provider| self.provider_authenticated(provider).unwrap_or(false))
            .cloned()
            .collect();
        let selected_provider = self.model_return.as_ref().map_or_else(
            || {
                self.current_model
                    .as_ref()
                    .map(|(provider, _)| provider.clone())
            },
            |draft| draft.provider.clone(),
        );
        if !providers.iter().any(|provider| {
            Some(&provider.models[0].provider_id) == selected_provider.as_ref()
                && provider.supports_discovery()
        }) {
            let directories = providers
                .into_iter()
                .map(|provider| (provider, Err("Account model check not requested".into())))
                .collect();
            return self
                .configured_model_choices(agent, console, target, directories)
                .await;
        }
        let id = Uuid::new_v4();
        self.show(
            console,
            Menu {
                id,
                title: "Discovering account models".into(),
                context: vec!["Checking this Provider.".into()],
                options: vec!["Skip account check".into()],
                selected: 0,
                confirmation: false,
            },
            Selection::Discovery { target },
        )?;
        let root = self.config_root.clone();
        let cache = self.model_discovery.clone();
        let cancel = tokio_util::sync::CancellationToken::new();
        let task_cancel = cancel.clone();
        let task_providers = providers.clone();
        let warmup = if self
            .model_warmup
            .as_ref()
            .is_some_and(|warmup| Some(&warmup.provider) == selected_provider.as_ref())
        {
            self.model_warmup.take()
        } else {
            None
        };

        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            if let Some(mut warmup) = warmup {
                let _ = (&mut warmup.task).await;
            }

            let mut directories = Vec::new();
            for provider in task_providers {
                let result = if Some(&provider.models[0].provider_id) == selected_provider.as_ref()
                    && provider.supports_discovery()
                {
                    cache
                        .discover(&provider.models[0], root.as_deref(), false, &task_cancel)
                        .await
                } else {
                    Err("Account model check not requested".into())
                };
                directories.push((provider, result));
            }
            let _ = sender.send(directories);
        });
        self.discovery_flow = Some(DiscoveryFlow {
            id,
            target,
            providers,
            cancel,
            task,
            receiver,
        });
        Ok(())
    }

    pub(super) async fn configured_model_choices(
        &mut self,
        agent: &Agent,
        console: &Console,
        target: ConfigTarget,
        directories: ModelDirectories,
    ) -> Result<(), String> {
        let snapshot = agent
            .configuration(target)
            .await
            .map_err(|error| error.to_string())?;
        self.current_model = Some((
            snapshot.model.provider_id.clone(),
            snapshot.model.model_id.clone(),
        ));
        self.model_directory_status = directories
            .iter()
            .map(|(provider, directory)| {
                (
                    provider.models[0].provider_id.clone(),
                    directory_status(&provider.models[0], directory),
                )
            })
            .collect();
        let preferred = if snapshot.thinking_enabled {
            snapshot.reasoning_effort
        } else {
            Some(ReasoningEffort::Off)
        };
        self.model_providers = directories
            .iter()
            .map(|(provider, _)| provider.clone())
            .collect();
        let mut choices: Vec<ModelChoice> = directories
            .into_iter()
            .flat_map(|(provider, directory)| {
                provider.models.into_iter().map(move |model| ModelChoice {
                    availability: directory.as_ref().map_or_else(
                        |error| ModelAvailability::Unknown {
                            reason: format!("Unverified · {error}"),
                        },
                        |directory| directory.availability(&model.model),
                    ),
                    thinking: if thinking_choices(&model).contains(&preferred) {
                        preferred
                    } else {
                        None
                    },
                    config: model,
                })
            })
            .filter(|choice| choice.availability.selectable())
            .collect();
        let draft = self.model_return.take();
        if let Some(draft) = &draft {
            for choice in &mut choices {
                if let Some(previous) = draft
                    .choices
                    .iter()
                    .find(|previous| same_model(&previous.config, &choice.config))
                    && thinking_choices(&choice.config).contains(&previous.thinking)
                {
                    choice.thinking = previous.thinking;
                }
            }
        }
        let focus = draft
            .as_ref()
            .and_then(|draft| draft.selected.as_ref())
            .or(self.current_model.as_ref())
            .cloned();
        self.model_choices = choices;
        let page_provider = match draft {
            Some(draft) => draft.provider,
            None => self
                .current_model
                .as_ref()
                .map(|(provider, _)| provider.clone()),
        };
        if let Some(provider) = page_provider
            && self
                .model_providers
                .iter()
                .any(|candidate| candidate.models[0].provider_id == provider)
        {
            return self.show_provider_models(console, target, &provider, focus.as_ref());
        }
        self.show_model_providers(
            console,
            target,
            focus.as_ref().map(|(provider, _)| provider.as_str()),
        )
    }

    pub(super) fn remember_model_drafts(&mut self, choices: &[ModelChoice]) {
        for choice in choices {
            if let Some(saved) = self
                .model_choices
                .iter_mut()
                .find(|saved| same_model(&saved.config, &choice.config))
            {
                saved.thinking = choice.thinking;
            }
        }
    }

    pub(super) fn show_model_providers(
        &mut self,
        console: &Console,
        target: ConfigTarget,
        focused: Option<&str>,
    ) -> Result<(), String> {
        self.model_provider = None;
        let mut options: Vec<_> = self
            .model_providers
            .iter()
            .map(|provider| {
                let current = if self
                    .current_model
                    .as_ref()
                    .is_some_and(|(id, _)| *id == provider.models[0].provider_id)
                {
                    "●"
                } else {
                    " "
                };
                safe_text(&format!("{current} {}", provider.model_page_name()))
            })
            .collect();
        options.push("Configure Providers".into());
        let selected = self
            .model_providers
            .iter()
            .position(|provider| Some(provider.models[0].provider_id.as_str()) == focused)
            .unwrap_or(0);
        let mut context = Vec::new();
        if self.model_providers.is_empty() {
            context.push("No Providers configured. Press p to configure a connection.".into());
        }
        self.show(
            console,
            Menu {
                id: Uuid::new_v4(),
                title: "Choose model Provider".into(),
                context,
                options,
                selected,
                confirmation: false,
            },
            Selection::ModelProviders { target },
        )
    }

    pub(super) fn show_provider_models(
        &mut self,
        console: &Console,
        target: ConfigTarget,
        provider: &str,
        focused: Option<&(String, String)>,
    ) -> Result<(), String> {
        let choices: Vec<_> = self
            .model_choices
            .iter()
            .filter(|choice| choice.config.provider_id == provider)
            .cloned()
            .collect();
        let focused = focused.or(self.current_model.as_ref());
        let selected = choices
            .iter()
            .position(|choice| {
                focused.is_some_and(|(id, model)| {
                    choice.config.provider_id == *id && choice.config.model_id == *model
                })
            })
            .unwrap_or(0);
        self.model_provider = Some(provider.into());
        self.show_model_choices(console, target, choices, selected, Uuid::new_v4())
    }

    pub(super) async fn refresh_model_choices(
        &mut self,
        agent: &Agent,
        console: &Console,
        target: ConfigTarget,
    ) -> Result<(), String> {
        if let Some(provider) = &self.model_provider {
            self.model_discovery.invalidate(provider);
        }
        self.model_return = Some(ModelPickerDraft {
            selected: self
                .model_provider
                .as_ref()
                .map(|provider| (provider.clone(), String::new())),
            choices: self.model_choices.clone(),
            provider: self.model_provider.clone(),
        });
        self.configured_models(agent, console, target).await
    }

    pub(super) fn open_model_provider_configuration(
        &mut self,
        console: &Console,
        target: ConfigTarget,
        provider: Option<&str>,
    ) -> Result<(), String> {
        self.model_return = Some(ModelPickerDraft {
            selected: provider.map(|provider| (provider.into(), String::new())),
            choices: self.model_choices.clone(),
            provider: self.model_provider.clone(),
        });
        self.provider_group = None;
        self.provider_selected = 0;
        if let Some(provider) = provider {
            self.focus_provider_configuration(provider);
        }
        self.back_to_providers(console, target)?;
        Ok(())
    }

    pub(super) async fn finish_provider_configuration(
        &mut self,
        agent: &Agent,
        console: &Console,
        target: ConfigTarget,
    ) -> Result<(), String> {
        if self.model_return.is_some() {
            self.configured_models(agent, console, target).await
        } else {
            self.back_to_providers(console, target)
        }
    }

    pub(super) fn open_model_providers(
        &mut self,
        console: &Console,
        target: ConfigTarget,
        choices: Vec<ModelChoice>,
        index: usize,
    ) -> Result<(), String> {
        self.remember_model_drafts(&choices);
        let provider = choices
            .get(index)
            .map(|choice| choice.config.provider_id.clone())
            .or_else(|| self.model_provider.clone());
        self.model_return = Some(ModelPickerDraft {
            selected: choices.get(index).map(|choice| {
                (
                    choice.config.provider_id.clone(),
                    choice.config.model_id.clone(),
                )
            }),
            choices: self.model_choices.clone(),
            provider: self.model_provider.clone(),
        });
        self.provider_group = None;
        self.provider_selected = 0;
        if let Some(provider) = provider {
            self.focus_provider_configuration(&provider);
        }
        self.back_to_providers(console, target)
    }

    fn focus_provider_configuration(&mut self, id: &str) {
        if let Some(provider) = self
            .provider_choices
            .iter()
            .find(|provider| provider.models[0].provider_id == id)
        {
            let group = provider_group(provider).map(str::to_owned);
            let rows = provider_rows(&self.provider_choices, group.as_deref());
            let selected = rows.iter().position(|row| matches!(row, ProviderRow::Provider(index) if self.provider_choices[*index].models[0].provider_id == id)).unwrap_or(0);
            if let Some(group) = group {
                self.provider_group = Some((group, selected));
            } else {
                self.provider_selected = selected;
            }
        }
    }

    pub(super) fn show_model_choices(
        &mut self,
        console: &Console,
        target: ConfigTarget,
        choices: Vec<ModelChoice>,
        selected: usize,
        id: Uuid,
    ) -> Result<(), String> {
        let mut options: Vec<String> = choices
            .iter()
            .map(|choice| {
                let thinking = (thinking_choices(&choice.config).len() > 1).then(|| {
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
                });
                let name = &choice.config.name;
                let current = if self
                    .current_model
                    .as_ref()
                    .is_some_and(|(provider, model)| {
                        choice.config.provider_id == *provider && choice.config.model_id == *model
                    }) {
                    "●"
                } else {
                    " "
                };
                safe_text(&thinking.map_or_else(
                    || format!("{current} {name}"),
                    |thinking| format!("{current} {name}    Thinking ‹ {thinking} ›"),
                ))
            })
            .collect();
        self.remember_model_drafts(&choices);
        let provider = self
            .model_providers
            .iter()
            .find(|provider| Some(&provider.models[0].provider_id) == self.model_provider.as_ref())
            .ok_or("model Provider does not exist")?;
        options.push("Configure Provider".into());
        if provider.supports_discovery() {
            options.push(super::super::console::REFRESH_ACCOUNT_MODELS_LABEL.into());
        }
        let mut context = Vec::new();
        if choices.is_empty() {
            context.push("No models available for this account. Configure the Provider or refresh account models.".into());
        }
        if let Some(status) = self
            .model_directory_status
            .get(&provider.models[0].provider_id)
        {
            context.push(status.clone());
        }
        if let Some(notice) = tickloop_agent::model::access::AccessCatalog::defaults()
            .provider(&provider.models[0].provider_id)
            .and_then(|access| access.notice.as_deref())
        {
            context.push(safe_text(notice));
        }
        let title = format!("Choose model · {}", provider.model_page_name());
        self.show(
            console,
            Menu {
                id,
                title: safe_text(&title),
                context,
                options,
                selected,
                confirmation: false,
            },
            Selection::Model { target, choices },
        )
    }

    pub(in crate::terminal) async fn picker_action(
        &mut self,
        agent: &Agent,
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
                target,
                choices,
                rows,
            } => {
                if let Some((_, selected)) = &mut self.provider_group {
                    *selected = index;
                } else {
                    self.provider_selected = index;
                }
                if index >= rows.len() {
                    self.show_providers(console, target, choices, index, menu_id)?;
                    return Ok(MenuOutcome::default());
                }
                let provider_index = match rows[index].clone() {
                    ProviderRow::Group(name) => {
                        self.provider_group = Some((name, 0));
                        self.show_providers(console, target, choices, 0, menu_id)?;
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
                        self.show_providers(console, target, choices, index, menu_id)?;
                        if let Some(sign_in) =
                            SignInTarget::from_auth(provider.models[0].auth_source)
                        {
                            if matches!(
                                action,
                                PickerAction::Reauthenticate | PickerAction::SignOut
                            ) {
                                self.dismiss(console)?;
                                self.login_return = Some((target, provider));
                                return Ok(MenuOutcome {
                                    provider_logout: true,
                                    logout_target: sign_in,
                                    reauthenticate_after_logout: action
                                        == PickerAction::Reauthenticate,
                                    ..Default::default()
                                });
                            }
                            return self.begin_provider_login(
                                console,
                                target,
                                provider,
                                sign_in.login_method(),
                            );
                        }
                        if provider.models[0].auth_source == ModelAuthSource::CodexSubscription {
                            if matches!(
                                action,
                                PickerAction::Reauthenticate | PickerAction::SignOut
                            ) {
                                self.dismiss(console)?;
                                self.login_return = Some((target, provider));
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
                                target,
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
                            self.provider_api_key(console, target, provider)?;
                        }
                        return Ok(MenuOutcome::default());
                    }
                    _ => {}
                }
                self.provider_choices = choices.clone();
                self.show_providers(console, target, choices, index, menu_id)?;
            }
            Selection::ModelProviders { target } => {
                if action == PickerAction::ConfigureProvider {
                    let provider = self
                        .model_providers
                        .get(index)
                        .map(|provider| provider.models[0].provider_id.clone());
                    self.open_model_provider_configuration(console, target, provider.as_deref())?;
                } else {
                    self.active = Some((menu_id, Selection::ModelProviders { target }));
                }
            }
            Selection::Model {
                target,
                mut choices,
            } => {
                if action == PickerAction::RefreshAccountModels {
                    self.remember_model_drafts(&choices);
                    self.refresh_model_choices(agent, console, target).await?;
                    return Ok(MenuOutcome::default());
                }
                if action == PickerAction::ConfigureProvider {
                    self.open_model_providers(console, target, choices, index)?;
                    return Ok(MenuOutcome::default());
                }
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
                self.show_model_choices(console, target, choices, index, menu_id)?;
            }
            other => self.active = Some((menu_id, other)),
        }
        Ok(MenuOutcome::default())
    }

    pub(super) fn provider_auth(
        &mut self,
        console: &Console,
        target: ConfigTarget,
        provider: ProviderModels,
    ) -> Result<MenuOutcome, String> {
        if provider.models[0].auth_source == ModelAuthSource::GoogleCloud {
            self.provider_service_account(console, target, provider, None)?;
            return Ok(MenuOutcome::default());
        }
        if let Some(sign_in) = SignInTarget::from_auth(provider.models[0].auth_source) {
            return self.begin_provider_login(console, target, provider, sign_in.login_method());
        }
        if provider.models[0].auth_source == ModelAuthSource::CodexSubscription {
            return self.begin_provider_login(
                console,
                target,
                provider,
                super::super::login::LoginMethod::Browser,
            );
        }
        if provider.models[0].api_key_env.is_empty() {
            self.back_to_providers(console, target)?;
            return Ok(MenuOutcome {
                status: Some("Local connection requires no credentials.".into()),
                ..Default::default()
            });
        }
        self.provider_api_key(console, target, provider)?;
        Ok(MenuOutcome::default())
    }

    pub(super) fn begin_provider_login(
        &mut self,
        console: &Console,
        target: ConfigTarget,
        provider: ProviderModels,
        method: super::super::login::LoginMethod,
    ) -> Result<MenuOutcome, String> {
        self.dismiss(console)?;
        self.login_return = Some((target, provider));
        Ok(MenuOutcome {
            provider_login: Some(method),
            ..Default::default()
        })
    }

    pub(super) fn provider_service_account(
        &mut self,
        console: &Console,
        target: ConfigTarget,
        provider: ProviderModels,
        entered_path: Option<&str>,
    ) -> Result<(), String> {
        let root = self
            .config_root
            .as_deref()
            .ok_or("Cannot locate the Agent state directory; set TICKLOOP_CONFIG_DIR.")?;
        let saved = crate::auth::google::GoogleAuth::new(root)
            .service_account_path()
            .map_err(|error| error.to_string())?;
        let menu = Menu {
            id: Uuid::new_v4(),
            title: "Service account · Google Vertex".into(),
            context: vec![
                "Enter the absolute path to your service-account JSON file on this computer."
                    .into(),
                "The file must be private (0600). Only its path is saved; the file stays in place."
                    .into(),
                "The project comes from the file unless overridden. Location defaults to global."
                    .into(),
            ],
            options: vec!["Save service account".into(), "Cancel".into()],
            selected: 0,
            confirmation: false,
        };
        self.dismiss(console)?;
        let id = menu.id;
        console.show_input_menu(
            menu,
            "JSON file path",
            entered_path.or(saved.as_deref()).unwrap_or(""),
        )?;
        self.active = Some((id, Selection::ServiceAccount { target, provider }));
        Ok(())
    }

    pub(super) fn provider_api_key(
        &mut self,
        console: &Console,
        target: ConfigTarget,
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
        self.active = Some((id, Selection::ApiKey { target, provider }));
        Ok(())
    }

    pub(in crate::terminal) async fn resume_provider_login(
        &mut self,
        agent: &Agent,
        console: &Console,
        succeeded: bool,
    ) -> Result<(), String> {
        if let Some((target, provider)) = self.login_return.take() {
            if succeeded {
                self.model_discovery
                    .invalidate(&provider.models[0].provider_id);
            }
            self.finish_provider_configuration(agent, console, target)
                .await?;
        }
        Ok(())
    }
    pub(in crate::terminal) fn discovery_active(&self) -> bool {
        self.discovery_flow.is_some()
    }

    pub(in crate::terminal) async fn next_discovery(&mut self) -> Option<ModelDirectories> {
        self.discovery_flow.as_mut()?.receiver.recv().await
    }

    pub(in crate::terminal) async fn finish_discovery(
        &mut self,
        agent: &Agent,
        console: &Console,
        result: Option<ModelDirectories>,
    ) -> Result<(), String> {
        let Some(flow) = self.discovery_flow.take() else {
            return Ok(());
        };
        if self.id() != Some(flow.id) {
            return Ok(());
        }
        self.configured_model_choices(
            agent,
            console,
            flow.target,
            result.unwrap_or_else(|| {
                flow.providers
                    .clone()
                    .into_iter()
                    .map(|provider| {
                        (
                            provider,
                            Err("Model discovery ended without a result".into()),
                        )
                    })
                    .collect()
            }),
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

pub(super) struct ModelWarmup {
    provider: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for ModelWarmup {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(super) struct DiscoveryFlow {
    id: Uuid,
    target: ConfigTarget,
    pub(super) providers: Vec<ProviderModels>,
    cancel: tokio_util::sync::CancellationToken,
    task: tokio::task::JoinHandle<()>,
    receiver: tokio::sync::mpsc::UnboundedReceiver<ModelDirectories>,
}

impl Drop for DiscoveryFlow {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.task.abort();
    }
}

type ModelDirectories = Vec<(ProviderModels, Result<AccountModelDirectory, String>)>;

pub(super) struct ModelPickerDraft {
    pub(super) provider: Option<String>,
    pub(super) selected: Option<(String, String)>,
    pub(super) choices: Vec<ModelChoice>,
}

fn same_model(left: &ModelConfig, right: &ModelConfig) -> bool {
    left.provider_id == right.provider_id && left.model_id == right.model_id
}

fn directory_status(
    config: &ModelConfig,
    directory: &Result<AccountModelDirectory, String>,
) -> String {
    match directory {
        Ok(directory) if directory.complete => "Account directory checked.".into(),
        Ok(_) => "Account directory is incomplete; missing models remain unverified.".into(),
        Err(_)
            if config
                .connection()
                .is_none_or(|connection| connection.discovery == ModelDiscovery::Unsupported) =>
        {
            "Account model access has not been checked.".into()
        }
        Err(error) if error.contains("cancelled") => {
            "Account model check skipped; access remains unverified.".into()
        }
        Err(error) if error.contains("timed out") => {
            "Account model check timed out. Use Refresh to retry.".into()
        }
        Err(error) if error.contains("HTTP ") => {
            let status = error
                .split("HTTP ")
                .nth(1)
                .unwrap_or("")
                .chars()
                .take(3)
                .collect::<String>();
            format!(
                "Account model check failed (HTTP {status}). Login remains saved; use Refresh to retry."
            )
        }
        Err(_) => {
            "Account model access could not be checked. Login remains saved; use Refresh to retry."
                .into()
        }
    }
}
