//! The failure of a native terminal host or the Tickhost, shared by their clients.

use serde::{Deserialize, Serialize};

/// A native host failure. `dispatched` says whether the host may have acted: a dispatched
/// mutation failure is OutcomeUnknown, and only the original operation can be queried.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct HostError {
    pub code: String,
    pub message: String,
    pub dispatched: bool,
}

impl HostError {
    pub fn before(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            dispatched: false,
        }
    }
    pub fn after(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            dispatched: true,
        }
    }
}
