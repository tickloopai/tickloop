use serde_json::json;
use tickloop_harness_model::{
    LocalModelClients, ModelClients,
    access::AccessCatalog,
    cards::{ModelCatalog, built_in_model_profiles, model_card},
    validate_model_config, validate_model_for_resume, validate_reasoning_effort,
};
use tickloop_harness_protocol::{
    Adapter, ModelAuthSource, ModelConfig, ModelProvider, NativeToolSearch, Protocol,
    ReasoningEffort, ThinkingControl, auto_compact_tokens,
};

#[test]
fn legacy_model_settings_keep_api_key_identity() {
    let legacy = json!({
        "name": "OpenAI API",
        "protocol": "responses",
        "base_url": "https://api.openai.com/v1",
        "model": "gpt-5.5",
        "api_key_env": "OPENAI_API_KEY",
        "max_output_tokens": 4096
    });
    let config: ModelConfig = serde_json::from_value(legacy.clone()).unwrap();
    assert_eq!(config.provider, ModelProvider::Api);
    assert_eq!(config.auth_source, ModelAuthSource::ApiKeyEnv);
    assert_eq!(config.identity().provider, ModelProvider::Api);
    assert_eq!(config.protocol, Protocol::Responses);
    assert_eq!(
        serde_json::to_value(config).unwrap()["api_key_env"],
        legacy["api_key_env"]
    );
}

#[test]
fn codex_subscription_has_distinct_private_history_identity() {
    let api: ModelConfig = serde_json::from_value(json!({
        "name": "GPT",
        "protocol": "responses",
        "base_url": "https://api.openai.com/v1",
        "model": "gpt-5.5",
        "api_key_env": "OPENAI_API_KEY",
        "max_output_tokens": 4096
    }))
    .unwrap();
    let mut subscription = api.clone();
    subscription.provider = ModelProvider::OpenAiCodex;
    subscription.auth_source = ModelAuthSource::CodexSubscription;
    subscription.api_key_env.clear();
    assert_ne!(api.identity(), subscription.identity());
    let saved = serde_json::to_value(&subscription).unwrap();
    assert_eq!(saved["provider"], "openai_codex");
    assert_eq!(saved["auth_source"], "codex_subscription");
    assert_eq!(
        serde_json::from_value::<ModelConfig>(saved).unwrap(),
        subscription
    );
}

#[test]
fn codex_profiles_are_source_aware_and_use_only_the_official_endpoint() {
    let profiles = built_in_model_profiles();
    let codex = profiles
        .iter()
        .find(|profile| profile.provider_id == "openai-codex" && profile.model == "gpt-6.1-sol")
        .unwrap();
    assert_eq!(codex.provider, ModelProvider::OpenAiCodex);
    assert_eq!(codex.auth_source, ModelAuthSource::CodexSubscription);
    assert_eq!(
        codex.base_url,
        "https://chatgpt.com/backend-api/codex/responses"
    );
    assert!(model_card(codex).is_some());
    assert!(validate_model_config(codex).is_ok());
    assert!(LocalModelClients::new(None).client(codex).is_err());

    let mut redirected = codex.clone();
    redirected.base_url = "http://127.0.0.1:1234".into();
    assert!(validate_model_config(&redirected).is_err());

    let mut mismatched_auth = codex.clone();
    mismatched_auth.auth_source = ModelAuthSource::ApiKeyEnv;
    assert!(validate_model_config(&mismatched_auth).is_err());
}

