//! Every agent/model integration test, compiled as one binary.

#[path = "all/codex_auth.rs"]
mod codex_auth;
#[path = "all/codex_models.rs"]
mod codex_models;
#[path = "all/google_models.rs"]
mod google_models;
#[path = "all/judgment.rs"]
mod judgment;
#[path = "all/minimax_models.rs"]
mod minimax_models;
#[path = "all/model_credentials.rs"]
mod model_credentials;
#[path = "all/model_messages.rs"]
mod model_messages;
#[path = "all/model_protocols.rs"]
mod model_protocols;
#[path = "all/model_seams.rs"]
mod model_seams;
#[path = "all/model_usage_observations.rs"]
mod model_usage_observations;
#[path = "all/openai_models.rs"]
mod openai_models;
#[path = "all/provider_expansion.rs"]
mod provider_expansion;
