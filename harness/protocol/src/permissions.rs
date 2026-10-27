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

    /// Shift+Tab's fixed order, like Claude Code's: Plan follows Ask and precedes Full Access.
    /// The user's `shiftTabModes` setting chooses which of these the cycle visits. Entering or
    /// leaving Plan changes the system prompt and tools, so it starts a new prompt cache.
    pub const CYCLE_ORDER: [Self; 4] =
        [Self::Auto, Self::AcceptEdits, Self::Plan, Self::FullAccess];

    /// The first of `enabled` after this mode in [`Self::CYCLE_ORDER`], wrapping around; a mode
    /// outside `enabled` moves to the next enabled one. `None` when no other mode is enabled.
    pub fn next_in(self, enabled: &[Self]) -> Option<Self> {
        let start = Self::CYCLE_ORDER
            .iter()
            .position(|mode| *mode == self)
            .expect("every execution mode is in the Shift+Tab order");
        (1..=Self::CYCLE_ORDER.len())
            .map(|offset| Self::CYCLE_ORDER[(start + offset) % Self::CYCLE_ORDER.len()])
            .find(|mode| *mode != self && enabled.contains(mode))
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
    /// Rules supplied only for this Agent launch; never restored from history.
    #[serde(skip)]
    pub launch_rules: Vec<PermissionRule>,
    #[serde(default)]
    pub project_constraints: Vec<PermissionRule>,
    #[serde(default)]
    pub managed_constraints: Vec<PermissionRule>,
    /// The rules the host imposes on every Chat, as it has them now: filled in wherever a policy
    /// is built or loaded and never recorded, so no record can carry or forge one.
    #[serde(skip)]
    pub host_rules: Vec<PermissionRule>,
    pub revision: u64,
}
