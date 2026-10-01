use serde_json::json;
use tickloop_agent::model::{
    HttpModelClient, ModelAuthSource, ModelConfig, ModelProvider, Protocol,
    cards::{built_in_model_profiles, model_card},
    model_client, validate_model_config, validate_model_for_resume,
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
        .find(|profile| profile.model == "gpt-5.6-sol")
        .unwrap();
    assert_eq!(codex.provider, ModelProvider::OpenAiCodex);
    assert_eq!(codex.auth_source, ModelAuthSource::CodexSubscription);
    assert_eq!(
        codex.base_url,
        "https://chatgpt.com/backend-api/codex/responses"
    );
    assert!(model_card(codex).is_some());
    assert!(validate_model_config(codex).is_ok());
    assert!(model_client(codex.clone(), None).is_err());
    assert!(HttpModelClient::deferred(codex.clone()).is_err());

    let mut redirected = codex.clone();
    redirected.base_url = "http://127.0.0.1:1234".into();
    assert!(validate_model_config(&redirected).is_err());

    let mut mismatched_auth = codex.clone();
    mismatched_auth.auth_source = ModelAuthSource::ApiKeyEnv;
    assert!(validate_model_config(&mismatched_auth).is_err());
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
