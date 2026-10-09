use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionMode {
    Plan,
    #[serde(rename = "acceptEdits", alias = "ask")]
    AcceptEdits,
    #[default]
    Auto,
    #[serde(rename = "fullAccess")]
    FullAccess,
}

impl ExecutionMode {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Plan => "Plan",
            Self::AcceptEdits => "Ask",
            Self::Auto => "Auto",
            Self::FullAccess => "Full Access",
        }
    }

    pub const fn next(self) -> Self {
        match self {
            Self::Auto => Self::AcceptEdits,
            Self::AcceptEdits => Self::FullAccess,
            Self::FullAccess | Self::Plan => Self::Auto,
        }
    }
}

impl std::fmt::Display for ExecutionMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.label())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Deny,
    Ask,
    Allow,
    Check,
}

impl Decision {
    /// The stricter of the two, ranking Deny over Ask over Check over Allow.
    pub fn stricter(self, other: Self) -> Self {
        let strictness = |decision: Self| match decision {
            Self::Allow => 0,
            Self::Check => 1,
            Self::Ask => 2,
            Self::Deny => 3,
        };
        if strictness(other) > strictness(self) {
            other
        } else {
            self
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum PermissionScope {
    Once,
    Session,
    Persistent,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PermissionRule {
    pub id: Uuid,
    pub decision: Decision,
    pub tool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub specifier: Option<String>,
    pub input: Option<Value>,
    pub loop_id: Option<Uuid>,
    pub persistent: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct PermissionPolicy {
    pub mode: ExecutionMode,
    pub rules: Vec<PermissionRule>,
    #[serde(default)]
    pub project_constraints: Vec<PermissionRule>,
    #[serde(default)]
    pub managed_constraints: Vec<PermissionRule>,
    pub revision: u64,
}
