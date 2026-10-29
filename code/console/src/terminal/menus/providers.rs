use super::super::login::SignInTarget;
use super::super::style::Role;
use super::*;
use tickloop_harness::model::{ModelConfigExt, ReasoningEffortExt};
use tickloop_harness::protocol::{
    AccountModelDirectory, Adapter, ApiKeyStatus, ModelAuthSource, ModelAvailability,
    ModelDiscovery, ProviderCredentialScope,
};

#[derive(Clone)]
pub(in crate::terminal) struct ProviderModels {
    pub(super) models: Vec<ModelConfig>,
    connection_key: String,
    canonical_endpoints: Option<BTreeMap<Adapter, String>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn configuring_an_authenticated_provider_replaces_credentials_even_during_setup() {
        let mut model = tickloop_harness::model::cards::ModelCatalog::defaults()
            .get("deepseek-api", "deepseek-flash")
            .unwrap();
        model.api_key_env = "TICKLOOP_TEST_ONBOARDING_REPLACE_KEY".into();
        let provider = ProviderModels::group(vec![model.clone()]).remove(0);
        let mut menus = Menus::default();
        let mut scope = tickloop_harness::model::auth::current_provider_credential_scope(None);
        scope.environment_keys = vec![model.api_key_env];
        menus.provider.provider_scope = Some(scope);
        for reason in [ModelSetup::FirstRun, ModelSetup::RepairSaved] {
            menus.model_setup = Some(reason);
            assert_eq!(menus.provider_authenticated(&provider), Ok(true));
            assert_eq!(
                menus.configure_credential_intent(&provider),
                CredentialIntent::Replace
            );
        }
        menus
            .provider
            .provider_scope
            .as_mut()
            .unwrap()
            .environment_keys
            .clear();
        assert_eq!(menus.provider_authenticated(&provider), Ok(false));
        assert_eq!(
            menus.configure_credential_intent(&provider),
            CredentialIntent::Setup
        );
    }

    #[tokio::test]
    async fn configured_model_picker_does_not_mark_an_unselected_setup_fallback() {
        let model = tickloop_harness::model::cards::ModelCatalog::defaults()
            .get("deepseek-api", "deepseek-flash")
            .unwrap();
        let agent = Agent::new_embedded(
            model.clone(),
            std::sync::Arc::new(tickloop_harness::test_support::ScriptedModel::new(
                Vec::new(),
            )),
        );
        let directory = tempfile::tempdir().unwrap();
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let provider = ProviderModels::group(vec![model]).remove(0);
        for target in [ConfigTarget::NewDraft, ConfigTarget::Agent(chat)] {
            let console = Console::test_interactive_for_chat(chat);
            let mut menus = Menus {
                model_setup: Some(ModelSetup::FirstRun),
                ..Default::default()
            };
            menus.model_setup_created_chat(chat);
            menus.provider.provider_choices = vec![provider.clone()];
            // Both the initial directory and an account-check repaint re-read configuration.
            for _ in 0..2 {
                menus
                    .configured_model_choices(
                        &agent,
                        &console,
                        target,
                        vec![(provider.clone(), Err("Account check fixture".into()))],
                    )
                    .await
                    .unwrap();
                assert!(menus.provider.current_model.is_none());
                assert!(menus.provider.current_connection.is_none());
                assert!(!menus.provider.model_choices.is_empty());
                let menu = console.menu_snapshot(menus.id().unwrap()).unwrap().unwrap();
                assert!(menu.options.iter().all(|option| !option.contains('●')));
            }
        }
        agent.shutdown().await;
    }

    #[test]
    fn lightweight_glm_models_offer_auto_off_and_on_without_fictitious_effort_levels() {
        let catalog = tickloop_harness::model::cards::ModelCatalog::defaults();
        for provider in ["zhipu-cn-api", "zai-global-api"] {
            for model in ["glm-4.5-air", "glm-4.7-flash"] {
                let config = catalog.get(provider, model).unwrap();
                assert_eq!(
                    thinking_choices(&config),
                    vec![None, Some(ReasoningEffort::Off), Some(ReasoningEffort::On)]
                );
            }
        }
    }

    #[test]
    fn scoped_discovery_preserves_other_and_newer_refreshes_until_check_completion() {
        let catalog = tickloop_harness::model::cards::ModelCatalog::defaults();
        let providers = ProviderModels::group(vec![
            catalog.get("anthropic-api", "claude-opus-5").unwrap(),
            catalog.get("openai-api", "gpt-6.1-sol").unwrap(),
        ]);
        assert!(providers.iter().all(ProviderModels::supports_discovery));
        let first = providers[0].key();
        let second = providers[1].key();
        let target = ConfigTarget::Agent(Uuid::new_v4());
        let mut menus = Menus::default();
        menus
            .refresh_connections
            .insert((target.agent(), first.clone()), Uuid::new_v4());
        menus
            .refresh_connections
            .insert((target.agent(), second.clone()), Uuid::new_v4());
        let scoped = menus.discovery_refreshes(target, &providers, Some(&first));
        assert_eq!(
            scoped.keys().cloned().collect::<HashSet<_>>(),
            HashSet::from([first.clone()])
        );
        assert_eq!(
            menus.refresh_connections.len(),
            2,
            "Preparing a query must not consume invalidations"
        );
        menus.finish_discovery_refreshes(target, &scoped);
        assert!(
            !menus
                .refresh_connections
                .contains_key(&(target.agent(), first))
        );
        let grouped = menus.discovery_refreshes(target, &providers, None);
        assert_eq!(
            grouped.keys().cloned().collect::<HashSet<_>>(),
            HashSet::from([second.clone()])
        );
        let newer = Uuid::new_v4();
        menus
            .refresh_connections
            .insert((target.agent(), second.clone()), newer);
        menus.finish_discovery_refreshes(target, &grouped);
        assert_eq!(
            menus.refresh_connections.get(&(target.agent(), second)),
            Some(&newer)
        );
        let latest = menus.discovery_refreshes(target, &providers, None);
        menus.finish_discovery_refreshes(target, &latest);
        assert!(menus.refresh_connections.is_empty());
    }

    #[tokio::test]
    async fn cancelled_grouped_discovery_keeps_refreshes_for_unqueried_providers() {
        let (mut menus, console, cancel, chat) = pending_model_discovery();
        let catalog = tickloop_harness::model::cards::ModelCatalog::defaults();
        let providers = ProviderModels::group(vec![
            catalog.get("anthropic-api", "claude-opus-5").unwrap(),
            catalog.get("openai-api", "gpt-6.1-sol").unwrap(),
        ]);
        let target = ConfigTarget::Agent(chat);
        for provider in &providers {
            menus
                .refresh_connections
                .insert((target.agent(), provider.key()), Uuid::new_v4());
        }
        let requested = menus.discovery_refreshes(target, &providers, None);
        assert_eq!(requested.len(), 2);
        menus.dismiss(&console).unwrap();
        assert!(cancel.is_cancelled());
        for provider in &providers {
            assert!(
                menus
                    .refresh_connections
                    .contains_key(&(target.agent(), provider.key())),
                "Cancelled discovery must retain {} refresh",
                provider.key()
            );
        }
    }

    #[tokio::test]
    async fn provider_logout_confirmation_keeps_the_connection_surface_open() {
        let model = tickloop_harness::model::cards::ModelCatalog::defaults()
            .get("openai-codex", "gpt-6-astra")
            .unwrap();
        let agent = Agent::new_embedded(
            model.clone(),
            std::sync::Arc::new(tickloop_harness::test_support::ScriptedModel::new(
                Vec::new(),
            )),
        );
        let console = Console::test_interactive_for_chat(Uuid::new_v4());
        let mut menus = Menus::default();
        let provider = ProviderModels::group(vec![model]).remove(0);
        menus.provider.provider_choices = vec![provider.clone()];
        menus
            .provider
            .provider_nav
            .back_stack
            .push(ProviderPage::Details {
                connection: provider.key(),
                selected: 2,
            });
        let id = Uuid::new_v4();
        menus
            .show(
                &console,
                Menu {
                    id,
                    title: "Sign out fixture".into(),
                    context: Vec::new(),
                    markdown: None,
                    options: vec!["Cancel".into(), "Sign out".into()],
                    selected: 1,
                    confirmation: false,
                },
                Selection::ConnectionConfirm {
                    target: ConfigTarget::NewDraft,
                    provider,
                    action: ConnectionAction::SignOut,
                },
            )
            .unwrap();
        let result = menus
            .choose(&agent, &console, id, Some(1), None)
            .await
            .unwrap();
        assert!(result.provider_logout);
        assert!(matches!(
            menus.active,
            Some((_, Selection::ConnectionDetails { .. }))
        ));
        agent.shutdown().await;
    }

    fn pending_model_discovery() -> (Menus, Console, tokio_util::sync::CancellationToken, Uuid) {
        let chat = Uuid::new_v4();
        let console = Console::test_for_chat(chat);
        let model = tickloop_harness::model::cards::ModelCatalog::defaults()
            .get("deepseek-api", "deepseek-flash")
            .unwrap();
        let provider = ProviderModels::group(vec![model.clone()]).remove(0);
        let mut menus = Menus {
            provider: ProviderSession {
                provider_choices: vec![provider.clone()],
                model_providers: vec![provider.clone()],
                model_provider: Some(provider.key()),
                ..Default::default()
            },
            ..Default::default()
        };
        let id = Uuid::new_v4();
        menus
            .show_model_choices(
                &console,
                ConfigTarget::Agent(chat),
                vec![ModelChoice {
                    config: model,
                    thinking: None,
                    availability: ModelAvailability::Unknown {
                        reason: "checking".into(),
                    },
                }],
                0,
                id,
            )
            .unwrap();
        let (_, receiver) = tokio::sync::mpsc::unbounded_channel();
        let cancel = tokio_util::sync::CancellationToken::new();
        menus.provider.discovery_flow = Some(DiscoveryFlow {
            refreshes: HashMap::new(),
            id,
            target: ConfigTarget::Agent(chat),
            providers: vec![provider],
            cancel: cancel.clone(),
            task: tokio::spawn(std::future::pending()),
            receiver,
        });
        (menus, console, cancel, chat)
    }

    #[tokio::test]
    async fn model_discovery_survives_a_preempting_plan_request() {
        let (mut menus, console, cancel, chat) = pending_model_discovery();
        assert!(menus.discovery_active());
        let request = Uuid::new_v4();
        menus
            .workflow(
                &console,
                (
                    Menu {
                        id: request,
                        title: "Plan approval".into(),
                        context: Vec::new(),
                        markdown: None,
                        options: vec!["Approve".into(), "Cancel".into()],
                        selected: 1,
                        confirmation: true,
                    },
                    workflows::Workflow::Plan {
                        chat,
                        version: 1,
                        request: None,
                        markdown: String::new(),
                        approval_mode: tickloop_harness::protocol::ExecutionMode::Auto,
                    },
                ),
            )
            .unwrap();
        assert!(
            menus.provider.discovery_flow.is_some(),
            "A request must suspend, not cancel, pending discovery"
        );
        assert!(!cancel.is_cancelled());
        assert!(
            !menus.discovery_active(),
            "Hidden model menus must not consume discovery results"
        );
        assert_eq!(menus.id(), Some(request));
    }

    #[tokio::test]
    async fn dismissing_the_active_model_picker_cancels_its_own_account_check() {
        let (mut menus, console, cancel, _) = pending_model_discovery();
        assert!(menus.discovery_active());
        menus.dismiss(&console).unwrap();
        assert!(cancel.is_cancelled());
        assert!(menus.provider.discovery_flow.is_none());
    }

    #[tokio::test]
    async fn model_discovery_does_not_consume_a_result_for_another_menu() {
        let (mut menus, _, _, _) = pending_model_discovery();
        menus.provider.discovery_flow.as_mut().unwrap().id = Uuid::new_v4();
        assert!(
            !menus.discovery_active(),
            "Only the originating model menu may consume its result"
        );
    }

