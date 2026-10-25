use tickloop_agent_access::{AccessError, AccessErrorCode};

use tickloop_agent_runtime::AgentError;

pub(crate) fn agent_access_error(error: AgentError) -> AccessError {
    let code = match &error {
        AgentError::NotFound => AccessErrorCode::NotFound,
        AgentError::Busy => AccessErrorCode::Busy,
        AgentError::UntrustedWorkingDirectory { .. } => AccessErrorCode::UntrustedWorkingDirectory,
        AgentError::Storage(_) | AgentError::Fenced => AccessErrorCode::Storage,
        AgentError::Invalid(message) if message.starts_with("Unsupported:") => {
            AccessErrorCode::Unsupported
        }
        _ => AccessErrorCode::Invalid,
    };
    let path = if let AgentError::UntrustedWorkingDirectory { path } = &error {
        Some(path.clone())
    } else {
        None
    };
    AccessError {
        code,
        message: error.to_string(),
        latest_permissions: None,
        path,
    }
}
