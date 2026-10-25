use tickloop_agent_model::{ModelConfigExt, validate_reasoning_effort};
use tickloop_agent_protocol::{AgentHostOptions, ExecutionMode, ModelConfig, ReasoningEffort};

use crate::settings::SettingsSnapshot;

/// Defaults for a new root Chat from user settings and host options. Without saved settings,
/// only execution mode follows host options; other fields keep the empty Chat defaults.
/// The New Agent draft uses the same rules to preview its first creation.
pub struct NewChatSettings {
    pub task_tools_enabled: bool,
    pub execution_mode: ExecutionMode,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub thinking_enabled: bool,
    pub context_window_tokens: Option<usize>,
}

pub fn new_chat_settings(
    saved: Option<&mut SettingsSnapshot>,
    host_options: &AgentHostOptions,
    model: &ModelConfig,
) -> NewChatSettings {
    let host_mode = host_options.execution_mode_override;
    let Some(saved) = saved else {
        return NewChatSettings {
            task_tools_enabled: false,
            execution_mode: host_mode.unwrap_or(ExecutionMode::Auto),
            reasoning_effort: None,
            thinking_enabled: true,
            context_window_tokens: None,
        };
    };
    let execution_mode = host_mode
        .or(saved.execution_mode)
        .unwrap_or(ExecutionMode::Auto);
    let context_window_tokens = if model.maximum_context_window_tokens().is_some() {
        None
    } else {
        saved
            .context_window_tokens
            .filter(|tokens| model.is_usable_context_window(*tokens))
    };
    let reasoning_effort = match saved.reasoning_effort {
        Some(effort) if validate_reasoning_effort(model, Some(effort)).is_ok() => Some(effort),
        Some(effort) => {
            saved.warn(format!(
                "`effortLevel` {} is unsupported by {}; using auto",
                effort.label(),
                model.model
            ));
            None
        }
        None => None,
    };
    NewChatSettings {
        task_tools_enabled: saved.task_tools_enabled,
        execution_mode,
        reasoning_effort,
        thinking_enabled: saved.thinking_enabled,
        context_window_tokens,
    }
}