    #[tokio::test]
    async fn provider_navigation_and_pending_discovery_restore_for_their_agent() {
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let console = Console::test_interactive_for_chat(first);
        let catalog = tickloop_harness::model::cards::ModelCatalog::defaults();
        let model = catalog.get("deepseek-api", "deepseek-flash").unwrap();
        let provider = ProviderModels::group(vec![model.clone()]).remove(0);
        let key = provider.key();
        let first_menu = Uuid::new_v4();
        let mut menus = Menus::default();
        menus.sync_dialog_chat(&console).unwrap();
        menus.provider.provider_choices = vec![provider.clone()];
        menus.provider.model_providers = vec![provider.clone()];
        menus.provider.model_provider = Some(key.clone());
        menus.provider.provider_nav.filter = ConnectionFilter::Configured;
        menus
            .provider
            .provider_nav
            .back_stack
            .push(ProviderPage::Directory {
                query: String::new(),
                filter: ConnectionFilter::Configured,
                focused: Some(key.clone()),
            });
        menus
            .show_model_choices(
                &console,
                ConfigTarget::Agent(first),
                vec![ModelChoice {
                    config: model,
                    thinking: None,
                    availability: ModelAvailability::Unknown {
                        reason: "checking first account".into(),
                    },
                }],
                0,
                first_menu,
            )
            .unwrap();
        let (_, receiver) = tokio::sync::mpsc::unbounded_channel();
        let cancel = tokio_util::sync::CancellationToken::new();
        menus.provider.discovery_flow = Some(DiscoveryFlow {
            refreshes: HashMap::new(),
            id: first_menu,
            target: ConfigTarget::Agent(first),
            providers: vec![provider.clone()],
            cancel: cancel.clone(),
            task: tokio::spawn(std::future::pending()),
            receiver,
        });
        menus.provider.login_return = Some((ConfigTarget::Agent(first), provider));
        console.select_chat(Some(second)).unwrap();
        menus.sync_dialog_chat(&console).unwrap();
        assert!(
            menus.provider.model_provider.is_none(),
            "Another Agent cannot inherit this connection"
        );
        assert!(menus.provider.discovery_flow.is_none());
        assert!(menus.provider.login_return.is_none());
        assert!(
            !cancel.is_cancelled(),
            "Hidden discovery must survive the Agent switch"
        );
        menus.provider.provider_nav.filter = ConnectionFilter::All;
        menus.provider.current_connection = Some("second-agent-connection".into());
        console.select_chat(Some(first)).unwrap();
        menus.sync_dialog_chat(&console).unwrap();
        assert_eq!(menus.provider.model_provider, Some(key));
        assert_eq!(
            menus.provider.provider_nav.filter,
            ConnectionFilter::Configured
        );
        assert_eq!(menus.provider.provider_nav.back_stack.len(), 1);
        assert_eq!(
            menus.provider.discovery_flow.as_ref().unwrap().id,
            first_menu
        );
        assert_eq!(
            menus.provider.login_return.as_ref().unwrap().0,
            ConfigTarget::Agent(first)
        );
        assert_eq!(console.dialog_menu_id().unwrap(), Some(first_menu));
        assert!(menus.discovery_active());
    }

    #[test]
    fn provider_login_completion_waits_for_its_agent_and_request_to_return() {
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let console = Console::test_interactive_for_chat(first);
        let mut menus = Menus::default();
        menus.note_provider_login_result(Some(first), true);
        console.select_chat(Some(second)).unwrap();
        assert_eq!(menus.take_provider_login_result(&console).unwrap(), None);
        console.select_chat(Some(first)).unwrap();
        let request = Uuid::new_v4();
        console.set_request_layout(Some(request)).unwrap();
        console
            .show_focused_menu(Menu {
                id: request,
                title: "Question".into(),
                context: Vec::new(),
                markdown: None,
                options: vec!["Answer".into()],
                selected: 0,
                confirmation: false,
            })
            .unwrap();
        assert_eq!(menus.take_provider_login_result(&console).unwrap(), None);
        console.clear_menu(request).unwrap();
        assert_eq!(
            menus.take_provider_login_result(&console).unwrap(),
            Some(true)
        );
        assert_eq!(menus.take_provider_login_result(&console).unwrap(), None);
    }

    async fn pending_discovery_through_question_pages(multi_select: bool) {
        use tickloop_harness::protocol::{Question, QuestionOption};
        let directory = tempfile::tempdir().unwrap();
        let model = tickloop_harness::model::cards::ModelCatalog::defaults()
            .get("deepseek-api", "deepseek-flash")
            .unwrap();
        let agent = Agent::new_embedded(
            model.clone(),
            std::sync::Arc::new(tickloop_harness::test_support::ScriptedModel::new(vec![])),
        );
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let console = Console::test_interactive_for_chat(chat);
        let provider = ProviderModels::group(vec![model.clone()]).remove(0);
        let key = provider.key();
        let mut menus = Menus::default();
        menus.sync_dialog_chat(&console).unwrap();
        menus.provider.provider_choices = vec![provider.clone()];
        menus.provider.model_providers = vec![provider.clone()];
        menus.provider.model_provider = Some(key.clone());
        menus
            .provider
            .provider_nav
            .back_stack
            .push(ProviderPage::Directory {
                query: String::new(),
                filter: ConnectionFilter::Configured,
                focused: Some(key),
            });
        let model_menu = Uuid::new_v4();
        menus
            .show_model_choices(
                &console,
                ConfigTarget::Agent(chat),
                vec![ModelChoice {
                    config: model,
                    thinking: None,
                    availability: ModelAvailability::Unknown {
                        reason: "pending account check".into(),
                    },
                }],
                0,
                model_menu,
            )
            .unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        let task_cancel = cancel.clone();
        let (release, gate) = tokio::sync::oneshot::channel();
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let result_provider = provider.clone();
        let task = tokio::spawn(async move {
            tokio::select! {
                _ = task_cancel.cancelled() => {},
                _ = gate => { let _ = sender.send(vec![(result_provider, Err("original account check delivered".into()))]); }
            }
        });
        menus.provider.discovery_flow = Some(DiscoveryFlow {
            refreshes: HashMap::new(),
            id: model_menu,
            target: ConfigTarget::Agent(chat),
            providers: vec![provider],
            cancel: cancel.clone(),
            task,
            receiver,
        });
        let request = QuestionRequest {
            id: Uuid::new_v4(),
            chat_id: chat.into(),
            loop_id: Uuid::new_v4().into(),
            run_id: Uuid::new_v4().into(),
            call_id: "question-review-regression".into(),
            questions: (0..2)
                .map(|index| Question {
                    question: format!("Question {index}"),
                    header: format!("Step {index}"),
                    options: vec![
                        QuestionOption {
                            label: "First".into(),
                            description: "first answer".into(),
                            preview: None,
                        },
                        QuestionOption {
                            label: "Second".into(),
                            description: "second answer".into(),
                            preview: None,
                        },
                    ],
                    multi_select: index == 0 && multi_select,
                })
                .collect(),
        };
        menus
            .show_question(
                &console,
                QuestionFlow::new(request.clone(), "Question flow".into()),
            )
            .unwrap();
        assert!(!cancel.is_cancelled());
        menus
            .choose(&agent, &console, menus.id().unwrap(), Some(0), None)
            .await
            .unwrap();
        assert!(
            !cancel.is_cancelled(),
            "A question page transition must keep the suspended account check alive"
        );
        if multi_select {
            menus
                .choose(&agent, &console, menus.id().unwrap(), Some(1), None)
                .await
                .unwrap();
            menus
                .switch_question(&console, menus.id().unwrap(), true)
                .unwrap();
            menus
                .switch_question(&console, menus.id().unwrap(), false)
                .unwrap();
            menus
                .choose(&agent, &console, menus.id().unwrap(), Some(0), None)
                .await
                .unwrap();
            menus
                .choose(&agent, &console, menus.id().unwrap(), Some(3), None)
                .await
                .unwrap();
        }
        menus
            .choose(&agent, &console, menus.id().unwrap(), Some(0), None)
            .await
            .unwrap();
        menus
            .switch_question(&console, menus.id().unwrap(), false)
            .unwrap();
        menus
            .switch_question(&console, menus.id().unwrap(), true)
            .unwrap();
        assert!(!cancel.is_cancelled());
        assert!(!menus.discovery_active());
        release.send(()).unwrap();
        let question_menu = menus.id().unwrap();
        // Esc declines the question; this fixture's question is not pending in the Agent, so
        // the decline fails, and the dialog still gives way to the suspended picker.
        assert!(
            menus
                .choose(&agent, &console, question_menu, None, None)
                .await
                .is_err()
        );
        console.clear_menu(question_menu).unwrap();
        menus.sync_dialog_chat(&console).unwrap();
        assert_eq!(menus.id(), Some(model_menu));
        assert!(menus.discovery_active());
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(2), menus.next_discovery())
                .await
                .unwrap();
        menus
            .finish_discovery(&agent, &console, result)
            .await
            .unwrap();
        assert!(menus.provider.model_choices.iter().any(|choice| matches!(&choice.availability, ModelAvailability::Unknown { reason } if reason.contains("original account check delivered"))));
        assert!(matches!(menus.active, Some((_, Selection::Model { .. }))));
        assert!(menus.provider.discovery_flow.is_none());
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn suspended_account_discovery_survives_multi_step_question_pages() {
        pending_discovery_through_question_pages(false).await;
    }

    #[tokio::test]
    async fn suspended_account_discovery_survives_multi_select_toggles_tabs_and_advance() {
        pending_discovery_through_question_pages(true).await;
    }

    #[tokio::test]
    async fn escape_queued_before_a_directory_result_still_returns_to_its_parent() {
        let model = tickloop_harness::model::cards::ModelCatalog::defaults()
            .get("deepseek-api", "deepseek-flash")
            .unwrap();
        let agent = Agent::new_embedded(
            model.clone(),
            std::sync::Arc::new(tickloop_harness::test_support::ScriptedModel::new(vec![])),
        );
        let (console, _) = Console::start(false).unwrap();
        let mut menus = Menus::default();
        let provider = ProviderModels::group(vec![model.clone()]).remove(0);
        let key = provider.key();
        menus.provider.provider_choices = vec![provider.clone()];
        menus.provider.model_providers = vec![provider.clone()];
        menus.provider.model_provider = Some(key.clone());
        menus
            .provider
            .provider_nav
            .back_stack
            .push(ProviderPage::Directory {
                query: String::new(),
                filter: ConnectionFilter::All,
                focused: Some(key),
            });
        let before = Uuid::new_v4();
        menus
            .show_model_choices(
                &console,
                ConfigTarget::NewDraft,
                vec![ModelChoice {
                    config: model,
                    thinking: None,
                    availability: ModelAvailability::Unknown {
                        reason: "checking".into(),
                    },
                }],
                0,
                before,
            )
            .unwrap();
        let (_, receiver) = tokio::sync::mpsc::unbounded_channel();
        menus.provider.discovery_flow = Some(DiscoveryFlow {
            refreshes: HashMap::new(),
            id: before,
            target: ConfigTarget::NewDraft,
            providers: vec![provider.clone()],
            cancel: tokio_util::sync::CancellationToken::new(),
            task: tokio::spawn(async {}),
            receiver,
        });
        menus
            .finish_discovery(
                &agent,
                &console,
                Some(vec![(provider, Err("controlled result".into()))]),
            )
            .await
            .unwrap();
        let after = menus.id().unwrap();
        assert_ne!(before, after);
        assert!(matches!(menus.active, Some((_, Selection::Model { .. }))));
        menus
            .choose(&agent, &console, before, Some(0), None)
            .await
            .unwrap();
        assert_eq!(
            menus.id(),
            Some(after),
            "A stale model choice must still be rejected"
        );
        menus
            .choose(&agent, &console, before, None, None)
            .await
            .unwrap();
        assert!(
            matches!(menus.active, Some((_, Selection::Provider { .. }))),
            "A directory result must not swallow the Escape sent for the visible model page"
        );
        agent.shutdown().await;
    }