#[test]
fn codex_cards_keep_per_model_limits_and_terra_upgrade_retains_previous_identities() {
    let catalog = ModelCatalog::defaults();
    let access = AccessCatalog::defaults();
    let visible = built_in_model_profiles();
    // Use Codex's default context_window of 272K (also for Terra, whose default is 500K), not its
    // max_context_window: OpenAI prices a whole request at the long-context rate once its input
    // passes 272K.
    // https://github.com/openai/codex/blob/bea47c06dfac12ecce362090418e12a6faeedbf9/codex-rs/models-manager/models.json
    for (model, context_window, compact_threshold, ordinary) in [
        ("gpt-5.5", 272_000, 239_000, false),
        ("gpt-5.6-sol", 272_000, 239_000, false),
        ("gpt-5.6-terra", 272_000, 239_000, false),
        ("gpt-5.6-luna", 272_000, 239_000, false),
        ("gpt-6-astra", 272_000, 239_000, true),
        ("gpt-6.1-sol", 272_000, 239_000, true),
        ("gpt-6.1-terra", 272_000, 239_000, true),
        ("gpt-6-luna", 272_000, 239_000, true),
    ] {
        let profile = catalog.get("openai-codex", model).unwrap();
        let card = profile.card.as_ref().unwrap();
        assert_eq!(card.context_window_tokens, Some(context_window), "{model}");
        assert_eq!(profile.max_output_tokens, 128_000, "{model}");
        assert_eq!(card.max_output_tokens, 128_000, "{model}");
        assert_eq!(
            auto_compact_tokens(&profile, None, None),
            Some(compact_threshold),
            "{model}"
        );
        assert_eq!(card.adapter, Adapter::CodexResponses);
        assert_eq!(profile.provider, ModelProvider::OpenAiCodex);
        assert_eq!(profile.auth_source, ModelAuthSource::CodexSubscription);
        assert_eq!(
            profile.base_url,
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert!(profile.accepts_images());
        assert_eq!(visible.contains(&profile), ordinary, "{model}");
        assert_eq!(
            access.ordinary_candidate(&profile, &catalog),
            Some(ordinary)
        );
        assert_eq!(
            card.native_tool_search.flatten(),
            (model == "gpt-6.1-sol").then_some(NativeToolSearch::OpenAi),
            "{model}"
        );
        assert_eq!(
            card.effort_levels,
            [
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
                ReasoningEffort::Xhigh,
                ReasoningEffort::Max,
            ]
        );
        for effort in &card.effort_levels {
            assert!(validate_reasoning_effort(&profile, Some(*effort)).is_ok());
        }
        let always_thinking = matches!(model, "gpt-6-astra" | "gpt-6.1-sol");
        assert_eq!(
            card.thinking,
            if always_thinking {
                ThinkingControl::AlwaysOn
            } else {
                ThinkingControl::Toggle
            }
        );
        assert_eq!(
            validate_reasoning_effort(&profile, Some(ReasoningEffort::Off)).is_ok(),
            !always_thinking
        );
        assert!(!card.adaptive_by_default);
    }
    let current = catalog.get("openai-codex", "gpt-6.1-terra").unwrap();
    let retained = catalog.get("openai-codex", "gpt-5.6-terra").unwrap();
    assert_eq!(current.name, "GPT-6.1 Terra");
    assert_eq!(retained.name, "GPT-5.6 Terra");
    assert_ne!(current.identity(), retained.identity());
    assert_ne!(
        current.identity(),
        catalog
            .get("openai-api", "gpt-6.1-terra")
            .unwrap()
            .identity()
    );
    assert!(access.series_for(&retained).is_none());
    let series = access.series_for(&current).unwrap();
    assert_eq!(series.id, "terra");
    assert_eq!(series.name, "GPT Terra");
    assert_eq!(series.model, "gpt-6.1-terra");
    assert_eq!(series.capability_order, Some(2));
    assert_eq!(
        access.recommended_model("openai-codex"),
        Some("gpt-6.1-sol")
    );
    assert_eq!(
        access
            .provider("openai-codex")
            .unwrap()
            .auxiliary_model
            .as_deref(),
        Some("gpt-6-luna")
    );
    assert_eq!(
        visible
            .iter()
            .filter(|profile| profile.provider_id == "openai-codex")
            .count(),
        4
    );
}

#[test]
fn resume_keeps_legacy_api_key_check_without_requiring_a_codex_login() {
    let mut api = built_in_model_profiles().remove(0);
    api.api_key_env = format!("TICKLOOP_ABSENT_{}", uuid::Uuid::new_v4().simple());
    assert!(validate_model_config(&api).is_ok());
    assert!(validate_model_for_resume(&api).is_err());

    let codex = built_in_model_profiles()
        .into_iter()
        .find(|profile| profile.provider == ModelProvider::OpenAiCodex)
        .unwrap();
    assert!(validate_model_for_resume(&codex).is_ok());
}
