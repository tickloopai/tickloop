#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoRuleSource {
    Builtin { version: u64 },
    User,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AutoRule {
    pub id: String,
    pub kind: AutoRuleKind,
    pub text: String,
    pub source: AutoRuleSource,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoEnvironmentSource {
    Builtin { version: u64 },
    UserReplacement,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AutoEnvironment {
    pub text: String,
    pub source: AutoEnvironmentSource,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AutoPolicy {
    pub version: u64,
    pub rules: Vec<AutoRule>,
    pub environment: AutoEnvironment,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskScopeFinding {
    WithinTask,
    OutsideTask,
    Uncertain,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleApplicability {
    Present,
    Absent,
    Uncertain,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SoftDenyClearance {
    ExplicitConsent,
    SoftAllow(String),
    Missing,
    Uncertain,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuleFinding {
    pub id: String,
    pub applicability: RuleApplicability,
    pub clearance: Option<SoftDenyClearance>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum AutoBlockKind {
    TaskScope,
    HardDeny,
    SoftDeny,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct VerdictEvidence {
    pub task_scope: TaskScopeFinding,
    pub finding: Option<RuleFinding>,
}