    #[test]
    fn directory_keeps_all_region_product_connections_in_metadata_order() {
        let mut profiles = tickloop_harness::model::cards::built_in_model_profiles();
        profiles.reverse();
        let choices = ProviderModels::group(profiles);
        let glm: Vec<_> = choices
            .iter()
            .filter(|provider| provider_group(provider) == Some("Zhipu GLM"))
            .map(|provider| provider.models[0].api_key_env.as_str())
            .collect();
        assert_eq!(
            glm,
            [
                "ZHIPU_API_KEY",
                "ZAI_API_KEY",
                "ZHIPU_CODING_API_KEY",
                "ZAI_CODING_API_KEY"
            ]
        );
        assert_eq!(
            choices
                .iter()
                .filter(|provider| provider_group(provider) == Some("MiniMax"))
                .count(),
            4
        );
        let alibaba: Vec<_> = choices
            .iter()
            .filter(|provider| provider_group(provider) == Some("Alibaba Bailian"))
            .map(|provider| provider.models[0].api_key_env.as_str())
            .collect();
        assert_eq!(alibaba, ["DASHSCOPE_API_KEY", "ALIBABA_TOKEN_PLAN_API_KEY"]);
        assert_eq!(
            choices
                .iter()
                .filter(|provider| provider_group(provider) == Some("OpenAI"))
                .count(),
            2
        );
    }

    #[test]
    fn current_card_with_custom_credential_and_endpoint_remains_a_separate_connection() {
        let profiles = tickloop_harness::model::cards::built_in_model_profiles();
        let mut current = profiles
            .iter()
            .find(|model| model.provider_id == "deepseek-api" && model.model_id.contains("flash"))
            .unwrap()
            .clone();
        current.api_key_env = "PICKER_CUSTOM_KEY".into();
        current.base_url = "http://127.0.0.1:43210/v1".into();
        let choices = ProviderModels::group(
            super::super::super::settings::model_choices_with_profiles(&current, profiles),
        );
        let custom = choices
            .iter()
            .find(|connection| connection.key() == connection_key(&current))
            .unwrap();
        assert_eq!(custom.models, [current.clone()]);
        assert!(custom.name().contains("Custom"));
        let default = choices
            .iter()
            .find(|connection| {
                connection.models[0].provider_id == "deepseek-api"
                    && connection.key() != custom.key()
            })
            .unwrap();
        assert_eq!(default.models[0].api_key_env, "DEEPSEEK_API_KEY");
        assert!(
            default
                .models
                .iter()
                .all(|model| model.base_url != current.base_url)
        );
    }

    #[test]
    fn kimi_connection_initial_focus_uses_the_recommendation_and_preserves_explicit_focus() {
        let profiles = tickloop_harness::model::cards::built_in_model_profiles()
            .into_iter()
            .filter(|model| model.provider_id == "kimi-code-oauth")
            .collect();
        let provider = ProviderModels::group(profiles).remove(0);
        assert_eq!(
            provider.models[0].model_id, "k3",
            "capability order should remain unchanged"
        );
        let key = provider.key();
        let console = Console::test_interactive_for_chat(Uuid::new_v4());
        let mut menus = Menus::default();
        menus.provider.provider_choices = vec![provider.clone()];
        menus.provider.model_providers = vec![provider.clone()];
        menus.provider.model_choices = provider
            .models
            .iter()
            .map(|model| ModelChoice {
                config: model.clone(),
                thinking: None,
                availability: ModelAvailability::Unknown {
                    reason: "Controlled directory fixture".into(),
                },
            })
            .collect();
        menus
            .show_provider_models(&console, ConfigTarget::NewDraft, &key, None, None)
            .unwrap();
        assert_eq!(
            menus.provider.focused_model,
            Some(("kimi-code-oauth".into(), "k3".into()))
        );
        menus.dismiss(&console).unwrap();
        menus
            .show_provider_models(
                &console,
                ConfigTarget::NewDraft,
                &key,
                Some(&("kimi-code-oauth".into(), "k3".into())),
                None,
            )
            .unwrap();
        assert_eq!(
            menus.provider.focused_model,
            Some(("kimi-code-oauth".into(), "k3".into()))
        );
    }

    #[test]
    fn connection_search_filters_rows_by_name_or_endpoint() {
        let catalog = tickloop_harness::model::cards::ModelCatalog::defaults();
        let choices = ProviderModels::group(vec![
            catalog.get("kimi-code-oauth", "k3").unwrap(),
            catalog.get("kimi-global-api", "kimi-k3").unwrap(),
            catalog.get("deepseek-api", "deepseek-flash").unwrap(),
        ]);
        let console = Console::test_interactive_for_chat(Uuid::new_v4());
        let mut menus = Menus::default();
        menus.provider.provider_choices = choices.clone();
        let id = Uuid::new_v4();
        let rows = |menus: &mut Menus| {
            menus
                .show_providers(&console, ConfigTarget::NewDraft, choices.clone(), 0, id)
                .unwrap();
            console
                .menu_snapshot(id)
                .unwrap()
                .expect("the directory is shown")
                .options
        };
        let all = rows(&mut menus);
        assert_eq!(
            all.len(),
            4,
            "three connections and the action row: {all:?}"
        );
        assert!(all[2].contains("Code subscription"), "{all:?}");

        menus.provider.provider_nav.query = "deepseek".into();
        let filtered = rows(&mut menus);
        assert_eq!(
            filtered.len(),
            2,
            "one match and the action row: {filtered:?}"
        );
        assert!(filtered[0].contains("api.deepseek.com"), "{filtered:?}");

        menus.provider.provider_nav.query = "kimi".into();
        let by_vendor = rows(&mut menus);
        assert_eq!(by_vendor.len(), 3, "both Kimi connections: {by_vendor:?}");
        assert!(by_vendor[0].contains("api.moonshot.ai"), "{by_vendor:?}");
        assert!(by_vendor[1].contains("api.kimi.com"), "{by_vendor:?}");

        menus.provider.provider_nav.query = "kimi.com".into();
        let by_endpoint = rows(&mut menus);
        assert_eq!(by_endpoint.len(), 2, "{by_endpoint:?}");
        assert!(by_endpoint[0].contains("api.kimi.com"), "{by_endpoint:?}");

        menus.provider.provider_nav.query = "nothing matches this".into();
        let empty = rows(&mut menus);
        assert_eq!(empty.len(), 2, "{empty:?}");
        assert!(empty[0].contains("No connections match"), "{empty:?}");

        menus.provider.provider_nav.query.clear();
        assert_eq!(rows(&mut menus), all);
        menus.dismiss(&console).unwrap();
    }

    #[test]
    fn connection_rows_take_the_role_of_their_authentication_state() {
        assert_eq!(provider_role(&Ok(true)), None);
        assert_eq!(provider_role(&Ok(false)), Some(Role::MutedText));
        assert_eq!(provider_role(&Err("expired".into())), Some(Role::Warning));
    }

    #[test]
    fn model_rows_name_their_connection_only_while_every_connection_is_listed() {
        let catalog = tickloop_harness::model::cards::ModelCatalog::defaults();
        let kimi =
            ProviderModels::group(vec![catalog.get("kimi-code-oauth", "k3").unwrap()]).remove(0);
        let console = Console::test_interactive_for_chat(Uuid::new_v4());
        let mut menus = Menus::default();
        menus.provider.provider_choices = vec![kimi.clone()];
        menus.provider.model_providers = vec![kimi.clone()];
        let choice = ModelChoice {
            config: kimi.models[0].clone(),
            thinking: None,
            availability: ModelAvailability::Unknown {
                reason: "test".into(),
            },
        };
        let id = Uuid::new_v4();
        menus
            .show_model_choices(
                &console,
                ConfigTarget::NewDraft,
                vec![choice.clone()],
                0,
                id,
            )
            .unwrap();
        let options = console.menu_snapshot(id).unwrap().unwrap().options;
        assert!(options[0].contains("Kimi (k3)"), "{}", options[0]);
        assert!(
            options[0].contains("· Kimi Code subscription"),
            "{}",
            options[0]
        );

        menus.provider.model_provider = Some(kimi.key());
        menus
            .show_model_choices(&console, ConfigTarget::NewDraft, vec![choice], 0, id)
            .unwrap();
        let options = console.menu_snapshot(id).unwrap().unwrap().options;
        assert!(
            !options[0].contains("· Kimi Code subscription"),
            "{}",
            options[0]
        );
        menus.dismiss(&console).unwrap();
    }

    #[test]
    fn grouped_models_and_refreshed_details_keep_the_selected_connections_failure() {
        let catalog = tickloop_harness::model::cards::ModelCatalog::defaults();
        let kimi = catalog.get("kimi-code-oauth", "k3").unwrap();
        let mut deepseek = catalog.get("deepseek-api", "deepseek-flash").unwrap();
        deepseek.api_key_env = "MERGE_MENU_FIXTURE_KEY".into();
        let models = vec![deepseek, kimi.clone()];
        for selected in 0..models.len() {
            let root = tempfile::tempdir().unwrap();
            let chat = Uuid::new_v4();
            let console = Console::test_interactive_for_chat(chat);
            let mut menus = Menus::new(None, Some(root.path().into()));
            menus.provider.provider_choices = ProviderModels::group(models.clone());
            let failure = tickloop_harness::protocol::ProviderHttpFailure {
                provider_id: kimi.provider_id.clone(),
                provider_name: "Kimi Code".into(),
                status: 403,
                class: tickloop_harness::protocol::ProviderFailureClass::UsageOrPlan,
                message: "Monthly usage limit reached".into(),
                hint: "Usage limit OR plan access; check K3 plan access".into(),
                retry_after: None,
                retryable: false,
            };
            menus
                .provider
                .provider_failures
                .insert(kimi.provider_id.clone(), failure);
            console
                .set_provider_failures(chat, &menus.provider.provider_failures)
                .unwrap();
            let choices = models
                .iter()
                .map(|config| ModelChoice {
                    config: config.clone(),
                    thinking: None,
                    availability: ModelAvailability::Unknown {
                        reason: "Fixture".into(),
                    },
                })
                .collect();
            menus
                .show_model_choices(
                    &console,
                    ConfigTarget::Agent(chat),
                    choices,
                    selected,
                    Uuid::new_v4(),
                )
                .unwrap();
            let menu = console.menu_snapshot(menus.id().unwrap()).unwrap().unwrap();
            assert!(menu.title.contains("Signed-in Providers"));
            assert_eq!(
                menu.context.iter().any(|line| line.contains("HTTP 403")),
                selected == 1
            );
            let provider = menus
                .provider
                .provider_choices
                .iter()
                .find(|provider| provider.models[0].provider_id == kimi.provider_id)
                .unwrap()
                .clone();
            menus
                .connection_details(&console, ConfigTarget::Agent(chat), provider.clone(), 1)
                .unwrap();
            let id = menus.id().unwrap();
            menus
                .connection_details(&console, ConfigTarget::Agent(chat), provider, 0)
                .unwrap();
            let menu = console.menu_snapshot(id).unwrap().unwrap();
            assert_eq!(menus.id(), Some(id));
            assert_eq!(menu.selected, 1);
            assert!(menu.context[0].contains("HTTP 403"));
            assert!(
                menu.context
                    .iter()
                    .any(|line| line.contains("Provider message: Monthly usage limit"))
            );
            console
                .set_provider_failures(chat, &HashMap::new())
                .unwrap();
            let menu = console.menu_snapshot(id).unwrap().unwrap();
            assert_eq!(menu.selected, 1);
            assert!(!menu.context.iter().any(|line| line.contains("HTTP 403")));
        }
    }

