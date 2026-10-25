use crate::LoadedSettings;
use tickloop_agent_model::ModelConfigExt;
use tickloop_agent_protocol::{AgentHostOptions, ModelConfig};
use tickloop_harness::policy::{new_chat, settings::SettingsSnapshot};

pub use new_chat::NewChatSettings;

/// Reads the preferences the Console previews using the Harness's new-Chat selection rules.
pub fn new_chat_settings(
    saved: Option<&mut LoadedSettings>,
    host_options: &AgentHostOptions,
    model: &ModelConfig,
) -> NewChatSettings {
    let Some(saved) = saved else {
        return new_chat::new_chat_settings(None, host_options, model);
    };
    let execution_mode = host_options
        .execution_mode_override
        .or_else(|| saved.default_execution_mode());
    let context_window_tokens = if model.maximum_context_window_tokens().is_some() {
        None
    } else {
        saved.context_window_tokens()
    };
    let mut preferences = SettingsSnapshot {
        execution_mode,
        context_window_tokens,
        reasoning_effort: saved.reasoning_effort(),
        ..Default::default()
    };
    let mut defaults = new_chat::new_chat_settings(Some(&mut preferences), host_options, model);
    for warning in preferences.warnings {
        saved.warn(warning);
    }
    defaults.task_tools_enabled = saved.task_tools_enabled();
    defaults.thinking_enabled = saved.thinking_enabled();
    defaults
}
