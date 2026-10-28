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
    /// The user's `shiftTabModes` setting chooses which of these the cycle visits. Changing the
    /// mode keeps the prompt cache: a host note at the end of the conversation announces it.
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

    /// The mode leaving Plan continues in: the mode Plan was entered from, Ask when that is
    /// unknown, as Claude Code's `prePlanMode ?? "default"`. Outside Plan, the mode itself.
    pub const fn leaving_plan(self, mode_before_plan: Option<Self>) -> Self {
        match (self, mode_before_plan) {
            (Self::Plan, Some(mode)) => mode,
            (Self::Plan, None) => Self::AcceptEdits,
            (mode, _) => mode,
        }
    }

    /// Whether Auto's admission and review decide what this mode would otherwise ask about: in
    /// Auto, and in Plan with Claude Code's `useAutoModeDuringPlan`, which is on by default.
    pub const fn uses_auto_admission(self, use_auto_mode_during_plan: bool) -> bool {
        match self {
            Self::Auto => true,
            Self::Plan => use_auto_mode_during_plan,
            Self::AcceptEdits | Self::FullAccess => false,
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
    /// The user's `useAutoModeDuringPlan: false`: Plan asks a person where Auto would admit or
    /// review. Filled in from settings like [`Self::host_rules`] and never recorded.
    #[serde(skip)]
    pub plan_without_auto: bool,
    pub revision: u64,
}

impl PermissionPolicy {
    /// [`ExecutionMode::uses_auto_admission`] with the user's `useAutoModeDuringPlan`.
    pub fn uses_auto_admission(&self) -> bool {
        self.mode.uses_auto_admission(!self.plan_without_auto)
    }
}