    #[test]
    fn capability_order_is_stable_and_separate_from_default_recommendations() {
        let mut profiles = tickloop_harness::model::cards::built_in_model_profiles();
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
        let access = tickloop_harness::model::access::AccessCatalog::defaults();
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
                "claude-haiku-5.5"
            ]
        );
        assert_eq!(
            access.recommended_model("github-copilot"),
            Some("claude-sonnet-5.5")
        );
    }

    #[test]
    fn codex_menu_uses_current_families_and_keeps_retired_cards_loadable() {
        let catalog = tickloop_harness::model::cards::ModelCatalog::defaults();
        let retired = catalog.get("openai-codex", "gpt-5.5").unwrap();
        let old_sol = catalog.get("openai-codex", "gpt-5.6-sol").unwrap();
        let profiles = super::super::super::settings::model_choices_with_profiles(
            &old_sol,
            tickloop_harness::model::cards::built_in_model_profiles(),
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
            ["GPT-6 Astra", "GPT-6.1 Sol", "GPT-6.1 Terra", "GPT-6 Luna"]
        );
        assert!(!codex.models.contains(&old_sol));
        assert!(!codex.models.contains(&retired));
        assert_eq!(
            tickloop_harness::model::access::AccessCatalog::defaults()
                .recommended_model("openai-codex"),
            Some("gpt-6.1-sol")
        );
    }

    #[test]
    fn api_key_page_names_the_environment_variable_of_the_connection() {
        let catalog = tickloop_harness::model::cards::ModelCatalog::defaults();
        let token_plan = catalog
            .get("alibaba-cn-token-plan", "qwen3.8-flash")
            .unwrap();
        let provider = ProviderModels::group(vec![token_plan]).remove(0);
        let context = provider.api_key_page_context(false);
        assert_eq!(
            context[2],
            "Without a saved key, the Agent uses ALIBABA_TOKEN_PLAN_API_KEY from the environment it started with."
        );
        assert_eq!(
            context.last().unwrap(),
            "Saving a key does not verify account access or select a model."
        );
        assert_eq!(
            provider.api_key_page_context(true).last().unwrap(),
            "Existing key is kept until Save succeeds."
        );

        let mut without_reference = catalog.get("deepseek-api", "deepseek-flash").unwrap();
        without_reference.api_key_env.clear();
        let provider = ProviderModels::group(vec![without_reference]).remove(0);
        let context = provider.api_key_page_context(false);
        assert_eq!(context.len(), 3);
        assert!(
            !context
                .iter()
                .any(|line| line.contains("Without a saved key"))
        );
    }

    #[test]
    fn retired_glm_card_is_not_offered_even_as_the_current_model() {
        let defaults = tickloop_harness::model::cards::ModelCatalog::defaults();
        let retired = defaults.get("zhipu-cn-api", "glm-5.2").unwrap();
        let profiles = tickloop_harness::model::cards::built_in_model_profiles();
        let choices =
            super::super::super::settings::model_choices_with_profiles(&retired, profiles);
        assert!(!choices.contains(&retired));
        let current = defaults.get("zhipu-cn-api", "glm-5.3-flash").unwrap();
        let choices = super::super::super::settings::model_choices_with_profiles(
            &current,
            tickloop_harness::model::cards::built_in_model_profiles(),
        );
        assert!(choices.contains(&current));
        assert_eq!(current.name, "GLM-5.3-Flash");
        assert_eq!(
            tickloop_harness::model::access::AccessCatalog::defaults()
                .series_for(&current)
                .unwrap()
                .name
                .clone(),
            "GLM Flash"
        );
    }
}

#[derive(Clone)]
pub(super) enum ProviderRow {
    Provider(usize),
}

pub(super) fn provider_group(provider: &ProviderModels) -> Option<&str> {
    let model = &provider.models[0];
    model
        .connection()
        .map(|connection| connection.vendor.as_str())
        .or_else(|| tickloop_harness::model::cards::provider_group(&model.provider_id))
}

pub(super) fn connection_key(model: &ModelConfig) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(
        serde_json::to_vec(&(
            &model.provider_id,
            model.auth_source,
            &model.api_key_env,
            model.base_url.trim_end_matches('/'),
        ))
        .expect("connection source is serializable"),
    );
    format!("{}:{digest:x}", model.provider_id)
}

fn endpoint_label(value: &str) -> String {
    reqwest::Url::parse(value)
        .map(|mut url| {
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.set_query(None);
            url.set_fragment(None);
            safe_text(url.as_str())
        })
        .unwrap_or_else(|_| "Custom endpoint".into())
}

/// A connection matches on its vendor, provider and directory names or its endpoint. Rows only
/// show the product and host, so `kimi` must still find the Moonshot-hosted Kimi connections.
fn provider_matches(provider: &ProviderModels, query: &str) -> bool {
    let config = &provider.models[0];
    [
        provider_group(provider).unwrap_or_default().to_owned(),
        config.provider_name.clone(),
        provider.directory_name(),
        provider.name(),
        config.base_url.clone(),
    ]
    .iter()
    .any(|candidate| candidate.to_lowercase().contains(query))
}

/// Configured connections keep the default text; the others are dimmed, and a credential that
/// cannot be read is a warning.
pub(super) fn provider_role(authenticated: &Result<bool, String>) -> Option<Role> {
    match authenticated {
        Ok(true) => None,
        Ok(false) => Some(Role::MutedText),
        Err(_) => Some(Role::Warning),
    }
}

impl ProviderModels {
    fn directory_name(&self) -> String {
        let config = &self.models[0];
        let Some(connection) = config.connection() else {
            return self.name();
        };
        let mut name = connection.product.clone();
        if let Some(region) = &connection.region {
            name.push_str(&format!(" · {}", region.name));
        }
        if self.is_custom() {
            name.push_str(" · Custom");
        }
        name
    }

    pub(super) fn key(&self) -> String {
        self.connection_key.clone()
    }

    fn is_custom(&self) -> bool {
        self.canonical_endpoints.is_none()
    }

    fn contains_source(&self, model: &ModelConfig) -> bool {
        let representative = &self.models[0];
        if representative.provider_id != model.provider_id
            || representative.auth_source != model.auth_source
            || representative.api_key_env != model.api_key_env
        {
            return false;
        }
        match &self.canonical_endpoints {
            Some(endpoints) => endpoints.get(&model.adapter()).is_some_and(|endpoint| {
                endpoint.trim_end_matches('/') == model.base_url.trim_end_matches('/')
            }),
            None => {
                representative.base_url.trim_end_matches('/')
                    == model.base_url.trim_end_matches('/')
            }
        }
    }

    pub(super) fn model_page_name(&self) -> String {
        match provider_group(self) {
            Some(vendor) => format!("{vendor} · {}", self.name()),
            None => self.name(),
        }
    }

    pub(super) fn api_key_page_context(&self, replacing: bool) -> Vec<String> {
        let name = self.model_page_name();
        let mut context = vec![
            safe_text(&format!(
                "Use an API key for {name}. Other regions/products have separate credentials."
            )),
            "Saved privately for this connection; saved values take precedence over the environment.".into(),
        ];
        let environment_key = &self.models[0].api_key_env;
        if !environment_key.is_empty() {
            context.push(safe_text(&format!(
                "Without a saved key, the Agent uses {environment_key} from the environment it started with."
            )));
        }
        context.push(if replacing {
            "Existing key is kept until Save succeeds.".into()
        } else {
            "Saving a key does not verify account access or select a model.".into()
        });
        context
    }

    pub(super) fn supports_discovery(&self) -> bool {
        self.models[0]
            .connection()
            .is_some_and(|connection| connection.discovery != ModelDiscovery::Unsupported)
    }

    pub(super) fn group(models: Vec<ModelConfig>) -> Vec<Self> {
        Self::group_with_catalog(
            models,
            &tickloop_harness::model::cards::ModelCatalog::defaults(),
        )
    }

    pub(super) fn group_with_catalog(
        models: Vec<ModelConfig>,
        catalog: &tickloop_harness::model::cards::ModelCatalog,
    ) -> Vec<Self> {
        use sha2::{Digest, Sha256};
        let mut choices: Vec<Self> = Vec::new();
        for model in models {
            let definition = catalog.providers.iter().find(|provider| {
                provider.id == model.provider_id
                    && provider.auth == model.auth_source
                    && provider.api_key_env == model.api_key_env
                    && provider
                        .endpoints
                        .get(&model.adapter())
                        .is_some_and(|endpoint| {
                            endpoint.trim_end_matches('/') == model.base_url.trim_end_matches('/')
                        })
            });
            let canonical_endpoints = definition.map(|provider| provider.endpoints.clone());
            let key = match &canonical_endpoints {
                Some(endpoints) => {
                    let digest = Sha256::digest(
                        serde_json::to_vec(&(
                            &model.provider_id,
                            model.auth_source,
                            &model.api_key_env,
                            endpoints,
                        ))
                        .expect("declared connection serializes"),
                    );
                    format!("{}:{digest:x}", model.provider_id)
                }
                None => connection_key(&model),
            };
            if let Some(provider) = choices
                .iter_mut()
                .find(|provider| provider.connection_key == key)
            {
                provider.models.push(model);
            } else {
                choices.push(Self {
                    models: vec![model],
                    connection_key: key,
                    canonical_endpoints,
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
        let access = tickloop_harness::model::access::AccessCatalog::defaults();
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
            let label =
                tickloop_harness::model::connection::connection_label(connection, &model.base_url);
            if self.is_custom() {
                format!("{label} · Custom")
            } else {
                label
            }
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
    if tickloop_harness::model::cards::model_card(model)
        .is_some_and(|card| card.thinking == tickloop_harness::protocol::ThinkingControl::Toggle)
    {
        choices.push(Some(ReasoningEffort::Off));
        if model.adapter() == Adapter::MinimaxMessages
            || ReasoningEffort::choices(model).is_empty()
                && tickloop_harness::model::validate_reasoning_effort(
                    model,
                    Some(ReasoningEffort::On),
                )
                .is_ok()
        {
            choices.push(Some(ReasoningEffort::On));
        }
    }
    choices.extend(ReasoningEffort::choices(model).iter().copied().map(Some));
    choices
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum ConnectionFilter {
    #[default]
    All,
    Configured,
}

#[derive(Clone)]
pub(super) enum ProviderPage {
    Directory {
        filter: ConnectionFilter,
        focused: Option<String>,
        query: String,
    },
    Models {
        connection: Option<String>,
        focused: Option<(String, String)>,
        action: Option<ModelAction>,
        query: String,
    },
    Settings,
    Details {
        connection: String,
        selected: usize,
    },
    LoginMethods {
        connection: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CredentialIntent {
    Setup,
    Replace,
}

#[derive(Clone, Default)]
pub(super) struct ProviderNavigator {
    pub(super) filter: ConnectionFilter,
    pub(super) focused: Option<String>,
    pub(super) back_stack: Vec<ProviderPage>,
    pub(super) credential_intent: Option<CredentialIntent>,
    /// The `/` search over the directory rows, kept across `p`/`esc` so the list comes back where
    /// it was left.
    pub(super) query: String,
}

impl ProviderNavigator {
    pub(super) fn escape_hint(&self) -> &'static str {
        match self.back_stack.last() {
            Some(ProviderPage::Directory { .. }) => "esc back to Providers",
            Some(ProviderPage::Models { .. }) => "esc back to models",
            Some(ProviderPage::Settings) => "esc back to Settings",
            Some(ProviderPage::Details { .. }) => "esc back to Connection",
            Some(ProviderPage::LoginMethods { .. }) => "esc back to sign-in",
            None => "esc close",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ModelAction {
    ConnectionDetails,
    RefreshAccountModels,
    Providers,
}

impl ModelAction {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::ConnectionDetails => "Connection details",
            Self::RefreshAccountModels => super::super::console::REFRESH_ACCOUNT_MODELS_LABEL,
            Self::Providers => "Manage Providers…",
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum ConnectionAction {
    Browse,
    Configure,
    RemoveKey,
    SignOut,
    CredentialDetails,
}

#[derive(Default)]
pub(super) struct ProviderSession {
    pub(super) search: super::search::SearchSession,
    pub(super) provider_failures:
        std::collections::HashMap<String, tickloop_harness::protocol::ProviderHttpFailure>,
    pub(super) login_return: Option<(ConfigTarget, ProviderModels)>,
    pub(super) provider_choices: Vec<ProviderModels>,
    pub(super) provider_selected: usize,
    pub(super) provider_nav: ProviderNavigator,
    pub(super) current_connection: Option<String>,
    pub(super) focused_model: Option<(String, String)>,
    pub(super) model_return: Option<ModelPickerDraft>,
    pub(super) model_choices: Vec<ModelChoice>,
    pub(super) model_providers: Vec<ProviderModels>,
    pub(super) model_provider: Option<String>,
    pub(super) model_query: String,
    pub(super) current_model: Option<(String, String)>,
    pub(super) provider_scope: Option<ProviderCredentialScope>,
    pub(super) model_warmup: Option<ModelWarmup>,
    pub(super) discovery_flow: Option<DiscoveryFlow>,
    // A directory repaint can race Escape; row choices stay fenced to the new menu ID.
    pub(super) refreshed_picker_cancel: Option<(Uuid, Uuid)>,
    pub(super) model_directory_status: BTreeMap<String, String>,
}

impl Menus {
    pub(super) fn sync_provider_chat(&mut self, chat: Option<Uuid>) {
        if self.provider_chat == Some(chat) {
            return;
        }
        if let Some(previous) = self.provider_chat {
            let next = self.provider_sessions.remove(&chat).unwrap_or_default();
            let previous_session = std::mem::replace(&mut self.provider, next);
            self.provider_sessions.insert(previous, previous_session);
        }
        self.provider_chat = Some(chat);
    }

    pub(in crate::terminal) fn note_provider_failures(
        &mut self,
        chat: Uuid,
        failures: &HashMap<String, tickloop_harness::protocol::ProviderHttpFailure>,
    ) {
        if self.provider_chat == Some(Some(chat)) {
            self.provider.provider_failures = failures.clone();
        }
        if let Some(session) = self.provider_sessions.get_mut(&Some(chat)) {
            session.provider_failures = failures.clone();
        }
    }

    pub(super) fn key_for_model(&self, model: &ModelConfig) -> Option<String> {
        self.provider
            .provider_choices
            .iter()
            .chain(self.provider.model_providers.iter())
            .find(|provider| provider.contains_source(model))
            .map(ProviderModels::key)
    }

    pub(super) fn provider_root(&self) -> Option<&std::path::Path> {
        self.provider
            .provider_scope
            .as_ref()
            .map_or(self.config_root.as_deref(), |scope| {
                scope.config_root.as_deref()
            })
    }

    pub(in crate::terminal) fn provider_login_root(&self) -> Option<PathBuf> {
        self.provider_root().map(std::path::Path::to_path_buf)
    }

    pub(super) fn provider_environment_key(&self, reference: &str) -> bool {
        self.provider.provider_scope.as_ref().map_or_else(
            || {
                std::env::var(reference).is_ok_and(|value| {
                    !value.trim().is_empty()
                        && reqwest::header::HeaderValue::from_str(&value).is_ok()
                })
            },
            |scope| scope.environment_keys.iter().any(|name| name == reference),
        )
    }

    fn provider_api_status(&self, reference: &str) -> Result<ApiKeyStatus, String> {
        if let Some(root) = self.provider_root()
            && tickloop_harness::model::credentials::load_api_key(root, reference)
                .map_err(|error| error.to_string())?
                .is_some()
        {
            return Ok(ApiKeyStatus::Saved);
        }
        Ok(if self.provider_environment_key(reference) {
            ApiKeyStatus::Environment
        } else {
            ApiKeyStatus::Missing
        })
    }

    pub(super) async fn refresh_provider_scope(
        &mut self,
        agent: &Agent,
        target: ConfigTarget,
    ) -> Result<(), String> {
        self.provider.provider_scope = Some(
            agent
                .provider_credentials(target)
                .await
                .map_err(|error| error.to_string())?,
        );
        Ok(())
    }

    pub(super) fn show_providers(
        &mut self,
        console: &Console,
        target: ConfigTarget,
        choices: Vec<ProviderModels>,
        selected: usize,
        id: Uuid,
    ) -> Result<(), String> {
        let query =
            super::super::conversation_picker::compact_line(&self.provider.provider_nav.query)
                .to_lowercase();
        let rows: Vec<_> = choices
            .iter()
            .enumerate()
            .filter(|(_, provider)| {
                self.provider.provider_nav.filter == ConnectionFilter::All
                    || self.provider_authenticated(provider) != Ok(false)
            })
            .filter(|(_, provider)| query.is_empty() || provider_matches(provider, &query))
            .map(|(index, _)| ProviderRow::Provider(index))
            .collect();
        let mut headers = Vec::new();
        let mut compact_rows = Vec::new();
        let mut row_roles = Vec::new();
        let mut last_vendor = None;
        let options: Vec<_> = rows
            .iter()
            .enumerate()
            .map(|(row_index, row)| {
                let ProviderRow::Provider(index) = row;
                let provider = &choices[*index];
                let vendor = provider_group(provider).unwrap_or("Custom");
                if last_vendor != Some(vendor) {
                    // The count follows the search but not the Configured filter, which hides
                    // exactly the rows it counts as missing.
                    let connections: Vec<_> = choices
                        .iter()
                        .filter(|candidate| provider_group(candidate).unwrap_or("Custom") == vendor)
                        .filter(|candidate| query.is_empty() || provider_matches(candidate, &query))
                        .collect();
                    let configured = connections
                        .iter()
                        .filter(|candidate| self.provider_authenticated(candidate) == Ok(true))
                        .count();
                    let attention = connections
                        .iter()
                        .filter(|candidate| self.provider_authenticated(candidate).is_err())
                        .count();
                    let mut label =
                        format!("{vendor} · {configured}/{} configured", connections.len());
                    if attention > 0 {
                        label.push_str(&format!(" · {attention} needs attention"));
                    }
                    headers.push((row_index, safe_text(&label)));
                    last_vendor = Some(vendor);
                }
                let current = if self.provider.current_connection.as_ref() == Some(&provider.key())
                {
                    "●"
                } else {
                    " "
                };
                let label = safe_text(&format!("{current} {}", provider.directory_name()));
                let detail = self.provider_auth_label(provider).to_owned();
                compact_rows.push(super::super::console::CompactPickerRow {
                    label: label.clone(),
                    detail: detail.clone(),
                });
                row_roles.push(provider_role(&self.provider_authenticated(provider)));
                safe_text(&format!("{current} {} · {detail}", provider.name()))
            })
            .collect();
        let selected = self
            .provider
            .provider_nav
            .focused
            .as_ref()
            .and_then(|focus| {
                rows.iter().position(|row| {
                    let ProviderRow::Provider(index) = row;
                    choices[*index].key() == *focus
                })
            })
            .unwrap_or(selected)
            .min(rows.len());
        let mut options = options;
        let empty = options.is_empty();
        if empty {
            options.push(if query.is_empty() {
                "Add connection…".into()
            } else {
                safe_text(&format!("No connections match “{}”", query))
            });
            row_roles.push(None);
        }
        options.push("Judgment settings · Jev (TypeSafe)".into());
        row_roles.push(None);
        let filter = match self.provider.provider_nav.filter {
            ConnectionFilter::All => "All",
            ConnectionFilter::Configured => "Configured",
        };
        let toggle = if filter == "All" { "Configured" } else { "All" };
        let escape = self.provider.provider_nav.escape_hint();
        let searching = !query.is_empty();
        let mut row_hints =
            vec![format!("p Connection · f Filter · / Search · enter · {escape}"); rows.len()];
        if empty {
            row_hints.push(if searching {
                format!("f {toggle} · / Search · enter clear search · {escape}")
            } else {
                format!("f {toggle} · / Search · enter add connection · {escape}")
            });
        }
        row_hints.push(format!("f {toggle} · enter Judgment settings · {escape}"));
        let mut context = if empty && !searching {
            vec!["No connections configured. Add a connection to browse models.".into()]
        } else {
            Vec::new()
        };
        if self.model_setup_applies_to(target.agent()) {
            context.insert(
                0,
                "Choose a provider, then select a model to get started.".into(),
            );
        }
        self.dismiss(console)?;
        let menu = Menu {
            id,
            title: format!("Choose Provider · {filter}"),
            context,
            markdown: None,
            options,
            selected,
            confirmation: false,
        };
        if console.is_interactive() {
            console.show_presented_picker_menu(
                menu,
                PickerKind::Provider,
                super::super::console::PickerPresentation {
                    headers,
                    editing_hint: None,
                    restore_context_offset: None,
                    refresh: false,
                    refresh_options: None,
                    connection_options: Some(rows.len()),
                    row_hints,
                    compact_rows,
                    row_roles,
                    search: Some(self.provider.provider_nav.query.clone()),
                    hint: format!(
                        "p Connection · f {} · enter models/setup · / Search · {escape}",
                        if filter == "All" { "Configured" } else { "All" }
                    ),
                    ..Default::default()
                },
            )?;
        } else {
            console.write(&format!("{}\n", menu.title))?;
            for (index, option) in menu.options.iter().enumerate() {
                console.write(&format!("  {}. {option}\n", index + 1))?;
            }
            console.write("Enter an option number, or cancel.\n")?;
        }
        self.active = Some((
            id,
            Selection::Provider {
                target,
                choices,
                rows,
            },
        ));
        Ok(())
    }

    pub(super) fn back_to_providers(
        &mut self,
        console: &Console,
        target: ConfigTarget,
    ) -> Result<(), String> {
        self.provider.model_provider = None;
        self.show_providers(
            console,
            target,
            self.provider.provider_choices.clone(),
            self.provider.provider_selected,
            tickloop_ids::generate(),
        )
    }

    pub(super) fn directory_parent(&self) -> ProviderPage {
        ProviderPage::Directory {
            filter: self.provider.provider_nav.filter,
            focused: self.provider.provider_nav.focused.clone(),
            query: self.provider.provider_nav.query.clone(),
        }
    }

    pub(super) fn connection(&self, key: &str) -> Result<ProviderModels, String> {
        self.provider
            .provider_choices
            .iter()
            .chain(self.provider.model_providers.iter())
            .find(|provider| provider.key() == key)
            .cloned()
            .ok_or_else(|| "Connection no longer exists.".into())
    }

    pub(super) fn connection_details(
        &mut self,
        console: &Console,
        target: ConfigTarget,
        provider: ProviderModels,
        selected: usize,
    ) -> Result<(), String> {
        let config = &provider.models[0];
        let authenticated = self.provider_authenticated(&provider);
        let mut actions = vec![ConnectionAction::Browse];
        let mut options = vec!["Browse models".into()];
        if config.auth_source == ModelAuthSource::GoogleCloud {
            actions.push(ConnectionAction::Configure);
            options.push("Edit service account…".into());
        } else if config.auth_source == ModelAuthSource::ApiKeyEnv {
            if !config.api_key_env.is_empty() {
                actions.push(ConnectionAction::Configure);
                options.push(
                    if authenticated == Ok(true) {
                        "Replace API key…"
                    } else {
                        "Set API key…"
                    }
                    .into(),
                );
                if self.provider_api_status(&config.api_key_env) == Ok(ApiKeyStatus::Saved) {
                    actions.push(ConnectionAction::RemoveKey);
                    options.push("Remove saved key…".into());
                }
            }
        } else {
            actions.push(ConnectionAction::Configure);
            options.push(
                if authenticated == Ok(true) {
                    "Sign in again…"
                } else {
                    "Sign in…"
                }
                .into(),
            );
            if authenticated == Ok(true) {
                actions.push(ConnectionAction::SignOut);
                options.push("Sign out…".into());
            }
        }
        actions.push(ConnectionAction::CredentialDetails);
        options.push("Credential details…".into());
        let failure = self.provider.provider_failures.get(&config.provider_id);
        let mut context = if console.is_interactive() {
            Vec::new()
        } else {
            failure.map(provider_failure_context).unwrap_or_default()
        };
        if self.model_setup_applies_to(target.agent()) {
            context.push(
                "Configure this connection, then select a model. Esc to choose another provider."
                    .into(),
            );
        }
        context.extend([
            self.provider_auth_label(&provider).into(),
            format!("Endpoint: {}", endpoint_label(&config.base_url)),
        ]);
        if let Err(error) = authenticated {
            context.push(safe_text(&error));
        }
        if !console.is_interactive()
            && let Some(failure) = failure
        {
            context.push(format!("Provider message: {}", failure.message));
        }
        let mut selected = selected;
        let mut id = tickloop_ids::generate();
        if let Some((
            previous_id,
            Selection::ConnectionDetails {
                target: previous_target,
                provider: previous_provider,
                actions: previous_actions,
            },
        )) = self.active.as_ref()
            && *previous_target == target
            && previous_provider.key() == provider.key()
        {
            let previous_selected = console.picker_selection(*previous_id)?.unwrap_or(selected);
            selected = previous_actions
                .get(previous_selected)
                .and_then(|action| actions.iter().position(|candidate| candidate == action))
                .unwrap_or(previous_selected.min(actions.len().saturating_sub(1)));
            if *previous_actions == actions {
                id = *previous_id;
            } else {
                self.provider.refreshed_picker_cancel = Some((*previous_id, id));
            }
        }
        self.show(
            console,
            Menu {
                id,
                title: safe_text(&format!("Connection · {}", provider.model_page_name())),
                context,
                markdown: None,
                options,
                selected,
                confirmation: false,
            },
            Selection::ConnectionDetails {
                target,
                provider,
                actions,
            },
        )
    }

    pub(super) async fn provider_back(
        &mut self,
        agent: &Agent,
        console: &Console,
        target: ConfigTarget,
    ) -> Result<(), String> {
        self.refresh_provider_scope(agent, target).await?;
        self.provider.discovery_flow.take();
        let parent = self.provider.provider_nav.back_stack.pop();
        if !matches!(&parent, Some(ProviderPage::LoginMethods { .. })) {
            self.provider.provider_nav.credential_intent = None;
        }
        match parent {
            Some(ProviderPage::Directory {
                filter,
                focused,
                query,
            }) => {
                self.provider.provider_nav.filter = filter;
                self.provider.provider_nav.focused = focused;
                self.provider.provider_nav.query = query;
                self.back_to_providers(console, target)
            }
            Some(ProviderPage::Models {
                connection,
                focused,
                action,
                query,
            }) => {
                self.provider.model_query = query;
                self.provider.model_return = Some(ModelPickerDraft {
                    provider: connection,
                    action,
                    selected: focused,
                    choices: self.provider.model_choices.clone(),
                });
                self.configured_models(agent, console, target).await
            }
            Some(ProviderPage::Details {
                connection,
                selected,
            }) => self.connection_details(console, target, self.connection(&connection)?, selected),
            Some(ProviderPage::LoginMethods { connection }) => {
                self.login_methods(console, target, self.connection(&connection)?)
            }
            Some(ProviderPage::Settings) => self.settings(agent, console, target).await,
            None => self.dismiss(console),
        }
    }

    pub(super) async fn enter_connection(
        &mut self,
        agent: &Agent,
        console: &Console,
        target: ConfigTarget,
        provider: ProviderModels,
    ) -> Result<MenuOutcome, String> {
        match self.provider_authenticated(&provider) {
            Ok(true) => {
                self.choose_provider_models(agent, console, target, &provider.key())
                    .await?;
                Ok(MenuOutcome::default())
            }
            Ok(false) => {
                self.provider.provider_nav.credential_intent = Some(CredentialIntent::Setup);
                self.provider_auth(console, target, provider)
            }
            Err(error) => {
                self.connection_details(console, target, provider, 0)?;
                Ok(MenuOutcome {
                    status: Some(error),
                    ..Default::default()
                })
            }
        }
    }

    pub(super) async fn credential_saved(
        &mut self,
        agent: &Agent,
        console: &Console,
        target: ConfigTarget,
        provider: &ProviderModels,
    ) -> Result<(), String> {
        self.refresh_connections
            .insert((target.agent(), provider.key()), tickloop_ids::generate());
        self.refresh_provider_scope(agent, target).await?;
        if matches!(
            self.provider.provider_nav.back_stack.last(),
            Some(ProviderPage::LoginMethods { .. })
        ) {
            self.provider.provider_nav.back_stack.pop();
        }
        self.model_discovery
            .invalidate(&provider.models[0].provider_id);
        if self.provider.provider_nav.credential_intent == Some(CredentialIntent::Replace) {
            self.provider_back(agent, console, target).await
        } else {
            if matches!(
                self.provider.provider_nav.back_stack.last(),
                Some(ProviderPage::Details { .. })
            ) {
                self.provider.provider_nav.back_stack.pop();
            }
            if matches!(self.provider.provider_nav.back_stack.last(), Some(ProviderPage::Models { connection, .. }) if connection.as_deref().is_none_or(|key| key == provider.key()))
            {
                return self.provider_back(agent, console, target).await;
            }
            self.provider.provider_nav.credential_intent = None;
            self.choose_provider_models(agent, console, target, &provider.key())
                .await
        }
    }

    pub(super) fn provider_auth_label(&self, provider: &ProviderModels) -> &'static str {
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
        match self.provider_api_status(&config.api_key_env) {
            Ok(ApiKeyStatus::Saved) => "API key saved",
            Ok(ApiKeyStatus::Environment) => "API key from environment",
            Ok(ApiKeyStatus::Missing) => "API key required",
            Err(_) => "API key needs attention",
        }
    }
    pub(super) fn configure_credential_intent(
        &self,
        provider: &ProviderModels,
    ) -> CredentialIntent {
        if self.provider_authenticated(provider) == Ok(false) {
            CredentialIntent::Setup
        } else {
            // Unknown authentication must not silently treat an existing credential as absent.
            CredentialIntent::Replace
        }
    }

    pub(super) fn provider_authenticated(&self, provider: &ProviderModels) -> Result<bool, String> {
        let scope = self.provider.provider_scope.clone().unwrap_or_else(|| {
            tickloop_harness::model::auth::current_provider_credential_scope(
                self.config_root.clone(),
            )
        });
        tickloop_harness::model::auth::provider_signed_in(&provider.models[0], &scope)
    }

    pub(in crate::terminal) fn warm_model_directory(
        &mut self,
        agent: &Agent,
        target: ConfigTarget,
        config: ModelConfig,
    ) {
        let provider = ProviderModels::group(vec![config.clone()]).remove(0);
        if !provider.supports_discovery()
            || (target == ConfigTarget::NewDraft
                && !self.provider_authenticated(&provider).unwrap_or(false))
        {
            return;
        }
        let cache = self.model_discovery.clone();
        let root = self.provider_login_root();
        let provider_id = connection_key(&config);
        let execution = agent.clone();
        let task = tokio::spawn(async move {
            if target.agent().is_some() {
                let _ = execution.account_models(target, config, false).await;
            } else {
                let _ = cache
                    .discover(
                        &config,
                        root.as_deref(),
                        false,
                        &tokio_util::sync::CancellationToken::new(),
                    )
                    .await;
            }
        });
        self.provider.model_warmup = Some(ModelWarmup {
            provider: provider_id,
            target,
            task,
        });
    }

    pub(in crate::terminal) fn invalidate_failed_directory(
        &mut self,
        snapshot: &ChatSnapshot,
        run: Uuid,
    ) {
        use tickloop_harness::protocol::{RequestFailure, RequestOutcome};
        let Some(request) = snapshot
            .request_usage
            .entries()
            .iter()
            .rev()
            .find(|request| request.run_id == Some(run.into()))
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
            self.refresh_connections.insert(
                (
                    Some(snapshot.id.into()),
                    self.key_for_model(&snapshot.model)
                        .unwrap_or_else(|| connection_key(&snapshot.model)),
                ),
                tickloop_ids::generate(),
            );
        } else {
            for provider in &self.provider.provider_choices {
                if provider
                    .models
                    .iter()
                    .any(|model| model.identity() == request.model)
                {
                    self.model_discovery
                        .invalidate(&provider.models[0].provider_id);
                    self.refresh_connections.insert(
                        (Some(snapshot.id.into()), provider.key()),
                        tickloop_ids::generate(),
                    );
                }
            }
        }
    }

    fn discovery_refreshes(
        &self,
        target: ConfigTarget,
        providers: &[ProviderModels],
        selected_provider: Option<&str>,
    ) -> HashMap<String, Uuid> {
        providers
            .iter()
            .filter(|provider| {
                provider.supports_discovery()
                    && selected_provider.is_none_or(|key| key == provider.key())
            })
            .filter_map(|provider| {
                let key = provider.key();
                self.refresh_connections
                    .get(&(target.agent(), key.clone()))
                    .map(|generation| (key, *generation))
            })
            .collect()
    }

    fn finish_discovery_refreshes(
        &mut self,
        target: ConfigTarget,
        refreshes: &HashMap<String, Uuid>,
    ) {
        self.refresh_connections
            .retain(|(chat, connection), generation| {
                *chat != target.agent() || refreshes.get(connection) != Some(generation)
            });
    }

    pub(super) async fn configured_models(
        &mut self,
        agent: &Agent,
        console: &Console,
        target: ConfigTarget,
    ) -> Result<(), String> {
        self.refresh_provider_scope(agent, target).await?;
        let requested = self
            .provider
            .model_return
            .as_ref()
            .and_then(|draft| draft.provider.as_ref());
        let providers: Vec<_> = self
            .provider
            .provider_choices
            .iter()
            .filter(|provider| {
                self.provider_authenticated(provider) == Ok(true)
                    || Some(&provider.key()) == requested
            })
            .cloned()
            .collect();
        let selected_provider = self.provider.model_return.as_ref().map_or_else(
            || self.provider.current_connection.clone(),
            |draft| draft.provider.clone(),
        );
        if !providers.iter().any(|provider| {
            selected_provider
                .as_ref()
                .is_none_or(|key| *key == provider.key())
                && provider.supports_discovery()
                && self.provider_authenticated(provider) == Ok(true)
        }) {
            let directories = providers
                .into_iter()
                .map(|provider| {
                    let error = if self.provider_authenticated(&provider) == Ok(true) {
                        "Account model check not requested"
                    } else {
                        "Credentials require attention before account check"
                    };
                    (provider, Err(error.into()))
                })
                .collect();
            return self
                .configured_model_choices(agent, console, target, directories)
                .await;
        }
        let initial = providers
            .iter()
            .cloned()
            .map(|provider| (provider, Err("Checking account models".into())))
            .collect();
        self.configured_model_choices(agent, console, target, initial)
            .await?;
        let id = self.id().ok_or("Model page closed before account check")?;
        let root = self.provider_login_root();
        let cache = self.model_discovery.clone();
        let cancel = tokio_util::sync::CancellationToken::new();
        let task_cancel = cancel.clone();
        let task_providers = providers.clone();
        let execution = agent.clone();
        let refreshed = self.discovery_refreshes(target, &providers, selected_provider.as_deref());
        let refreshes = refreshed.clone();
        let warmup = if self.provider.model_warmup.as_ref().is_some_and(|warmup| {
            warmup.target == target
                && providers.iter().any(|provider| {
                    selected_provider
                        .as_ref()
                        .is_none_or(|key| *key == provider.key())
                        && connection_key(&provider.models[0]) == warmup.provider
                })
        }) {
            self.provider.model_warmup.take()
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
                let result = if selected_provider
                    .as_ref()
                    .is_none_or(|key| *key == provider.key())
                    && provider.supports_discovery()
                {
                    if target.agent().is_some() {
                        execution
                            .account_models(
                                target,
                                provider.models[0].clone(),
                                refreshed.contains_key(&provider.key()),
                            )
                            .await
                    } else {
                        cache
                            .discover(
                                &provider.models[0],
                                root.as_deref(),
                                refreshed.contains_key(&provider.key()),
                                &task_cancel,
                            )
                            .await
                    }
                } else {
                    Err("Account model check not requested".into())
                };
                directories.push((provider, result));
            }
            let _ = sender.send(directories);
        });
        self.provider.discovery_flow = Some(DiscoveryFlow {
            refreshes,
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
        self.update_current_model(target, &snapshot.model);
        self.provider.model_directory_status = directories
            .iter()
            .map(|(provider, directory)| {
                (
                    provider.key(),
                    directory_status(&provider.models[0], directory),
                )
            })
            .collect();
        let preferred = if snapshot.thinking_enabled {
            snapshot.reasoning_effort
        } else {
            Some(ReasoningEffort::Off)
        };
        self.provider.model_providers = directories
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
        let draft = self.provider.model_return.take();
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
            .or(self.provider.current_model.as_ref())
            .cloned();
        self.provider.model_choices = choices;
        let action = draft.as_ref().and_then(|draft| draft.action);
        let page_provider = draft.and_then(|draft| draft.provider);
        if let Some(provider) = page_provider
            && self
                .provider
                .model_providers
                .iter()
                .any(|candidate| candidate.key() == provider)
        {
            return self.show_provider_models(console, target, &provider, focus.as_ref(), action);
        }
        self.provider.model_provider = None;
        let choices = self.provider.model_choices.clone();
        let selected = choices
            .iter()
            .position(|choice| {
                focus.as_ref().is_some_and(|(provider, model)| {
                    choice.config.provider_id == *provider && choice.config.model_id == *model
                })
            })
            .unwrap_or(0);
        let selected = action
            .and_then(|action| {
                self.model_actions(&choices)
                    .iter()
                    .position(|candidate| *candidate == action)
            })
            .map_or(selected, |index| choices.len() + index);
        self.show_model_choices(console, target, choices, selected, tickloop_ids::generate())
    }

    pub(super) fn remember_model_drafts(&mut self, choices: &[ModelChoice]) {
        for choice in choices {
            if let Some(saved) = self
                .provider
                .model_choices
                .iter_mut()
                .find(|saved| same_model(&saved.config, &choice.config))
            {
                saved.thinking = choice.thinking;
            }
        }
    }

    pub(super) fn show_provider_models(
        &mut self,
        console: &Console,
        target: ConfigTarget,
        provider: &str,
        focused: Option<&(String, String)>,
        action: Option<ModelAction>,
    ) -> Result<(), String> {
        let choices: Vec<_> = self
            .provider
            .model_choices
            .iter()
            .filter(|choice| self.key_for_model(&choice.config).as_deref() == Some(provider))
            .cloned()
            .collect();
        let focused = focused.or(self.provider.current_model.as_ref());
        let selected = choices
            .iter()
            .position(|choice| {
                focused.is_some_and(|(id, model)| {
                    choice.config.provider_id == *id && choice.config.model_id == *model
                })
            })
            .or_else(|| {
                let first = choices.first()?;
                if first.config.provider_id != "kimi-code-oauth" {
                    return None;
                }
                let recommended = tickloop_harness::model::access::AccessCatalog::defaults()
                    .recommended_model(&first.config.provider_id)?;
                choices
                    .iter()
                    .position(|choice| choice.config.model_id == recommended)
            })
            .unwrap_or(0);
        self.provider.model_provider = Some(provider.into());
        let selected = action
            .and_then(|action| {
                self.model_actions(&choices)
                    .iter()
                    .position(|candidate| *candidate == action)
            })
            .map_or(selected, |index| choices.len() + index);
        self.show_model_choices(console, target, choices, selected, tickloop_ids::generate())
    }

    pub(super) async fn refresh_model_choices(
        &mut self,
        agent: &Agent,
        console: &Console,
        target: ConfigTarget,
        connection: Option<&str>,
        action: Option<ModelAction>,
    ) -> Result<(), String> {
        let connection = connection
            .map(str::to_owned)
            .or_else(|| self.provider.model_provider.clone());
        for provider in &self.provider.model_providers {
            if provider.supports_discovery()
                && connection.as_ref().is_none_or(|key| *key == provider.key())
            {
                self.refresh_connections
                    .insert((target.agent(), provider.key()), tickloop_ids::generate());
            }
        }
        self.provider.model_return = Some(ModelPickerDraft {
            action,
            selected: self.provider.focused_model.clone(),
            choices: self.provider.model_choices.clone(),
            provider: self.provider.model_provider.clone(),
        });
        self.configured_models(agent, console, target).await
    }

    pub(super) async fn choose_provider_models(
        &mut self,
        agent: &Agent,
        console: &Console,
        target: ConfigTarget,
        provider: &str,
    ) -> Result<(), String> {
        self.provider.model_query.clear();
        self.provider.model_return = Some(ModelPickerDraft {
            action: None,
            selected: self.provider.focused_model.clone(),
            choices: self.provider.model_choices.clone(),
            provider: Some(provider.into()),
        });
        self.configured_models(agent, console, target).await
    }

    pub(super) fn open_model_providers(
        &mut self,
        console: &Console,
        target: ConfigTarget,
        choices: Vec<ModelChoice>,
        index: usize,
    ) -> Result<(), String> {
        self.remember_model_drafts(&choices);
        let connection = choices
            .get(index)
            .and_then(|choice| self.key_for_model(&choice.config))
            .or_else(|| self.provider.model_provider.clone())
            .or_else(|| self.provider.current_connection.clone())
            .ok_or("Model connection no longer exists")?;
        self.provider.focused_model = choices.get(index).map(|choice| {
            (
                choice.config.provider_id.clone(),
                choice.config.model_id.clone(),
            )
        });
        self.provider
            .provider_nav
            .back_stack
            .push(ProviderPage::Models {
                connection: self.provider.model_provider.clone(),
                focused: self.provider.focused_model.clone(),
                query: self.provider.model_query.clone(),
                action: index
                    .checked_sub(choices.len())
                    .and_then(|index| self.model_actions(&choices).get(index).copied()),
            });
        self.connection_details(console, target, self.connection(&connection)?, 0)
    }

    pub(super) fn show_model_choices(
        &mut self,
        console: &Console,
        target: ConfigTarget,
        choices: Vec<ModelChoice>,
        selected: usize,
        id: Uuid,
    ) -> Result<(), String> {
        let query = super::super::conversation_picker::compact_line(&self.provider.model_query)
            .to_lowercase();
        let browsing_all = self.provider.model_provider.is_none();
        // `selected` indexes the unfiltered models, then the actions; keep pointing at the same
        // model or action once the query hides some rows.
        let unfiltered = choices.len();
        let focused = choices.get(selected).map(|choice| choice.config.clone());
        let choices: Vec<_> = choices
            .into_iter()
            .filter(|choice| query.is_empty() || self.model_choice_matches(choice, &query))
            .collect();
        let selected = match focused {
            Some(config) => choices
                .iter()
                .position(|choice| same_model(&choice.config, &config))
                .unwrap_or(0),
            None => choices.len() + selected.saturating_sub(unfiltered),
        };
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
                let name = format!(
                    "{} ({})",
                    super::super::settings::model_name(&choice.config),
                    choice.config.model
                );
                let current =
                    if self
                        .provider
                        .current_model
                        .as_ref()
                        .is_some_and(|(provider, model)| {
                            choice.config.provider_id == *provider
                                && choice.config.model_id == *model
                                && self.provider.current_connection
                                    == self.key_for_model(&choice.config)
                        })
                    {
                        "●"
                    } else {
                        " "
                    };
                // Browsing every connection at once, the group heading is the only other place
                // the connection appears, so each row names it.
                let connection = browsing_all
                    .then(|| self.key_for_model(&choice.config))
                    .flatten()
                    .and_then(|key| self.connection(&key).ok())
                    .map_or_else(String::new, |provider| {
                        format!("  · {}", provider.models[0].provider_name)
                    });
                safe_text(&thinking.map_or_else(
                    || format!("{current} {name}{connection}"),
                    |thinking| format!("{current} {name}    Thinking ‹ {thinking} ›{connection}"),
                ))
            })
            .collect();
        self.remember_model_drafts(&choices);
        let actions = self.model_actions(&choices);
        let mut selected = selected;
        let mut id = id;
        if console.is_interactive()
            && let Some((
                previous_id,
                Selection::Model {
                    target: previous_target,
                    choices: previous_choices,
                    actions: previous_actions,
                },
            )) = self.active.as_ref()
            && *previous_target == target
            && console.dialog_menu_id()? == Some(*previous_id)
        {
            let previous_selected = console.picker_selection(*previous_id)?.unwrap_or(selected);
            selected = if let Some(previous) = previous_choices.get(previous_selected) {
                choices
                    .iter()
                    .position(|choice| same_model(&previous.config, &choice.config))
                    .unwrap_or(previous_selected.min(choices.len().saturating_sub(1)))
            } else {
                previous_selected
                    .checked_sub(previous_choices.len())
                    .and_then(|index| previous_actions.get(index))
                    .and_then(|action| actions.iter().position(|candidate| candidate == action))
                    .map_or(selected, |index| choices.len() + index)
            };
            if previous_choices.len() == choices.len()
                && previous_choices
                    .iter()
                    .zip(&choices)
                    .all(|(previous, current)| same_model(&previous.config, &current.config))
                && *previous_actions == actions
            {
                id = *previous_id;
            }
        }
        options.extend(actions.iter().map(|action| action.label().to_owned()));
        self.provider.focused_model = choices.get(selected).map(|choice| {
            (
                choice.config.provider_id.clone(),
                choice.config.model_id.clone(),
            )
        });
        let provider = self
            .provider
            .model_provider
            .as_ref()
            .and_then(|key| self.connection(key).ok());
        let mut context = provider.as_ref().map_or_else(Vec::new, |provider| {
            vec![self.provider_auth_label(provider).into()]
        });
        if self.model_setup_applies_to(target.agent()) {
            context.insert(0, "Select a model to finish setup.".into());
        }
        if choices.is_empty() && !query.is_empty() {
            context.push(safe_text(&format!("No models match “{query}”")));
        } else if choices.is_empty() {
            context.push(
                "No models available. Use /provider to sign in or configure a connection.".into(),
            );
        }
        if let Some(provider) = &provider {
            if let Some(status) = self.provider.model_directory_status.get(&provider.key()) {
                context.push(status.clone());
            }
            if let Some(notice) = tickloop_harness::model::access::AccessCatalog::defaults()
                .provider(&provider.models[0].provider_id)
                .and_then(|access| access.notice.as_deref())
            {
                context.push(safe_text(notice));
            }
        } else if self.provider.discovery_flow.is_some() {
            context.push("Checking account models…".into());
        }
        let failure_provider = choices
            .get(selected)
            .map(|choice| choice.config.provider_id.as_str())
            .or_else(|| {
                provider
                    .as_ref()
                    .map(|provider| provider.models[0].provider_id.as_str())
            });
        if !console.is_interactive()
            && let Some(failure) =
                failure_provider.and_then(|provider| self.provider.provider_failures.get(provider))
        {
            context.splice(0..0, provider_failure_context(failure));
        }
        let title = provider.as_ref().map_or_else(
            || "Choose model · Signed-in Providers".into(),
            |provider| format!("Choose model · {}", provider.model_page_name()),
        );
        self.show(
            console,
            Menu {
                id,
                title: safe_text(&title),
                context,
                markdown: None,
                options,
                selected,
                confirmation: false,
            },
            Selection::Model {
                target,
                choices,
                actions,
            },
        )?;
        if let Some(flow) = &mut self.provider.discovery_flow
            && flow.target == target
        {
            flow.id = id;
        }
        Ok(())
    }

    /// A model matches on its series, card name, service id or connection.
    fn model_choice_matches(&self, choice: &ModelChoice, query: &str) -> bool {
        [
            super::super::settings::model_name(&choice.config),
            choice.config.name.as_str(),
            choice.config.model.as_str(),
            choice.config.model_id.as_str(),
        ]
        .iter()
        .any(|candidate| candidate.to_lowercase().contains(query))
            || self
                .key_for_model(&choice.config)
                .and_then(|key| self.connection(&key).ok())
                .is_some_and(|provider| provider_matches(&provider, query))
    }

    pub(super) fn model_actions(&self, choices: &[ModelChoice]) -> Vec<ModelAction> {
        let mut actions = Vec::new();
        if !choices.is_empty() || self.provider.model_provider.is_some() {
            actions.push(ModelAction::ConnectionDetails);
        }
        if self.provider.model_providers.iter().any(|provider| {
            self.provider
                .model_provider
                .as_ref()
                .is_none_or(|key| *key == provider.key())
                && provider.supports_discovery()
        }) {
            actions.push(ModelAction::RefreshAccountModels);
        }
        if self.provider.model_provider.is_none() {
            actions.push(ModelAction::Providers);
        }
        actions
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
        if let Some(target) = selection.config_target() {
            self.refresh_provider_scope(agent, target).await?;
        }
        match selection {
            Selection::Provider {
                target,
                choices,
                rows,
            } => {
                self.provider.provider_selected = index;
                let provider = rows.get(index).map(|row| {
                    let ProviderRow::Provider(index) = row;
                    choices[*index].clone()
                });
                if let Some(provider) = &provider {
                    self.provider.provider_nav.focused = Some(provider.key());
                }
                if action == PickerAction::SearchQuery {
                    self.provider.provider_nav.query = console.picker_search_query(menu_id)?;
                    self.show_providers(console, target, choices, index, menu_id)?;
                } else if action == PickerAction::ToggleConnections {
                    self.provider.provider_nav.filter = match self.provider.provider_nav.filter {
                        ConnectionFilter::All => ConnectionFilter::Configured,
                        ConnectionFilter::Configured => ConnectionFilter::All,
                    };
                    self.show_providers(console, target, choices, 0, tickloop_ids::generate())?;
                } else if action == PickerAction::ConfigureProvider {
                    if let Some(provider) = provider {
                        self.provider
                            .provider_nav
                            .back_stack
                            .push(self.directory_parent());
                        self.connection_details(console, target, provider, 0)?;
                    } else {
                        self.provider.provider_nav.filter = ConnectionFilter::All;
                        self.show_providers(console, target, choices, 0, tickloop_ids::generate())?;
                    }
                } else {
                    self.show_providers(console, target, choices, index, menu_id)?;
                }
            }
            Selection::Model {
                target,
                mut choices,
                ..
            } => {
                self.provider.focused_model = choices.get(index).map(|choice| {
                    (
                        choice.config.provider_id.clone(),
                        choice.config.model_id.clone(),
                    )
                });
                if action == PickerAction::SearchQuery {
                    self.provider.model_query = console.picker_search_query(menu_id)?;
                    // The menu holds only the rows the previous query kept, so filter again from
                    // every model of this page.
                    self.remember_model_drafts(&choices);
                    let focused = choices.get(index).map(|choice| choice.config.clone());
                    let all: Vec<_> = self
                        .provider
                        .model_choices
                        .iter()
                        .filter(|choice| {
                            self.provider.model_provider.as_ref().is_none_or(|key| {
                                self.key_for_model(&choice.config).as_ref() == Some(key)
                            })
                        })
                        .cloned()
                        .collect();
                    let selected = focused
                        .and_then(|config| {
                            all.iter()
                                .position(|choice| same_model(&choice.config, &config))
                        })
                        .unwrap_or(0);
                    self.show_model_choices(console, target, all, selected, menu_id)?;
                } else if action == PickerAction::RefreshAccountModels {
                    let connection = choices
                        .get(index)
                        .and_then(|choice| self.key_for_model(&choice.config));
                    self.remember_model_drafts(&choices);
                    self.refresh_model_choices(agent, console, target, connection.as_deref(), None)
                        .await?;
                } else if action == PickerAction::ConfigureProvider {
                    self.open_model_providers(console, target, choices, index)?;
                } else {
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
            }
            other => self.active = Some((menu_id, other)),
        }
        Ok(MenuOutcome::default())
    }

    pub(super) fn login_methods(
        &mut self,
        console: &Console,
        target: ConfigTarget,
        provider: ProviderModels,
    ) -> Result<(), String> {
        self.show(console, Menu { id: tickloop_ids::generate(), title: format!("Sign in · {}", provider.model_page_name()), context: vec!["Authorize this connection. Existing login is kept until authorization succeeds.".into()], markdown: None, options: vec!["Device code".into(), "Browser + callback".into()], selected: 0, confirmation: false }, Selection::LoginMethods { target, provider })
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
            self.login_methods(console, target, provider)?;
            return Ok(MenuOutcome::default());
        }
        if provider.models[0].api_key_env.is_empty() {
            self.back_to_providers(console, target)?;
            return Ok(MenuOutcome {
                status: Some("Local connection requires no credentials.".into()),
                ..Default::default()
            });
        }
        self.provider_api_key(console, target, provider, None)?;
        Ok(MenuOutcome::default())
    }

    pub(super) fn begin_provider_login(
        &mut self,
        console: &Console,
        target: ConfigTarget,
        provider: ProviderModels,
        method: super::super::login::LoginMethod,
    ) -> Result<MenuOutcome, String> {
        self.provider_root()
            .ok_or("Cannot locate the Agent state directory; set TICKLOOP_CONFIG_DIR.")?;
        self.dismiss(console)?;
        self.provider.login_return = Some((target, provider));
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
            .provider_root()
            .ok_or("Cannot locate the Agent state directory; set TICKLOOP_CONFIG_DIR.")?;
        let saved = self
            .provider
            .provider_scope
            .as_ref()
            .map_or_else(
                || {
                    tickloop_harness::model::auth::google::GoogleAuth::new(root)
                        .service_account_path()
                        .map_err(|error| error.to_string())
                },
                |scope| scope.service_account_path.clone(),
            )
            .unwrap_or(None);
        let menu = Menu {
            id: tickloop_ids::generate(),
            title: "Service account · Google Vertex".into(),
            context: vec![
                "Enter the absolute path to your service-account JSON file on this computer."
                    .into(),
                "The file must be private (0600). Only its path is saved; the file stays in place."
                    .into(),
                "The project comes from the file unless overridden. Location defaults to global."
                    .into(),
            ],
            markdown: None,
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
        candidate: Option<&str>,
    ) -> Result<(), String> {
        let _root = self
            .provider_root()
            .ok_or("Cannot locate the Agent state directory; set TICKLOOP_CONFIG_DIR.")?;
        if !console.is_interactive() {
            return Err(format!(
                "API key entry requires an interactive terminal. Configure {} in the Agent .env file.",
                provider.models[0].api_key_env
            ));
        }
        let menu = Menu {
            id: tickloop_ids::generate(),
            title: safe_text(&format!("API key · {}", provider.model_page_name())),
            context: provider.api_key_page_context(
                self.provider.provider_nav.credential_intent == Some(CredentialIntent::Replace),
            ),
            markdown: None,
            options: vec!["Save API key".into(), "Cancel".into()],
            selected: 0,
            confirmation: false,
        };
        self.dismiss(console)?;
        let id = menu.id;
        console.show_secret_menu_with_value(menu, candidate.unwrap_or(""))?;
        self.active = Some((id, Selection::ApiKey { target, provider }));
        Ok(())
    }

    pub(in crate::terminal) fn note_provider_login_result(
        &mut self,
        chat: Option<Uuid>,
        succeeded: bool,
    ) {
        if succeeded {
            let session = if self.provider_chat == Some(chat) {
                Some(&self.provider)
            } else {
                self.provider_sessions.get(&chat)
            };
            if let Some((target, provider)) =
                session.and_then(|session| session.login_return.as_ref())
            {
                self.model_discovery
                    .invalidate(&provider.models[0].provider_id);
                self.refresh_connections
                    .insert((target.agent(), provider.key()), tickloop_ids::generate());
            }
        }
        self.provider_login_results.insert(chat, succeeded);
    }

    pub(in crate::terminal) fn take_provider_login_result(
        &mut self,
        console: &Console,
    ) -> Result<Option<bool>, String> {
        if console.agent_request_is_open()? {
            return Ok(None);
        }
        self.sync_dialog_chat(console)?;
        Ok(self.provider_login_results.remove(&console.dialog_chat()?))
    }

    pub(in crate::terminal) async fn resume_provider_login(
        &mut self,
        agent: &Agent,
        console: &Console,
        succeeded: bool,
    ) -> Result<(), String> {
        if let Some((target, provider)) = self.provider.login_return.take() {
            if succeeded {
                self.model_discovery
                    .invalidate(&provider.models[0].provider_id);
                self.refresh_connections
                    .insert((target.agent(), provider.key()), tickloop_ids::generate());
            }
            if matches!(&self.active, Some((_, Selection::ConnectionDetails { target: current_target, provider: current_provider, .. })) if *current_target == target && current_provider.key() == provider.key())
            {
                self.refresh_provider_scope(agent, target).await?;
                self.connection_details(console, target, provider, 0)?;
            } else if self.active.is_none() {
                self.provider_back(agent, console, target).await?;
            }
        }
        Ok(())
    }
    pub(in crate::terminal) fn provider_logout_menu_is_active(&self, id: Uuid) -> bool {
        matches!((&self.active, &self.provider.login_return),
            (Some((menu_id, Selection::ConnectionDetails { target, provider, .. })), Some((login_target, login_provider)))
            if *menu_id == id && target == login_target && provider.key() == login_provider.key())
    }

    pub(in crate::terminal) fn discovery_active(&self) -> bool {
        self.provider.discovery_flow.as_ref().is_some_and(
            |flow| matches!(self.active, Some((id, Selection::Model { .. })) if id == flow.id),
        )
    }

    pub(in crate::terminal) async fn next_provider_update(&mut self) -> ProviderUpdate {
        let search_active = self.search_test_active();
        let discovery_active = self.discovery_active();
        let (search, discovery) = (
            &mut self.provider.search.flow,
            &mut self.provider.discovery_flow,
        );
        tokio::select! {
            result = async { (&mut search.as_mut().expect("active search test").receiver).await }, if search_active => {
                ProviderUpdate::Search(result.unwrap_or_else(|_| Err("Search test ended without a result".into())))
            },
            result = async { discovery.as_mut().expect("active model discovery").receiver.recv().await }, if discovery_active => {
                ProviderUpdate::Models(result)
            },
        }
    }

    #[cfg(test)]
    pub(in crate::terminal) async fn next_discovery(&mut self) -> Option<ModelDirectories> {
        self.provider.discovery_flow.as_mut()?.receiver.recv().await
    }

    pub(in crate::terminal) async fn finish_discovery(
        &mut self,
        agent: &Agent,
        console: &Console,
        result: Option<ModelDirectories>,
    ) -> Result<(), String> {
        let Some(flow) = self.provider.discovery_flow.take() else {
            return Ok(());
        };
        if self.id() != Some(flow.id) {
            return Ok(());
        }
        if result.is_some() {
            self.finish_discovery_refreshes(flow.target, &flow.refreshes);
        }
        if let Some((_, Selection::Model { choices, .. })) = self.active.as_ref() {
            let selected = console
                .picker_selection(flow.id)?
                .and_then(|index| choices.get(index))
                .map(|choice| {
                    (
                        choice.config.provider_id.clone(),
                        choice.config.model_id.clone(),
                    )
                });
            let action = console
                .picker_selection(flow.id)?
                .and_then(|index| index.checked_sub(choices.len()))
                .and_then(|index| self.model_actions(choices).get(index).copied());
            let drafts = choices.clone();
            self.remember_model_drafts(&drafts);
            self.provider.model_return = Some(ModelPickerDraft {
                action,
                provider: self.provider.model_provider.clone(),
                selected,
                choices: self.provider.model_choices.clone(),
            });
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
        .await?;
        if let Some((updated, Selection::Model { .. })) = self.active.as_ref() {
            self.provider.refreshed_picker_cancel = Some((flow.id, *updated));
        }
        Ok(())
    }

    pub(in crate::terminal) fn invalidate_model_discovery(&mut self) {
        if let Some((target, provider)) = &self.provider.login_return {
            self.refresh_connections
                .insert((target.agent(), provider.key()), tickloop_ids::generate());
            self.model_discovery
                .invalidate(&provider.models[0].provider_id);
        }
    }
}

pub(super) struct ModelWarmup {
    provider: String,
    target: ConfigTarget,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for ModelWarmup {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(super) struct DiscoveryFlow {
    pub(super) id: Uuid,
    target: ConfigTarget,
    pub(super) providers: Vec<ProviderModels>,
    refreshes: HashMap<String, Uuid>,
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

pub(in crate::terminal) enum ProviderUpdate {
    Models(Option<ModelDirectories>),
    Search(Result<tickloop_harness::protocol::SearchTestResult, String>),
}

type ModelDirectories = Vec<(ProviderModels, Result<AccountModelDirectory, String>)>;

pub(super) struct ModelPickerDraft {
    pub(super) action: Option<ModelAction>,
    pub(super) provider: Option<String>,
    pub(super) selected: Option<(String, String)>,
    pub(super) choices: Vec<ModelChoice>,
}

fn same_model(left: &ModelConfig, right: &ModelConfig) -> bool {
    connection_key(left) == connection_key(right) && left.model_id == right.model_id
}

fn directory_status(
    config: &ModelConfig,
    directory: &Result<AccountModelDirectory, String>,
) -> String {
    match directory {
        Err(error) if error == "Credentials require attention before account check" => "Review credentials in Connection Details before checking account access.".into(),
        Err(error) if error == "Checking account models" => "Checking account models… esc returns and cancels the check.".into(),
        Ok(directory) if directory.complete => "Account directory checked.".into(),
        Ok(_) => "Account directory is incomplete; missing models remain unverified.".into(),
        Err(_)
            if config
                .connection()
                .is_none_or(|connection| connection.discovery == ModelDiscovery::Unsupported) =>
        {
            "Model access is checked when a request is sent.".into()
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
                "Account model check failed (HTTP {status}). Credentials remain configured; use Refresh to retry."
            )
        }
        Err(_) => {
            "Account model access could not be checked. Credentials remain configured; use Refresh to retry."
                .into()
        }
    }
}

pub(in crate::terminal) fn provider_failure_context(
    failure: &tickloop_harness::protocol::ProviderHttpFailure,
) -> Vec<String> {
    let next_step =
        if failure.class == tickloop_harness::protocol::ProviderFailureClass::UsageOrPlan {
            "K3 may require a higher Kimi Code plan; check plan access and usage limits.".into()
        } else {
            failure.hint.clone()
        };
    vec![
        format!("Last request failed: {}", failure.summary()),
        next_step,
    ]
}
