mod command_selection;
pub use command_selection::shell_command_matches_pattern;

use std::path::{Path, PathBuf};

use serde_json::Value;
use uuid::Uuid;

use crate::{AUTO_ALLOWLISTED_TOOLS, BASH_TOOL_NAME, PreparedTool, WEB_FETCH_TOOL_NAME, Workspace};
use tickloop_harness_protocol::{
    Decision, ExecutionMode, PermissionPolicy, PermissionRule, PermissionRuleScope,
    PermissionRuleSource, PermissionRuleView,
};

/// Claude Code's rule content prefix for the host a `WebFetch` call fetches.
const WEB_FETCH_DOMAIN_PREFIX: &str = "domain:";

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SessionPermissionDecision {
    Allow,
    Deny,
}

impl From<SessionPermissionDecision> for Decision {
    fn from(decision: SessionPermissionDecision) -> Self {
        match decision {
            SessionPermissionDecision::Allow => Self::Allow,
            SessionPermissionDecision::Deny => Self::Deny,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExplicitPermissionDecision {
    Deny,
    Ask,
    Allow,
}

impl From<ExplicitPermissionDecision> for Decision {
    fn from(decision: ExplicitPermissionDecision) -> Self {
        match decision {
            ExplicitPermissionDecision::Deny => Self::Deny,
            ExplicitPermissionDecision::Ask => Self::Ask,
            ExplicitPermissionDecision::Allow => Self::Allow,
        }
    }
}

/// Whose rule decided a call: the host's, a managed or project constraint, or the user's own
/// rules and session decisions, or launch arguments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuleSource {
    Host,
    Managed,
    Project,
    User,
    LaunchArguments,
}

/// An explicit decision for a call and the source whose rule made it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExplicitRuleMatch {
    pub decision: ExplicitPermissionDecision,
    pub source: RuleSource,
}

/// Rule matching and mode admission for a [`PermissionPolicy`]; the policy itself lives in the
/// protocol crate.
pub trait PermissionPolicyExt {
    fn explicit_decision_with_session(
        &self,
        name: &str,
        input: &Value,
        loop_id: Uuid,
        session: Option<SessionPermissionDecision>,
    ) -> Option<Decision>;

    fn explicit_match_with_session(
        &self,
        name: &str,
        input: &Value,
        loop_id: Uuid,
        session: Option<SessionPermissionDecision>,
    ) -> Option<ExplicitPermissionDecision>;

    /// The explicit decision for a call, Deny before Ask before Allow, with the source of the rule
    /// that made it. A session decision counts as the user's; an Allow that only rules of several
    /// sources cover together counts as the user's too.
    fn explicit_rule_match(
        &self,
        name: &str,
        input: &Value,
        loop_id: Uuid,
        session: Option<SessionPermissionDecision>,
    ) -> Option<ExplicitRuleMatch>;

    /// Match MCP rules against the resolved server, including hashed model tool names.
    fn explicit_mcp_rule_match(
        &self,
        name: &str,
        server: &str,
        input: &Value,
        loop_id: Uuid,
        session: Option<SessionPermissionDecision>,
    ) -> Option<ExplicitRuleMatch>;

    fn decide(&self, operation: &PreparedTool, workspace: &Workspace, loop_id: Uuid) -> Decision;

    /// [`Self::decide_with_session`] with Claude Code 2.1.302's allowance for the current Loop's
    /// plan file, which comes before its other file checks: in every mode the file is readable
    /// and writable without asking, and in Plan it is the one writable file. Explicit Deny and
    /// Ask rules still apply to it.
    fn decide_with_plan_file(
        &self,
        operation: &PreparedTool,
        workspace: &Workspace,
        loop_id: Uuid,
        session: Option<SessionPermissionDecision>,
        plan_file: Option<&Path>,
    ) -> Decision;

    /// Applies deterministic rules before the mode's admission and Auto review.
    fn decide_with_session(
        &self,
        operation: &PreparedTool,
        workspace: &Workspace,
        loop_id: Uuid,
        session: Option<SessionPermissionDecision>,
    ) -> Decision;

    /// Claude Code `autoAllowBashIfSandboxed`: Accept Edits and Auto run a Shell whose environment
    /// reports sufficient isolation without asking or reviewing, ahead of explicit Allow rules as
    /// in Claude Code. Explicit Deny and Ask rules and safety checks are decided before it; a
    /// Shell that requires approval never qualifies.
    fn admits_by_isolation(&self, operation: &PreparedTool) -> bool;

    /// Claude Code's refusal when a Deny naming this call's rule content applies, such as
    /// `WebFetch denied access to domain:example.com.`; other denials keep the generic reason.
    fn content_denial_message(
        &self,
        name: &str,
        input: &Value,
        loop_id: Uuid,
        session: Option<SessionPermissionDecision>,
    ) -> Option<String>;
}

impl PermissionPolicyExt for PermissionPolicy {
    fn explicit_decision_with_session(
        &self,
        name: &str,
        input: &Value,
        loop_id: Uuid,
        session: Option<SessionPermissionDecision>,
    ) -> Option<Decision> {
        self.explicit_match_with_session(name, input, loop_id, session)
            .map(Decision::from)
    }

    fn explicit_match_with_session(
        &self,
        name: &str,
        input: &Value,
        loop_id: Uuid,
        session: Option<SessionPermissionDecision>,
    ) -> Option<ExplicitPermissionDecision> {
        self.explicit_rule_match(name, input, loop_id, session)
            .map(|matched| matched.decision)
    }

    fn explicit_rule_match(
        &self,
        name: &str,
        input: &Value,
        loop_id: Uuid,
        session: Option<SessionPermissionDecision>,
    ) -> Option<ExplicitRuleMatch> {
        let sources = rule_sources(self);
        for (decision, explicit) in [
            (Decision::Deny, ExplicitPermissionDecision::Deny),
            (Decision::Ask, ExplicitPermissionDecision::Ask),
        ] {
            if session.is_some_and(|session| Decision::from(session) == decision) {
                return Some(ExplicitRuleMatch {
                    decision: explicit,
                    source: RuleSource::User,
                });
            }
            if let Some((source, _)) = sources.iter().find(|(_, rules)| {
                rules
                    .iter()
                    .any(|rule| rule_matches_any(rule, decision, name, input, loop_id))
            }) {
                return Some(ExplicitRuleMatch {
                    decision: explicit,
                    source: *source,
                });
            }
        }
        let uses_auto = self.uses_auto_admission();
        let host_allows = allow_rules_cover_call(
            &admitted_allow_rules(uses_auto, RuleSource::Host, &self.host_rules),
            name,
            input,
            loop_id,
        );
        if session == Some(SessionPermissionDecision::Allow) && !host_allows {
            return Some(ExplicitRuleMatch {
                decision: ExplicitPermissionDecision::Allow,
                source: RuleSource::User,
            });
        }
        let allow_source = sources
            .iter()
            .find(|(source, rules)| {
                allow_rules_cover_call(
                    &admitted_allow_rules(uses_auto, *source, rules),
                    name,
                    input,
                    loop_id,
                )
            })
            .map(|(source, _)| *source)
            .or_else(|| {
                let combined = sources
                    .iter()
                    .flat_map(|(source, rules)| admitted_allow_rules(uses_auto, *source, rules))
                    .collect::<Vec<_>>();
                allow_rules_cover_call(&combined, name, input, loop_id).then_some(RuleSource::User)
            });
        allow_source.map(|source| ExplicitRuleMatch {
            decision: ExplicitPermissionDecision::Allow,
            source,
        })
    }

    fn explicit_mcp_rule_match(
        &self,
        name: &str,
        server: &str,
        input: &Value,
        loop_id: Uuid,
        session: Option<SessionPermissionDecision>,
    ) -> Option<ExplicitRuleMatch> {
        let mut policy = self.clone();

        // Resolve service rules before generic matching: display names may be hashed, and
        // underscores in service names must not grant a different service's authority.
        for rules in [
            &mut policy.rules,
            &mut policy.launch_rules,
            &mut policy.host_rules,
            &mut policy.project_constraints,
            &mut policy.managed_constraints,
        ] {
            rules.retain_mut(|rule| {
                if !rule.tool.starts_with("mcp__") {
                    return true;
                }
                if !mcp_tool_rule_matches(&rule.tool, name, server) {
                    return false;
                }
                rule.tool = name.to_owned();
                true
            });
        }
        policy.explicit_rule_match(name, input, loop_id, session)
    }

    fn decide(&self, operation: &PreparedTool, workspace: &Workspace, loop_id: Uuid) -> Decision {
        self.decide_with_session(operation, workspace, loop_id, None)
    }

    fn decide_with_plan_file(
        &self,
        operation: &PreparedTool,
        workspace: &Workspace,
        loop_id: Uuid,
        session: Option<SessionPermissionDecision>,
        plan_file: Option<&Path>,
    ) -> Decision {
        let is_plan_file_access = plan_file.is_some_and(|path| operation.accesses_file(path));
        if !is_plan_file_access {
            return self.decide_with_session(operation, workspace, loop_id, session);
        }
        match self.explicit_match_with_session(&operation.name, &operation.input, loop_id, session)
        {
            Some(ExplicitPermissionDecision::Deny) => Decision::Deny,
            Some(ExplicitPermissionDecision::Ask) => Decision::Ask,
            _ => Decision::Allow,
        }
    }

    fn decide_with_session(
        &self,
        operation: &PreparedTool,
        workspace: &Workspace,
        loop_id: Uuid,
        session: Option<SessionPermissionDecision>,
    ) -> Decision {
        if self.mode == ExecutionMode::Plan && operation.is_plan_mode_side_effect() {
            return Decision::Deny;
        }
        let explicit =
            self.explicit_match_with_session(&operation.name, &operation.input, loop_id, session);
        if explicit == Some(ExplicitPermissionDecision::Deny) {
            return Decision::Deny;
        }
        if explicit == Some(ExplicitPermissionDecision::Ask) {
            return Decision::Ask;
        }
        if self.admission_mode() == ExecutionMode::FullAccess {
            return Decision::Allow;
        }
        let safety_check = operation.safety_check();
        let is_accept_edits = self.mode == ExecutionMode::AcceptEdits;
        let decision = if safety_check.is_some() {
            Decision::Ask
        } else if self.admits_by_isolation(operation)
            || explicit == Some(ExplicitPermissionDecision::Allow)
        {
            Decision::Allow
        } else if operation.requires_environment_approval() && self.mode != ExecutionMode::Plan {
            Decision::Ask
        } else if is_accept_edits && operation.is_allowed_in_accept_edits(workspace) {
            Decision::Allow
        } else if is_accept_edits && operation.accept_edits_supersedes_read_only() {
            Decision::Ask
        } else if !operation.requires_permission {
            Decision::Allow
        } else {
            Decision::Ask
        };
        if !self.uses_auto_admission() || decision != Decision::Ask {
            return decision;
        }
        if safety_check
            .as_ref()
            .is_some_and(|check| !check.classifier_approvable)
        {
            return Decision::Ask;
        }
        let accept_edits_admits = operation.is_allowed_in_accept_edits(workspace)
            || operation.is_allowed_in_accept_edits_with_sibling_worktrees(workspace)
            || !operation.accept_edits_supersedes_read_only() && !operation.requires_permission;
        let passes_fast_path = !operation.requires_environment_approval()
            && safety_check.is_none()
            && !content_rule_asks(self, &operation.name, &operation.input, loop_id)
            && accept_edits_admits;
        if passes_fast_path || AUTO_ALLOWLISTED_TOOLS.contains(&operation.name.as_str()) {
            Decision::Allow
        } else {
            Decision::Check
        }
    }

    fn admits_by_isolation(&self, operation: &PreparedTool) -> bool {
        matches!(self.mode, ExecutionMode::AcceptEdits | ExecutionMode::Auto)
            && operation.skips_approval_by_isolation()
    }

    fn content_denial_message(
        &self,
        name: &str,
        input: &Value,
        loop_id: Uuid,
        session: Option<SessionPermissionDecision>,
    ) -> Option<String> {
        let content = permission_rule_content(name, input)?;
        let is_denied_by_content = session == Some(SessionPermissionDecision::Deny)
            || rule_sources(self)
                .into_iter()
                .flat_map(|(_, rules)| rules)
                .filter(|rule| rule.specifier.is_some())
                .any(|rule| rule_matches_any(rule, Decision::Deny, name, input, loop_id));
        is_denied_by_content.then(|| format!("{name} denied access to {content}."))
    }
}

/// Whether an Ask rule scoped to this call's content matches. Claude Code
/// evaluates such rules inside the tool's own check, so they also stop the
/// Accept Edits fast path, unlike whole-tool Ask rules.
fn content_rule_asks(policy: &PermissionPolicy, name: &str, input: &Value, loop_id: Uuid) -> bool {
    rule_sources(policy)
        .into_iter()
        .flat_map(|(_, rules)| rules)
        .filter(|rule| rule.specifier.is_some() || rule.input.is_some())
        .any(|rule| rule_matches_any(rule, Decision::Ask, name, input, loop_id))
}

/// Every rule the policy evaluates, by source: the host's first, then the managed and project
/// constraints, then the user's rules and launch arguments.
fn rule_sources(policy: &PermissionPolicy) -> [(RuleSource, &[PermissionRule]); 5] {
    [
        (RuleSource::Host, &policy.host_rules),
        (RuleSource::Managed, &policy.managed_constraints),
        (RuleSource::Project, &policy.project_constraints),
        (RuleSource::User, &policy.rules),
        (RuleSource::LaunchArguments, &policy.launch_rules),
    ]
}

/// The rules of `source` whose Allow counts: Claude Code sets aside the user's and the project's
/// broad Allow rules while Auto's admission is on, in Auto and in Plan with Auto.
fn admitted_allow_rules(
    uses_auto: bool,
    source: RuleSource,
    rules: &[PermissionRule],
) -> Vec<&PermissionRule> {
    rules
        .iter()
        .filter(|rule| {
            matches!(source, RuleSource::Host | RuleSource::Managed)
                || !uses_auto
                || !bypasses_auto_classifier(rule)
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionRuleCandidate {
    pub label: String,
    pub specifier: String,
}

/// The content a rule must name exactly to match a call of a tool whose rules name one aspect of
/// its input. A `WebFetch` call is identified by the host it fetches, whatever its path and
/// prompt (Claude Code `webFetchToolInputToPermissionRuleContent`).
pub fn permission_rule_content(name: &str, input: &Value) -> Option<String> {
    if name != WEB_FETCH_TOOL_NAME {
        return None;
    }
    let url = reqwest::Url::parse(input.get("url")?.as_str()?).ok()?;
    Some(format!("{WEB_FETCH_DOMAIN_PREFIX}{}", url.host_str()?))
}

/// Whether `specifier` is rule content `tool` can match: a Bash command pattern, or a `WebFetch`
/// host as `domain:{hostname}`.
pub fn is_supported_rule_content(tool: &str, specifier: &str) -> bool {
    match tool {
        BASH_TOOL_NAME | "*" => true,
        WEB_FETCH_TOOL_NAME => specifier
            .strip_prefix(WEB_FETCH_DOMAIN_PREFIX)
            .is_some_and(|host| !host.trim().is_empty()),
        _ => false,
    }
}

/// Why `rule` cannot be saved or staged, checked before any policy changes.
pub fn validate_rule(rule: &PermissionRule) -> Result<(), &'static str> {
    if rule.tool.is_empty() || rule.decision == Decision::Check {
        return Err("rules require a tool and allow, ask or deny decision");
    }
    if let Some(target) = rule.tool.strip_prefix("mcp__") {
        let target = target.strip_suffix("__*").unwrap_or(target);
        if target.is_empty()
            || !target
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(
                "MCP rules require a server or tool name, with only a trailing __* server wildcard",
            );
        }
    }
    if rule.persistent && rule.loop_id.is_some() {
        return Err("project rules cannot name a Loop");
    }
    if rule
        .specifier
        .as_deref()
        .is_some_and(|specifier| !is_supported_rule_content(&rule.tool, specifier))
    {
        return Err(
            "rule content is supported only for Bash command patterns and WebFetch domain:{hostname}",
        );
    }
    Ok(())
}

/// How the Permissions view lists rules, for a visible Agent and for the New Agent screen.
pub fn rule_views(
    rules: &[PermissionRule],
    rule_project: impl Fn(&PermissionRule) -> PathBuf,
    workdir: &Path,
    uses_auto_admission: bool,
    project_constraints: &[PermissionRule],
    managed_constraints: &[PermissionRule],
    host_rules: &[PermissionRule],
) -> Vec<PermissionRuleView> {
    rules
        .iter()
        .map(|rule| PermissionRuleView {
            rule: rule.clone(),
            source: if rule.persistent {
                PermissionRuleSource::ProjectUser
            } else {
                PermissionRuleSource::ChatSession
            },
            scope: if rule.persistent {
                PermissionRuleScope::Project(rule_project(rule))
            } else if let Some(loop_id) = rule.loop_id {
                PermissionRuleScope::Loop(loop_id)
            } else {
                PermissionRuleScope::Chat
            },
            editable: true,
            effective_note: (uses_auto_admission && bypasses_auto_classifier(rule))
                .then(|| "broad Allow does not bypass Auto review".into()),
        })
        .chain(project_constraints.iter().map(|rule| PermissionRuleView {
            rule: rule.clone(),
            source: PermissionRuleSource::ProjectConstraint,
            scope: PermissionRuleScope::Project(workdir.to_path_buf()),
            editable: false,
            effective_note: Some("project constraint cannot be edited here".into()),
        }))
        .chain(managed_constraints.iter().map(|rule| PermissionRuleView {
            rule: rule.clone(),
            source: PermissionRuleSource::ManagedConstraint,
            scope: PermissionRuleScope::Project(workdir.to_path_buf()),
            editable: false,
            effective_note: Some("managed constraint cannot be edited here".into()),
        }))
        .chain(host_rules.iter().map(|rule| PermissionRuleView {
            rule: rule.clone(),
            source: PermissionRuleSource::Host,
            scope: PermissionRuleScope::Chat,
            editable: false,
            effective_note: Some("the host's rule cannot be edited".into()),
        }))
        .collect()
}

/// Launch rules appear separately from editable and persistent user rules.
pub fn launch_rule_views(
    rules: &[PermissionRule],
    uses_auto_admission: bool,
) -> impl Iterator<Item = PermissionRuleView> + '_ {
    rules.iter().map(move |rule| PermissionRuleView {
        rule: rule.clone(),
        source: PermissionRuleSource::LaunchArguments,
        scope: PermissionRuleScope::Chat,
        editable: false,
        effective_note: Some(
            if uses_auto_admission && bypasses_auto_classifier(rule) {
                "launch argument; broad Allow does not bypass Auto review"
            } else {
                "launch argument; cannot be edited or saved"
            }
            .into(),
        ),
    })
}

/// An additional directory as permissions record it: canonical, and an existing directory.
pub fn canonical_directory(path: PathBuf) -> Result<PathBuf, &'static str> {
    let path = path
        .canonicalize()
        .map_err(|_| "directory is unavailable")?;
    if !path.is_dir() {
        return Err("path is not a directory");
    }
    Ok(path)
}

pub fn permission_rule_candidates(name: &str, input: &Value) -> Vec<PermissionRuleCandidate> {
    if let Some(content) = permission_rule_content(name, input) {
        return vec![PermissionRuleCandidate {
            label: format!("{name}({content})"),
            specifier: content,
        }];
    }
    if name != BASH_TOOL_NAME {
        return Vec::new();
    }
    let Some(command) = input.get("command").and_then(Value::as_str) else {
        return Vec::new();
    };
    let command = command.trim();
    if command.is_empty() || command.contains('*') {
        return Vec::new();
    }
    let mut specifiers = vec![command.to_owned()];
    if split_shell_commands(command).is_some_and(|commands| commands.len() == 1)
        && let Some(token_ends) = shell_token_ends(command)
    {
        for end in token_ends.into_iter().rev().skip(1) {
            let prefix = command[..end].trim_end();
            if !prefix.is_empty() {
                specifiers.push(format!("{prefix} *"));
            }
        }
    }
    specifiers.dedup();
    specifiers
        .into_iter()
        .map(|specifier| PermissionRuleCandidate {
            label: format!("Bash({specifier})"),
            specifier,
        })
        .collect()
}

/// Shell entry points that run arbitrary code, from Claude Code's
/// `DANGEROUS_BASH_PATTERNS`.
const CODE_EXECUTION_COMMANDS: [&str; 26] = [
    "python", "python3", "python2", "node", "deno", "tsx", "ruby", "perl", "php", "lua", "npx",
    "bunx", "npm run", "yarn run", "pnpm run", "bun run", "bash", "sh", "ssh", "zsh", "fish",
    "eval", "exec", "env", "xargs", "sudo",
];

/// Whether an Allow rule would let the Agent run arbitrary code without the
/// Auto classifier. Claude Code strips such user and project rules while auto
/// mode is active and restores them afterwards; managed rules stay in force.
pub fn bypasses_auto_classifier(rule: &PermissionRule) -> bool {
    if rule.decision != Decision::Allow || !matches!(rule.tool.as_str(), BASH_TOOL_NAME | "*") {
        return false;
    }
    let Some(specifier) = rule.specifier.as_deref() else {
        return rule.input.is_none();
    };
    let content = specifier.trim().to_lowercase();
    content.is_empty()
        || content == "*"
        || CODE_EXECUTION_COMMANDS.iter().any(|command| {
            content == *command
                || content == format!("{command}:*")
                || content == format!("{command}*")
                || content == format!("{command} *")
                || (content.starts_with(&format!("{command} -")) && content.ends_with('*'))
        })
}

// Persisted permission rules keep their original authority across the Tick API rename. These
// names are policy projections only; model calls never dispatch through the old tool names.
fn previous_tick_tool_name(name: &str, input: &Value) -> Option<&'static str> {
    match name {
        "TickDescribe" | "TickRead" => Some("ReadTick"),
        "TickCall" => Some("CallTickTool"),
        "TickManage" => match input["action"].as_str() {
            Some("subscribe") => Some("SubscribeTick"),
            Some("unsubscribe") => Some("UnsubscribeTick"),
            Some("open" | "close" | "reopen" | "send_input") => Some("ModifyTick"),
            _ => None,
        },
        _ => None,
    }
}

fn rule_matches_any(
    rule: &PermissionRule,
    decision: Decision,
    name: &str,
    input: &Value,
    loop_id: Uuid,
) -> bool {
    if let Some(previous) = previous_tick_tool_name(name, input)
        && (rule.tool == previous
            || (rule.tool == "*"
                && rule
                    .input
                    .as_ref()
                    .is_some_and(|expected| expected.get("action").is_none())))
    {
        let mut previous_input = input.clone();
        if matches!(previous, "SubscribeTick" | "UnsubscribeTick")
            && let Some(fields) = previous_input.as_object_mut()
        {
            fields.remove("action");
        }
        return rule_matches_any(rule, decision, previous, &previous_input, loop_id);
    }
    rule.decision == decision
        && rule_applies_to_tool_and_goal(rule, name, loop_id)
        && match &rule.specifier {
            Some(specifier) if name == BASH_TOOL_NAME => shell_rule_matches_any(specifier, input),
            Some(specifier) => {
                permission_rule_content(name, input).is_some_and(|content| content == *specifier)
            }
            None => rule.input.as_ref().is_none_or(|expected| expected == input),
        }
}

pub fn rule_matches_call(rule: &PermissionRule, name: &str, input: &Value, loop_id: Uuid) -> bool {
    rule_matches_any(rule, rule.decision, name, input, loop_id)
}

fn mcp_tool_rule_matches(rule: &str, name: &str, server: &str) -> bool {
    rule == name || rule == format!("mcp__{server}") || rule == format!("mcp__{server}__*")
}

/// Match the original rule against the same resolved service used for admission.
pub fn mcp_rule_matches_call(
    rule: &PermissionRule,
    name: &str,
    server: &str,
    input: &Value,
    loop_id: Uuid,
) -> bool {
    if !rule.tool.starts_with("mcp__") {
        return rule_matches_call(rule, name, input, loop_id);
    }
    if !mcp_tool_rule_matches(&rule.tool, name, server) {
        return false;
    }
    let mut resolved = rule.clone();
    resolved.tool = name.into();
    rule_matches_call(&resolved, name, input, loop_id)
}

/// The tool and content of a Permission Rule written as Claude Code writes one, `Tool` or
/// `Tool(content)`, with `\(` and `\)` standing for parentheses in the content; `Tool()` and
/// `Tool(*)` name the whole tool. `None` when the text is not a rule.
pub fn parse_rule_text(text: &str) -> Option<(String, Option<String>)> {
    let text = text.trim();
    let Some(open) = text.find('(') else {
        return (!text.is_empty() && !text.contains(')')).then(|| (text.to_owned(), None));
    };
    let tool = text[..open].trim();
    let content = text[open + 1..].strip_suffix(')')?;
    if tool.is_empty() || tool.contains(char::is_whitespace) {
        return None;
    }
    let content = content.replace(r"\(", "(").replace(r"\)", ")");
    let content = content.trim();
    Some((
        tool.to_owned(),
        (!content.is_empty() && content != "*").then(|| content.to_owned()),
    ))
}

/// Whether `text` is a rule whose content Tickloop can match: a Bash command pattern, a `WebFetch`
/// domain or a whole tool. Path patterns of the file tools are not matched by any rule.
pub fn is_supported_rule_text(text: &str) -> bool {
    parse_rule_text(text).is_some_and(|(tool, content)| {
        content.is_none_or(|content| is_supported_rule_content(&tool, &content))
    })
}

/// Whether a restriction written as `rule` may apply to the call, failing closed: as Deny and Ask
/// rules match, a compound command matches when any of its commands does, here with its
/// redirections set aside, and a command whose commands cannot be told apart matches whatever
/// the pattern. An MCP rule names its service by the call's own name, and a call whose service
/// that name does not show matches too.
pub fn restriction_may_match_call(
    rule: &PermissionRule,
    name: &str,
    input: &Value,
    loop_id: Uuid,
) -> bool {
    if rule.tool.starts_with("mcp__") && name.starts_with("mcp__") {
        let Some(server) = name
            .strip_prefix("mcp__")
            .and_then(|rest| rest.split_once("__"))
            .map(|(server, _)| server)
            .filter(|server| !server.is_empty())
        else {
            return true;
        };
        return mcp_rule_matches_call(rule, name, server, input, loop_id);
    }
    if name != BASH_TOOL_NAME || !rule_applies_to_tool_and_goal(rule, name, loop_id) {
        return rule_matches_any(rule, rule.decision, name, input, loop_id);
    }
    let Some(pattern) = rule.specifier.as_deref() else {
        return rule_matches_any(rule, rule.decision, name, input, loop_id);
    };
    let Some(command) = input.get("command").and_then(Value::as_str) else {
        return true;
    };
    // A substitution runs a command of its own, quoted or not.
    if shell_rule_matches_any(pattern, input) || command.contains("$(") || command.contains('`') {
        return true;
    }
    let commands = match split_shell_commands(command) {
        Some(commands) => commands.into_iter().map(str::to_owned).collect::<Vec<_>>(),
        None => {
            let Some(parts) = split_shell_parts(command, true) else {
                return true;
            };
            let commands = parts
                .iter()
                .map(|part| without_redirections(part))
                .collect::<Vec<_>>();
            if commands
                .iter()
                .any(|command| command.contains(['(', ')', '{', '}', '<', '>']))
            {
                return true;
            }
            commands
        }
    };
    commands.iter().any(|command| {
        bash_pattern_matches(pattern, command)
            || bash_pattern_matches(pattern, &without_wrappers(command))
    })
}

/// A simple command as the program it runs sees it: without leading variable assignments, the
/// `env` and `command` wrappers and their options, and with the program named without its
/// directory.
fn without_wrappers(command: &str) -> String {
    let is_assignment = |token: &str| {
        token.split_once('=').is_some_and(|(name, _)| {
            !name.is_empty()
                && !name.starts_with(|character: char| character.is_ascii_digit())
                && name
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '_')
        })
    };
    let mut tokens = command.split_whitespace().peekable();
    loop {
        match tokens.peek().copied() {
            Some(token) if is_assignment(token) => {}
            Some("env" | "command") => {
                tokens.next();
                while tokens
                    .peek()
                    .is_some_and(|token| token.starts_with('-') || is_assignment(token))
                {
                    tokens.next();
                }
                continue;
            }
            _ => break,
        }
        tokens.next();
    }
    let mut words = Vec::new();
    if let Some(program) = tokens.next() {
        words.push(program.rsplit('/').next().unwrap_or(program));
    }
    words.extend(tokens);
    words.join(" ")
}

/// A simple command with its redirections, such as `> out.txt` or `2>&1`, left out.
fn without_redirections(command: &str) -> String {
    let is_redirection = |token: &str| {
        let operator = token.trim_start_matches(|character: char| character.is_ascii_digit());
        operator.starts_with(['<', '>']) || operator.starts_with("&>")
    };
    let mut kept = Vec::new();
    let mut tokens = command.split_whitespace();
    while let Some(token) = tokens.next() {
        if !is_redirection(token) {
            kept.push(token);
            continue;
        }
        let operator = token.trim_start_matches(|character: char| character.is_ascii_digit());
        if operator.trim_end_matches(['<', '>', '&', '|']).is_empty() {
            tokens.next();
        }
    }
    kept.join(" ")
}

fn allow_rules_cover_call(
    rules: &[&PermissionRule],
    name: &str,
    input: &Value,
    loop_id: Uuid,
) -> bool {
    let allow_rules = rules
        .iter()
        .copied()
        .filter(|rule| {
            rule.decision == Decision::Allow
                && (rule_applies_to_tool_and_goal(rule, name, loop_id)
                    || previous_tick_tool_name(name, input).is_some_and(|previous| {
                        rule_applies_to_tool_and_goal(rule, previous, loop_id)
                    }))
        })
        .collect::<Vec<_>>();
    if allow_rules.iter().any(|rule| {
        rule.specifier.is_none() && rule_matches_any(rule, Decision::Allow, name, input, loop_id)
    }) {
        return true;
    }
    if let Some(content) = permission_rule_content(name, input) {
        return allow_rules
            .iter()
            .any(|rule| rule.specifier.as_deref() == Some(content.as_str()));
    }
    if name != BASH_TOOL_NAME {
        return false;
    }
    let Some(command) = input.get("command").and_then(Value::as_str) else {
        return false;
    };
    if allow_rules.iter().any(|rule| {
        rule.specifier
            .as_deref()
            .is_some_and(|pattern| !pattern.contains('*') && pattern == command.trim())
    }) {
        return true;
    }
    let Some(commands) = split_shell_commands(command) else {
        return false;
    };
    !commands.is_empty()
        && commands.iter().all(|command| {
            allow_rules.iter().any(|rule| {
                rule.specifier
                    .as_deref()
                    .is_some_and(|pattern| bash_pattern_matches(pattern, command))
            })
        })
}

fn rule_applies_to_tool_and_goal(rule: &PermissionRule, name: &str, loop_id: Uuid) -> bool {
    (rule.tool == name || rule.tool == "*") && rule.loop_id.is_none_or(|id| id == loop_id)
}

fn shell_rule_matches_any(pattern: &str, input: &Value) -> bool {
    let Some(command) = input.get("command").and_then(Value::as_str) else {
        return false;
    };
    if !pattern.contains('*') && pattern == command {
        return true;
    }
    split_shell_commands(command).is_some_and(|commands| {
        commands
            .iter()
            .any(|command| bash_pattern_matches(pattern, command))
    })
}

fn bash_pattern_matches(pattern: &str, command: &str) -> bool {
    let normalized = pattern
        .strip_suffix(":*")
        .map_or_else(|| pattern.to_owned(), |prefix| format!("{prefix} *"));
    if let Some(prefix) = normalized.strip_suffix(" *")
        && !prefix.contains('*')
        && command == prefix
    {
        return true;
    }
    wildcard_matches(&normalized, command)
}

fn wildcard_matches(pattern: &str, value: &str) -> bool {
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    let (mut pattern_index, mut value_index) = (0, 0);
    let (mut wildcard_index, mut wildcard_value_index) = (None, 0);
    while value_index < value.len() {
        if pattern.get(pattern_index) == Some(&value[value_index]) {
            pattern_index += 1;
            value_index += 1;
        } else if pattern.get(pattern_index) == Some(&b'*') {
            wildcard_index = Some(pattern_index);
            pattern_index += 1;
            wildcard_value_index = value_index;
        } else if let Some(index) = wildcard_index {
            pattern_index = index + 1;
            wildcard_value_index += 1;
            value_index = wildcard_value_index;
        } else {
            return false;
        }
    }
    while pattern.get(pattern_index) == Some(&b'*') {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

fn split_shell_commands(command: &str) -> Option<Vec<&str>> {
    split_shell_parts(command, false)
}

fn split_shell_parts(command: &str, allow_special_syntax: bool) -> Option<Vec<&str>> {
    let bytes = command.as_bytes();
    let mut commands = Vec::new();
    let mut start = 0;
    let mut quote = None;
    let mut escaped = false;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if escaped {
            escaped = false;
            index += 1;
            continue;
        }
        if byte == b'\\' && quote != Some(b'\'') {
            escaped = true;
            index += 1;
            continue;
        }
        if matches!(byte, b'\'' | b'"') {
            if quote == Some(byte) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(byte);
            }
            index += 1;
            continue;
        }
        if quote.is_none() {
            if !allow_special_syntax
                && (matches!(byte, b'`' | b'(' | b')' | b'{' | b'}' | b'<' | b'>')
                    || (byte == b'$' && bytes.get(index + 1) == Some(&b'(')))
            {
                return None;
            }
            let separator_length = match (byte, bytes.get(index + 1).copied()) {
                (b'&', Some(b'&')) | (b'|', Some(b'|')) | (b'|', Some(b'&')) => 2,
                (b';' | b'|' | b'&' | b'\n', _) => 1,
                _ => 0,
            };
            if separator_length > 0 {
                let part = command[start..index].trim();
                if part.is_empty() {
                    return None;
                }
                commands.push(part);
                index += separator_length;
                start = index;
                continue;
            }
        }
        index += 1;
    }
    if escaped || quote.is_some() {
        return None;
    }
    let part = command[start..].trim();
    if part.is_empty() {
        return None;
    }
    commands.push(part);
    Some(commands)
}

fn shell_token_ends(command: &str) -> Option<Vec<usize>> {
    let bytes = command.as_bytes();
    let mut ends = Vec::new();
    let mut quote = None;
    let mut escaped = false;
    let mut in_token = false;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if escaped {
            escaped = false;
            in_token = true;
            index += 1;
            continue;
        }
        if byte == b'\\' && quote != Some(b'\'') {
            escaped = true;
            in_token = true;
            index += 1;
            continue;
        }
        if matches!(byte, b'\'' | b'"') {
            if quote == Some(byte) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(byte);
            }
            in_token = true;
            index += 1;
            continue;
        }
        if quote.is_none() && byte.is_ascii_whitespace() {
            if in_token {
                ends.push(index);
                in_token = false;
            }
        } else {
            in_token = true;
        }
        index += 1;
    }
    if escaped || quote.is_some() {
        return None;
    }
    if in_token {
        ends.push(command.len());
    }
    (!ends.is_empty()).then_some(ends)
}

#[cfg(test)]
mod restriction_tests {
    use super::*;
    use serde_json::json;

    fn deny(tool: &str, specifier: Option<&str>) -> PermissionRule {
        PermissionRule {
            id: Uuid::nil(),
            decision: Decision::Deny,
            tool: tool.into(),
            specifier: specifier.map(Into::into),
            input: None,
            loop_id: None,
            persistent: false,
        }
    }

    fn bash_matches(pattern: &str, command: &str) -> bool {
        restriction_may_match_call(
            &deny(BASH_TOOL_NAME, Some(pattern)),
            BASH_TOOL_NAME,
            &json!({ "command": command }),
            Uuid::nil(),
        )
    }

    #[test]
    fn rule_text_names_a_tool_and_its_content() {
        assert_eq!(
            parse_rule_text("Bash(git push*)"),
            Some(("Bash".into(), Some("git push*".into())))
        );
        assert_eq!(parse_rule_text("Bash"), Some(("Bash".into(), None)));
        assert_eq!(parse_rule_text("Bash(*)"), Some(("Bash".into(), None)));
        assert_eq!(
            parse_rule_text(r"Bash(echo \(x\))"),
            Some(("Bash".into(), Some("echo (x)".into())))
        );
        assert_eq!(parse_rule_text("Bash(git push*"), None);
        assert_eq!(parse_rule_text("(git push)"), None);
        assert_eq!(parse_rule_text(""), None);
        assert!(is_supported_rule_text("Bash(git push*)"));
        assert!(is_supported_rule_text("WebFetch(domain:example.com)"));
        assert!(is_supported_rule_text("Edit"));
        assert!(!is_supported_rule_text("Edit(src/**)"));
        assert!(!is_supported_rule_text("WebFetch(https://example.com)"));
        assert!(!is_supported_rule_text("Bash(git push*"));
    }

    #[test]
    fn a_compound_command_matches_when_any_of_its_commands_does() {
        assert!(bash_matches("git push*", "git push origin main"));
        assert!(bash_matches("git push*", "cd repo && git push"));
        assert!(bash_matches("git push*", "cargo test; git push --force"));
        assert!(!bash_matches(
            "git push*",
            "git commit -m done && cargo test"
        ));
    }

    #[test]
    fn redirections_are_set_aside_and_what_cannot_be_told_apart_matches() {
        assert!(bash_matches(
            "git push*",
            "git push origin main > push.log 2>&1"
        ));
        assert!(bash_matches(
            "git push*",
            "cargo build > build.log && git push"
        ));
        assert!(!bash_matches("git push*", "cargo build > build.log"));
        assert!(bash_matches("git push*", "echo $(git push)"));
        assert!(bash_matches("git push*", "(cd repo && git push)"));
        assert!(bash_matches("git push*", "echo 'unclosed"));
    }

    #[test]
    fn substitutions_anywhere_cannot_be_judged_and_match() {
        assert!(bash_matches("git push*", r#"echo "$(git push)""#));
        assert!(bash_matches("git push*", "echo \"`git push`\""));
        assert!(bash_matches("git push*", "echo `date`"));
    }

    #[test]
    fn wrappers_assignments_and_program_paths_do_not_hide_a_command() {
        for command in [
            "env X=1 git push",
            "env -i PATH=/bin git push origin",
            "command git push",
            "command -p git push",
            "/usr/bin/git push",
            "X=1 Y=2 git push",
            "cargo test && FOO=bar /opt/homebrew/bin/git push",
        ] {
            assert!(bash_matches("git push*", command), "{command}");
        }
        assert!(!bash_matches("git push*", "env X=1 git status"));
        assert!(!bash_matches("git push*", "/usr/bin/git status"));
    }

    #[test]
    fn the_same_commands_keep_their_existing_deny_rule_matching() {
        let rule = deny(BASH_TOOL_NAME, Some("git push*"));
        for (command, matches) in [
            ("git push", true),
            ("cargo test && git push", true),
            ("env X=1 git push", false),
            ("/usr/bin/git push", false),
        ] {
            assert_eq!(
                rule_matches_call(
                    &rule,
                    BASH_TOOL_NAME,
                    &json!({ "command": command }),
                    Uuid::nil()
                ),
                matches,
                "{command}"
            );
        }
    }

    #[test]
    fn mcp_restrictions_name_the_service_from_the_call() {
        let call = |rule: &str, name: &str| {
            restriction_may_match_call(&deny(rule, None), name, &json!({}), Uuid::nil())
        };
        assert!(call("mcp__github", "mcp__github__create_issue"));
        assert!(call("mcp__github__*", "mcp__github__create_issue"));
        assert!(call(
            "mcp__github__create_issue",
            "mcp__github__create_issue"
        ));
        assert!(!call("mcp__github", "mcp__slack__post"));
        assert!(call("mcp__github", "mcp__hashedname"));
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn prepared_tool(directory: &std::path::Path) -> (Workspace, PreparedTool) {
        let workspace = Workspace {
            folder: Some(directory.to_path_buf()),
            additional_directories: Vec::new(),
        };
        let operation = crate::prepare(
            &workspace,
            "Write",
            &json!({"file_path":"report.txt","content":"approved"}),
        )
        .unwrap();
        (workspace, operation)
    }

    fn prepare_in(workspace: &Workspace, name: &str, input: Value) -> PreparedTool {
        crate::prepare(workspace, name, &input).unwrap()
    }

    fn tool_rule(decision: Decision, tool: &str, input: Option<Value>) -> PermissionRule {
        PermissionRule {
            id: Uuid::new_v4(),
            decision,
            tool: tool.into(),
            specifier: None,
            input,
            loop_id: None,
            persistent: false,
        }
    }

    /// The host's rules are a source of their own: they decide like any rule, say which source
    /// decided, and in Auto keep a broad Allow the user's rules would lose.
    #[test]
    fn host_rules_decide_as_their_own_source_and_user_denials_still_tighten_them() {
        let loop_id = Uuid::new_v4();
        let input = json!({"task":"check"});
        let host_allow = PermissionPolicy {
            host_rules: vec![tool_rule(Decision::Allow, "Subagent", None)],
            ..PermissionPolicy::default()
        };
        assert_eq!(
            host_allow.explicit_rule_match("Subagent", &input, loop_id, None),
            Some(ExplicitRuleMatch {
                decision: ExplicitPermissionDecision::Allow,
                source: RuleSource::Host,
            })
        );
        let user_allow = PermissionPolicy {
            rules: vec![tool_rule(Decision::Allow, "Subagent", None)],
            ..PermissionPolicy::default()
        };
        assert_eq!(
            user_allow
                .explicit_rule_match("Subagent", &input, loop_id, None)
                .map(|matched| matched.source),
            Some(RuleSource::User)
        );
        let user_deny = PermissionPolicy {
            rules: vec![tool_rule(Decision::Deny, "Subagent", None)],
            ..host_allow.clone()
        };
        assert_eq!(
            user_deny.explicit_rule_match("Subagent", &input, loop_id, None),
            Some(ExplicitRuleMatch {
                decision: ExplicitPermissionDecision::Deny,
                source: RuleSource::User,
            })
        );
        assert_eq!(
            host_allow.explicit_rule_match(
                "Subagent",
                &input,
                loop_id,
                Some(SessionPermissionDecision::Deny)
            ),
            Some(ExplicitRuleMatch {
                decision: ExplicitPermissionDecision::Deny,
                source: RuleSource::User,
            })
        );
        assert_eq!(
            host_allow
                .explicit_rule_match(
                    "Subagent",
                    &input,
                    loop_id,
                    Some(SessionPermissionDecision::Allow)
                )
                .map(|matched| matched.source),
            Some(RuleSource::Host),
            "a session Allow does not hide the host's"
        );
        let broad_bash = PermissionPolicy {
            mode: ExecutionMode::Auto,
            host_rules: vec![tool_rule(Decision::Allow, BASH_TOOL_NAME, None)],
            ..PermissionPolicy::default()
        };
        let command = json!({"command":"python script.py"});
        assert_eq!(
            broad_bash
                .explicit_rule_match(BASH_TOOL_NAME, &command, loop_id, None)
                .map(|matched| matched.source),
            Some(RuleSource::Host)
        );
        let user_broad_bash = PermissionPolicy {
            rules: broad_bash.host_rules.clone(),
            host_rules: Vec::new(),
            ..broad_bash.clone()
        };
        assert_eq!(
            user_broad_bash.explicit_rule_match(BASH_TOOL_NAME, &command, loop_id, None),
            None
        );
        assert_eq!(
            PermissionPolicy::default().explicit_rule_match("Subagent", &input, loop_id, None),
            None
        );
    }

    #[test]
    fn auto_keeps_explicit_ask_ahead_of_fast_paths_and_reviews_unmatched_calls() {
        let directory = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join(".git")).unwrap();
        std::fs::write(external.path().join("notes.txt"), "notes").unwrap();
        let (workspace, workspace_write) = prepared_tool(directory.path());
        let git_write = prepare_in(
            &workspace,
            "Write",
            json!({"file_path":".git/config","content":"x"}),
        );
        let external_write = prepare_in(
            &workspace,
            "Write",
            json!({"file_path":external.path().join("out.txt"),"content":"x"}),
        );
        let external_read = prepare_in(
            &workspace,
            "Read",
            json!({"file_path":external.path().join("notes.txt")}),
        );
        let decide = |mode, rules: Vec<PermissionRule>, operation: &PreparedTool| {
            PermissionPolicy {
                mode,
                rules,
                ..PermissionPolicy::default()
            }
            .decide(operation, &workspace, Uuid::nil())
        };
        let auto = ExecutionMode::Auto;
        assert_eq!(decide(auto, vec![], &workspace_write), Decision::Allow);
        assert_eq!(decide(auto, vec![], &git_write), Decision::Check);
        assert_eq!(
            decide(ExecutionMode::AcceptEdits, vec![], &git_write),
            Decision::Ask
        );
        assert_eq!(decide(auto, vec![], &external_write), Decision::Check);
        assert_eq!(decide(auto, vec![], &external_read), Decision::Allow);
        assert_eq!(
            decide(
                auto,
                vec![tool_rule(Decision::Ask, "Write", None)],
                &workspace_write
            ),
            Decision::Ask
        );
        assert_eq!(
            decide(
                auto,
                vec![tool_rule(
                    Decision::Ask,
                    "Write",
                    Some(workspace_write.input.clone())
                )],
                &workspace_write
            ),
            Decision::Ask
        );
        assert_eq!(
            decide(
                auto,
                vec![tool_rule(Decision::Allow, "Write", None)],
                &git_write
            ),
            Decision::Check
        );
        assert_eq!(
            decide(
                auto,
                vec![tool_rule(Decision::Deny, "Read", None)],
                &external_read
            ),
            Decision::Deny
        );
    }

    /// Claude Code's `useAutoModeDuringPlan`: Plan still refuses side effects, and what it would
    /// ask about goes through Auto's admission instead, but only when Plan was entered from Auto
    /// and the user did not turn that off. Entered from any other mode or an unknown one, it asks.
    #[test]
    fn plan_admits_as_auto_only_after_auto_unless_the_user_turned_it_off() {
        let directory = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        std::fs::write(external.path().join("notes.txt"), "notes").unwrap();
        let (workspace, workspace_write) = prepared_tool(directory.path());
        let external_read = prepare_in(
            &workspace,
            "Read",
            json!({"file_path":external.path().join("notes.txt")}),
        );
        let web_fetch = prepare_in(
            &workspace,
            crate::WEB_FETCH_TOOL_NAME,
            json!({"url":"https://docs.example.com/guide","prompt":"Summarize"}),
        );
        let mutating_shell = prepare_in(
            &workspace,
            BASH_TOOL_NAME,
            json!({"command":"rm -rf build"}),
        );
        let decide = |mode_before_plan,
                      plan_without_auto,
                      rules: Vec<PermissionRule>,
                      operation: &PreparedTool| {
            PermissionPolicy {
                mode: ExecutionMode::Plan,
                rules,
                plan_without_auto,
                mode_before_plan,
                ..PermissionPolicy::default()
            }
            .decide(operation, &workspace, Uuid::nil())
        };
        let broad_allow = || vec![tool_rule(Decision::Allow, "*", None)];
        let after_auto = Some(ExecutionMode::Auto);

        assert_eq!(
            decide(after_auto, false, vec![], &web_fetch),
            Decision::Check
        );
        assert_eq!(
            decide(after_auto, false, vec![], &external_read),
            Decision::Allow
        );
        assert_eq!(
            decide(after_auto, false, broad_allow(), &web_fetch),
            Decision::Check,
            "a broad Allow does not bypass the review, as in Auto"
        );
        for operation in [&workspace_write, &mutating_shell] {
            assert_eq!(
                decide(after_auto, false, broad_allow(), operation),
                Decision::Deny
            );
        }
        assert_eq!(
            decide(
                after_auto,
                false,
                vec![tool_rule(Decision::Ask, "Read", None)],
                &external_read
            ),
            Decision::Ask
        );
        assert_eq!(
            decide(
                after_auto,
                false,
                vec![web_fetch_rule(Decision::Deny, "domain:docs.example.com")],
                &web_fetch
            ),
            Decision::Deny
        );

        for (mode_before_plan, plan_without_auto) in [
            (after_auto, true),
            (Some(ExecutionMode::AcceptEdits), false),
            (None, false),
        ] {
            let decide =
                |rules, operation| decide(mode_before_plan, plan_without_auto, rules, operation);
            assert_eq!(decide(vec![], &web_fetch), Decision::Ask);
            assert_eq!(decide(vec![], &external_read), Decision::Ask);
            assert_eq!(decide(broad_allow(), &web_fetch), Decision::Allow);
            for operation in [&workspace_write, &mutating_shell] {
                assert_eq!(decide(vec![], operation), Decision::Deny);
            }
        }
    }

    /// As in Claude Code's Plan with bypass available, Plan entered from Full Access allows what
    /// it would otherwise ask about, whether Auto in Plan is on or off. Plan still refuses its
    /// side effects, and explicit Ask and Deny rules still win.
    #[test]
    fn plan_after_full_access_allows_what_it_does_not_refuse() {
        let directory = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        std::fs::write(external.path().join("notes.txt"), "notes").unwrap();
        let (workspace, workspace_write) = prepared_tool(directory.path());
        let external_read = prepare_in(
            &workspace,
            "Read",
            json!({"file_path":external.path().join("notes.txt")}),
        );
        let web_fetch = prepare_in(
            &workspace,
            crate::WEB_FETCH_TOOL_NAME,
            json!({"url":"https://docs.example.com/guide","prompt":"Summarize"}),
        );
        let mutating_shell = prepare_in(
            &workspace,
            BASH_TOOL_NAME,
            json!({"command":"rm -rf build"}),
        );
        for plan_without_auto in [false, true] {
            let decide = |rules: Vec<PermissionRule>, operation: &PreparedTool| {
                PermissionPolicy {
                    mode: ExecutionMode::Plan,
                    rules,
                    plan_without_auto,
                    mode_before_plan: Some(ExecutionMode::FullAccess),
                    ..PermissionPolicy::default()
                }
                .decide(operation, &workspace, Uuid::nil())
            };
            assert_eq!(decide(vec![], &web_fetch), Decision::Allow);
            assert_eq!(decide(vec![], &external_read), Decision::Allow);
            for operation in [&workspace_write, &mutating_shell] {
                assert_eq!(decide(vec![], operation), Decision::Deny);
            }
            assert_eq!(
                decide(vec![tool_rule(Decision::Ask, "Read", None)], &external_read),
                Decision::Ask
            );
            assert_eq!(
                decide(
                    vec![web_fetch_rule(Decision::Deny, "domain:docs.example.com")],
                    &web_fetch
                ),
                Decision::Deny
            );
        }
    }

    fn git(directory: &std::path::Path, arguments: &[&str]) {
        let status = std::process::Command::new("git")
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
            .args(arguments)
            .current_dir(directory)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .unwrap();
        assert!(status.success(), "git {arguments:?} failed");
    }

    /// Auto counts the other linked worktrees of the Folder's repository as the project, the way
    /// a session in one Herdr worktree edits a sibling it just created; the main checkout,
    /// unrelated directories, other modes and sensitive paths keep their usual handling.
    #[test]
    fn auto_admits_edits_in_sibling_worktrees_of_the_folder_repository() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let main = root_path.join("main");
        let unrelated = root_path.join("unrelated");
        std::fs::create_dir_all(&main).unwrap();
        std::fs::create_dir_all(&unrelated).unwrap();
        git(&main, &["init", "--quiet"]);
        git(&main, &["commit", "--quiet", "--allow-empty", "-m", "init"]);
        for name in ["current", "sibling"] {
            let path = root_path.join("worktrees").join(name);
            git(
                &main,
                &[
                    "worktree",
                    "add",
                    "--quiet",
                    "-b",
                    name,
                    path.to_str().unwrap(),
                ],
            );
        }
        let sibling = root_path.join("worktrees/sibling");
        let workspace = Workspace {
            folder: Some(root_path.join("worktrees/current")),
            additional_directories: Vec::new(),
        };
        let write = |path: std::path::PathBuf| {
            prepare_in(&workspace, "Write", json!({"file_path":path,"content":"x"}))
        };
        let touch = |path: std::path::PathBuf| {
            prepare_in(
                &workspace,
                "Bash",
                json!({"command":format!("touch {}", path.display())}),
            )
        };
        let decide = |mode, operation: &PreparedTool| {
            PermissionPolicy {
                mode,
                ..PermissionPolicy::default()
            }
            .decide(operation, &workspace, Uuid::nil())
        };
        let auto = ExecutionMode::Auto;

        assert_eq!(
            decide(auto, &write(sibling.join("notes.md"))),
            Decision::Allow
        );
        assert_eq!(
            decide(auto, &touch(sibling.join("new.md"))),
            Decision::Allow
        );
        assert_eq!(
            decide(auto, &write(sibling.join(".gitmodules"))),
            Decision::Check
        );
        assert_eq!(decide(auto, &write(main.join("notes.md"))), Decision::Check);
        assert_eq!(
            decide(auto, &write(unrelated.join("notes.md"))),
            Decision::Check
        );
        assert_eq!(
            decide(auto, &touch(unrelated.join("new.md"))),
            Decision::Check
        );
        assert_eq!(
            decide(ExecutionMode::AcceptEdits, &write(sibling.join("notes.md"))),
            Decision::Ask
        );
    }

    fn shell_rule(decision: Decision, specifier: &str) -> PermissionRule {
        PermissionRule {
            id: Uuid::new_v4(),
            decision,
            tool: BASH_TOOL_NAME.into(),
            specifier: Some(specifier.into()),
            input: None,
            loop_id: None,
            persistent: true,
        }
    }

    #[test]
    fn auto_ignores_user_allow_rules_that_would_bypass_the_classifier() {
        let loop_id = Uuid::new_v4();
        let python = json!({"command":"python3 -c 'print(1)'"});
        let git = json!({"command":"git push"});
        let mut whole_shell = shell_rule(Decision::Allow, "*");
        whole_shell.specifier = None;
        for (rule, input) in [
            (whole_shell, &git),
            (shell_rule(Decision::Allow, "*"), &git),
            (shell_rule(Decision::Allow, "python3:*"), &python),
            (shell_rule(Decision::Allow, "python3 *"), &python),
            (shell_rule(Decision::Allow, "python3 -*"), &python),
        ] {
            let mut policy = PermissionPolicy {
                mode: ExecutionMode::AcceptEdits,
                rules: vec![rule.clone()],
                ..PermissionPolicy::default()
            };
            assert_eq!(
                policy.explicit_decision_with_session(BASH_TOOL_NAME, input, loop_id, None),
                Some(Decision::Allow)
            );
            policy.mode = ExecutionMode::Auto;
            assert_eq!(
                policy.explicit_decision_with_session(BASH_TOOL_NAME, input, loop_id, None),
                None,
                "{:?}",
                rule.specifier
            );
            let managed = PermissionPolicy {
                mode: ExecutionMode::Auto,
                managed_constraints: vec![rule],
                ..PermissionPolicy::default()
            };
            assert_eq!(
                managed.explicit_decision_with_session(BASH_TOOL_NAME, input, loop_id, None),
                Some(Decision::Allow)
            );
        }
        let bounded = PermissionPolicy {
            mode: ExecutionMode::Auto,
            rules: vec![shell_rule(Decision::Allow, "git push")],
            ..PermissionPolicy::default()
        };
        assert_eq!(
            bounded.explicit_decision_with_session(BASH_TOOL_NAME, &git, loop_id, None),
            Some(Decision::Allow)
        );
    }

    #[test]
    fn accept_edits_uses_the_claude_code_persistence_name() {
        assert_eq!(
            serde_json::to_value(ExecutionMode::AcceptEdits).unwrap(),
            json!("acceptEdits")
        );
        assert_eq!(
            serde_json::from_value::<ExecutionMode>(json!("acceptEdits")).unwrap(),
            ExecutionMode::AcceptEdits
        );
        assert_eq!(
            serde_json::from_value::<ExecutionMode>(json!("ask")).unwrap(),
            ExecutionMode::AcceptEdits
        );
        assert_eq!(ExecutionMode::AcceptEdits.label(), "Ask");
        assert_eq!(ExecutionMode::FullAccess.label(), "Full Access");
        assert_eq!(
            serde_json::to_value(ExecutionMode::FullAccess).unwrap(),
            json!("fullAccess")
        );
        assert_eq!(
            serde_json::from_value::<ExecutionMode>(json!("fullAccess")).unwrap(),
            ExecutionMode::FullAccess
        );
        let all = ExecutionMode::CYCLE_ORDER;
        assert_eq!(
            ExecutionMode::Auto.next_in(&all),
            Some(ExecutionMode::AcceptEdits)
        );
        assert_eq!(
            ExecutionMode::AcceptEdits.next_in(&all),
            Some(ExecutionMode::Plan)
        );
        assert_eq!(
            ExecutionMode::Plan.next_in(&all),
            Some(ExecutionMode::FullAccess)
        );
        assert_eq!(
            ExecutionMode::FullAccess.next_in(&all),
            Some(ExecutionMode::Auto)
        );
        let default = [ExecutionMode::Auto, ExecutionMode::Plan];
        assert_eq!(
            ExecutionMode::Auto.next_in(&default),
            Some(ExecutionMode::Plan)
        );
        assert_eq!(
            ExecutionMode::Plan.next_in(&default),
            Some(ExecutionMode::Auto)
        );
        assert_eq!(
            ExecutionMode::AcceptEdits.next_in(&default),
            Some(ExecutionMode::Plan),
            "a mode outside the cycle moves to the next enabled one"
        );
        assert_eq!(
            ExecutionMode::FullAccess.next_in(&default),
            Some(ExecutionMode::Auto)
        );
        assert_eq!(ExecutionMode::Auto.next_in(&[ExecutionMode::Auto]), None);
        assert_eq!(
            ExecutionMode::Plan.next_in(&[ExecutionMode::Auto]),
            Some(ExecutionMode::Auto)
        );
    }

    #[test]
    fn full_access_admits_ordinary_tools_but_preserves_explicit_rules() {
        let directory = tempfile::tempdir().unwrap();
        let (workspace, workspace_write) = prepared_tool(directory.path());
        let shell = prepare_in(
            &workspace,
            BASH_TOOL_NAME,
            json!({"command":"ln -s missing-target link"}),
        );
        let loop_id = Uuid::new_v4();
        let mut policy = PermissionPolicy {
            mode: ExecutionMode::FullAccess,
            ..PermissionPolicy::default()
        };
        assert_eq!(policy.decide(&shell, &workspace, loop_id), Decision::Allow);
        assert_eq!(
            policy.decide(&workspace_write, &workspace, loop_id),
            Decision::Allow
        );

        policy
            .rules
            .push(tool_rule(Decision::Ask, BASH_TOOL_NAME, None));
        assert_eq!(policy.decide(&shell, &workspace, loop_id), Decision::Ask);
        policy
            .rules
            .push(tool_rule(Decision::Deny, BASH_TOOL_NAME, None));
        assert_eq!(policy.decide(&shell, &workspace, loop_id), Decision::Deny);

        policy.rules.clear();
        policy
            .managed_constraints
            .push(tool_rule(Decision::Deny, BASH_TOOL_NAME, None));
        assert_eq!(policy.decide(&shell, &workspace, loop_id), Decision::Deny);
        policy.managed_constraints.clear();
        assert_eq!(
            policy.decide_with_session(
                &shell,
                &workspace,
                loop_id,
                Some(SessionPermissionDecision::Deny)
            ),
            Decision::Deny
        );
        policy.mode = ExecutionMode::AcceptEdits;
        assert_eq!(policy.decide(&shell, &workspace, loop_id), Decision::Ask);
        assert_eq!(
            policy.decide(&workspace_write, &workspace, loop_id),
            Decision::Allow
        );
        policy.mode = ExecutionMode::Auto;
        assert_eq!(policy.decide(&shell, &workspace, loop_id), Decision::Check);
    }

    #[test]
    fn task_stop_needs_no_approval_in_any_mode_but_explicit_rules_still_apply() {
        let directory = tempfile::tempdir().unwrap();
        let (workspace, _) = prepared_tool(directory.path());
        let task_stop = prepare_in(
            &workspace,
            crate::TASK_STOP_TOOL_NAME,
            json!({"task_id":"b7f0c3a1"}),
        );
        assert!(!task_stop.requires_permission);
        assert!(!task_stop.is_read_only);
        let loop_id = Uuid::new_v4();
        for mode in [
            ExecutionMode::AcceptEdits,
            ExecutionMode::Auto,
            ExecutionMode::Plan,
            ExecutionMode::FullAccess,
        ] {
            let decide = |rules: Vec<PermissionRule>| {
                PermissionPolicy {
                    mode,
                    rules,
                    ..PermissionPolicy::default()
                }
                .decide(&task_stop, &workspace, loop_id)
            };
            assert_eq!(decide(vec![]), Decision::Allow, "{mode:?}");
            assert_eq!(
                decide(vec![tool_rule(
                    Decision::Deny,
                    crate::TASK_STOP_TOOL_NAME,
                    None
                )]),
                Decision::Deny,
                "{mode:?}"
            );
            assert_eq!(
                decide(vec![tool_rule(
                    Decision::Ask,
                    crate::TASK_STOP_TOOL_NAME,
                    None
                )]),
                Decision::Ask,
                "{mode:?}"
            );
        }
    }

    fn web_fetch_rule(decision: Decision, content: &str) -> PermissionRule {
        PermissionRule {
            id: Uuid::new_v4(),
            decision,
            tool: crate::WEB_FETCH_TOOL_NAME.into(),
            specifier: Some(content.into()),
            input: None,
            loop_id: None,
            persistent: true,
        }
    }

    #[test]
    fn web_fetch_rules_match_the_host_whatever_the_path_and_prompt() {
        let directory = tempfile::tempdir().unwrap();
        let (workspace, _) = prepared_tool(directory.path());
        let fetch = |url: &str, prompt: &str| {
            prepare_in(
                &workspace,
                crate::WEB_FETCH_TOOL_NAME,
                json!({"url": url, "prompt": prompt}),
            )
        };
        let guide = fetch("https://docs.example.com/guide", "Summarize the guide");
        let api = fetch("http://docs.example.com/api?page=2", "List the endpoints");
        let parent = fetch("https://example.com/guide", "Summarize the guide");
        let subdomain = fetch("https://api.docs.example.com/guide", "Summarize the guide");
        assert_eq!(
            permission_rule_content(&guide.name, &guide.input).as_deref(),
            Some("domain:docs.example.com")
        );
        let loop_id = Uuid::new_v4();
        for decision in [Decision::Allow, Decision::Ask, Decision::Deny] {
            let policy = PermissionPolicy {
                mode: ExecutionMode::AcceptEdits,
                rules: vec![web_fetch_rule(decision, "domain:docs.example.com")],
                ..PermissionPolicy::default()
            };
            for call in [&guide, &api] {
                assert_eq!(policy.decide(call, &workspace, loop_id), decision);
            }
            for call in [&parent, &subdomain] {
                assert_eq!(policy.decide(call, &workspace, loop_id), Decision::Ask);
            }
        }
        assert_eq!(
            PermissionPolicy::default().decide(&guide, &workspace, loop_id),
            Decision::Check
        );
        assert_eq!(
            permission_rule_candidates(&api.name, &api.input),
            vec![PermissionRuleCandidate {
                label: "WebFetch(domain:docs.example.com)".into(),
                specifier: "domain:docs.example.com".into(),
            }]
        );
    }

    #[test]
    fn a_denied_web_fetch_domain_is_named_in_claude_code_words() {
        let directory = tempfile::tempdir().unwrap();
        let (workspace, _) = prepared_tool(directory.path());
        let fetch = prepare_in(
            &workspace,
            crate::WEB_FETCH_TOOL_NAME,
            json!({"url": "https://docs.example.com/guide", "prompt": "Summarize"}),
        );
        let loop_id = Uuid::new_v4();
        let message = |rules: Vec<PermissionRule>, session| {
            PermissionPolicy {
                rules,
                ..PermissionPolicy::default()
            }
            .content_denial_message(&fetch.name, &fetch.input, loop_id, session)
        };
        let expected = Some("WebFetch denied access to domain:docs.example.com.".to_owned());
        assert_eq!(
            message(
                vec![web_fetch_rule(Decision::Deny, "domain:docs.example.com")],
                None
            ),
            expected
        );
        assert_eq!(
            message(vec![], Some(SessionPermissionDecision::Deny)),
            expected
        );
        assert_eq!(
            message(
                vec![tool_rule(Decision::Deny, crate::WEB_FETCH_TOOL_NAME, None)],
                None
            ),
            None
        );
        assert_eq!(
            message(
                vec![web_fetch_rule(Decision::Deny, "domain:example.com")],
                None
            ),
            None
        );
    }

    #[test]
    fn accept_edits_allows_workspace_file_changes_unless_a_rule_is_stricter() {
        let directory = tempfile::tempdir().unwrap();
        let (workspace, operation) = prepared_tool(directory.path());
        let mut policy = PermissionPolicy {
            mode: ExecutionMode::AcceptEdits,
            ..PermissionPolicy::default()
        };
        assert_eq!(
            policy.decide(&operation, &workspace, Uuid::nil()),
            Decision::Allow
        );
        policy.rules.push(PermissionRule {
            id: Uuid::new_v4(),
            decision: Decision::Ask,
            tool: operation.name.clone(),
            specifier: None,
            input: None,
            loop_id: None,
            persistent: false,
        });
        assert_eq!(
            policy.decide(&operation, &workspace, Uuid::nil()),
            Decision::Ask
        );
    }

    #[test]
    fn shell_candidates_progress_from_exact_command_to_broader_prefixes() {
        assert_eq!(
            permission_rule_candidates(
                BASH_TOOL_NAME,
                &json!({"command":"cargo test --locked -p tickloop-code"})
            ),
            vec![
                PermissionRuleCandidate {
                    label: "Bash(cargo test --locked -p tickloop-code)".into(),
                    specifier: "cargo test --locked -p tickloop-code".into(),
                },
                PermissionRuleCandidate {
                    label: "Bash(cargo test --locked -p *)".into(),
                    specifier: "cargo test --locked -p *".into(),
                },
                PermissionRuleCandidate {
                    label: "Bash(cargo test --locked *)".into(),
                    specifier: "cargo test --locked *".into(),
                },
                PermissionRuleCandidate {
                    label: "Bash(cargo test *)".into(),
                    specifier: "cargo test *".into(),
                },
                PermissionRuleCandidate {
                    label: "Bash(cargo *)".into(),
                    specifier: "cargo *".into(),
                },
            ]
        );
    }

    #[test]
    fn exact_and_prefix_shell_rules_have_distinct_matching_scopes() {
        let loop_id = Uuid::new_v4();
        let exact = PermissionPolicy {
            rules: vec![shell_rule(Decision::Allow, "cargo test")],
            ..PermissionPolicy::default()
        };
        assert_eq!(
            exact.explicit_decision_with_session(
                BASH_TOOL_NAME,
                &json!({"command":"cargo test"}),
                loop_id,
                None
            ),
            Some(Decision::Allow)
        );
        assert_eq!(
            exact.explicit_decision_with_session(
                BASH_TOOL_NAME,
                &json!({"command":"cargo test --locked"}),
                loop_id,
                None
            ),
            None
        );
        let prefix = PermissionPolicy {
            rules: vec![shell_rule(Decision::Allow, "cargo test *")],
            ..PermissionPolicy::default()
        };
        assert_eq!(
            prefix.explicit_decision_with_session(
                BASH_TOOL_NAME,
                &json!({"command":"cargo test --locked"}),
                loop_id,
                None
            ),
            Some(Decision::Allow)
        );
        assert_eq!(
            prefix.explicit_decision_with_session(
                BASH_TOOL_NAME,
                &json!({"command":"cargo build"}),
                loop_id,
                None
            ),
            None
        );
    }

    #[test]
    fn compound_shell_commands_require_a_rule_for_every_subcommand() {
        let loop_id = Uuid::new_v4();
        let input = json!({"command":"cargo test && npm test"});
        let partial = PermissionPolicy {
            rules: vec![shell_rule(Decision::Allow, "cargo test *")],
            ..PermissionPolicy::default()
        };
        assert_eq!(
            partial.explicit_decision_with_session(BASH_TOOL_NAME, &input, loop_id, None),
            None
        );
        let complete = PermissionPolicy {
            rules: vec![
                shell_rule(Decision::Allow, "cargo test *"),
                shell_rule(Decision::Allow, "npm test *"),
            ],
            ..PermissionPolicy::default()
        };
        assert_eq!(
            complete.explicit_decision_with_session(BASH_TOOL_NAME, &input, loop_id, None),
            Some(Decision::Allow)
        );
        let denied = PermissionPolicy {
            rules: vec![shell_rule(Decision::Deny, "npm test *")],
            ..PermissionPolicy::default()
        };
        assert_eq!(
            denied.explicit_decision_with_session(BASH_TOOL_NAME, &input, loop_id, None),
            Some(Decision::Deny)
        );
        let exact = PermissionPolicy {
            rules: vec![shell_rule(Decision::Allow, "cargo test && npm test")],
            ..PermissionPolicy::default()
        };
        assert_eq!(
            exact.explicit_decision_with_session(BASH_TOOL_NAME, &input, loop_id, None),
            Some(Decision::Allow)
        );
        assert_eq!(
            permission_rule_candidates(BASH_TOOL_NAME, &input),
            vec![PermissionRuleCandidate {
                label: "Bash(cargo test && npm test)".into(),
                specifier: "cargo test && npm test".into(),
            }]
        );
    }

    #[test]
    fn durable_deny_and_ask_stay_more_restrictive_than_a_session_allow() {
        let directory = tempfile::tempdir().unwrap();
        let (workspace, operation) = prepared_tool(directory.path());
        for durable in [Decision::Deny, Decision::Ask] {
            let policy = PermissionPolicy {
                mode: ExecutionMode::AcceptEdits,
                rules: vec![PermissionRule {
                    id: Uuid::new_v4(),
                    decision: durable,
                    tool: operation.name.clone(),
                    specifier: None,
                    input: Some(operation.input.clone()),
                    loop_id: Some(Uuid::nil()),
                    persistent: true,
                }],
                ..PermissionPolicy::default()
            };
            assert_eq!(
                policy.decide_with_session(
                    &operation,
                    &workspace,
                    Uuid::nil(),
                    Some(SessionPermissionDecision::Allow),
                ),
                durable
            );
        }
    }

    #[test]
    fn session_deny_wins_over_a_durable_allow() {
        let directory = tempfile::tempdir().unwrap();
        let (workspace, operation) = prepared_tool(directory.path());
        let policy = PermissionPolicy {
            rules: vec![PermissionRule {
                id: Uuid::new_v4(),
                decision: Decision::Allow,
                tool: operation.name.clone(),
                specifier: None,
                input: Some(operation.input.clone()),
                loop_id: Some(Uuid::nil()),
                persistent: true,
            }],
            ..PermissionPolicy::default()
        };
        assert_eq!(
            policy.decide_with_session(
                &operation,
                &workspace,
                Uuid::nil(),
                Some(SessionPermissionDecision::Deny),
            ),
            Decision::Deny
        );
    }

    #[test]
    fn the_current_plan_file_is_readable_and_writable_in_every_mode() {
        let directory = tempfile::tempdir().unwrap();
        let plans = tempfile::tempdir().unwrap();
        let plans_path = plans.path().canonicalize().unwrap();
        let plan_file = plans_path.join("calm-baking-aurora-loop-plan.md");
        let (workspace, _) = prepared_tool(directory.path());
        let write = |path: &Path| {
            prepare_in(
                &workspace,
                "Write",
                json!({"file_path": path, "content": "Plan."}),
            )
        };
        let plan_write = write(&plan_file);
        let other_write = write(&plans_path.join("other.md"));
        std::fs::write(&plan_file, "Plan.").unwrap();
        let plan_read = prepare_in(&workspace, "Read", json!({"file_path": plan_file}));
        let decide = |policy: &PermissionPolicy, operation: &PreparedTool| {
            policy.decide_with_plan_file(operation, &workspace, Uuid::nil(), None, Some(&plan_file))
        };

        let plan = PermissionPolicy {
            mode: ExecutionMode::Plan,
            ..PermissionPolicy::default()
        };
        assert_eq!(decide(&plan, &plan_write), Decision::Allow);
        assert_eq!(decide(&plan, &plan_read), Decision::Allow);
        assert_eq!(decide(&plan, &other_write), Decision::Deny);
        let denied = PermissionPolicy {
            mode: ExecutionMode::Plan,
            rules: vec![tool_rule(Decision::Deny, "Write", None)],
            ..PermissionPolicy::default()
        };
        assert_eq!(decide(&denied, &plan_write), Decision::Deny);

        for mode in [
            ExecutionMode::AcceptEdits,
            ExecutionMode::Auto,
            ExecutionMode::FullAccess,
        ] {
            let policy = PermissionPolicy {
                mode,
                ..PermissionPolicy::default()
            };
            assert_eq!(decide(&policy, &plan_read), Decision::Allow, "{mode:?}");
            assert_eq!(decide(&policy, &plan_write), Decision::Allow, "{mode:?}");
            for decision in [Decision::Deny, Decision::Ask] {
                let ruled = PermissionPolicy {
                    mode,
                    rules: vec![tool_rule(decision, "Write", None)],
                    ..PermissionPolicy::default()
                };
                assert_eq!(decide(&ruled, &plan_write), decision, "{mode:?}");
            }
        }
    }

    #[test]
    fn session_allow_replaces_the_default_review_without_changing_plain_decisions() {
        let directory = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let (workspace, _) = prepared_tool(directory.path());
        let operation = prepare_in(
            &workspace,
            "Write",
            json!({"file_path":external.path().join("report.txt"),"content":"approved"}),
        );
        let policy = PermissionPolicy::default();
        assert_eq!(
            policy.decide(&operation, &workspace, Uuid::nil()),
            Decision::Check
        );
        assert_eq!(
            policy.decide_with_session(
                &operation,
                &workspace,
                Uuid::nil(),
                Some(SessionPermissionDecision::Allow),
            ),
            Decision::Allow
        );
        assert_eq!(
            policy.decide_with_session(&operation, &workspace, Uuid::nil(), None),
            policy.decide(&operation, &workspace, Uuid::nil())
        );
    }
}

#[cfg(test)]
mod tick_policy_rename_tests {
    use super::*;

    #[test]
    fn saved_exact_subscription_rules_match_through_the_policy_decision() {
        let loop_id = Uuid::new_v4();
        for (tool, action, input) in [
            (
                "SubscribeTick",
                "subscribe",
                serde_json::json!({"tick_instance_id":"shell:one"}),
            ),
            (
                "UnsubscribeTick",
                "unsubscribe",
                serde_json::json!({"subscription":Uuid::new_v4()}),
            ),
        ] {
            for decision in [Decision::Allow, Decision::Deny, Decision::Ask] {
                let policy = PermissionPolicy {
                    rules: vec![PermissionRule {
                        id: Uuid::new_v4(),
                        decision,
                        tool: tool.into(),
                        specifier: None,
                        input: Some(input.clone()),
                        loop_id: Some(loop_id),
                        persistent: true,
                    }],
                    ..PermissionPolicy::default()
                };
                let mut current = input.clone();
                current["action"] = action.into();
                assert_eq!(
                    policy.explicit_decision_with_session("TickManage", &current, loop_id, None),
                    Some(decision)
                );
                current["action"] = "close".into();
                assert_eq!(
                    policy.explicit_decision_with_session("TickManage", &current, loop_id, None),
                    None
                );
            }
        }
    }

    #[test]
    fn saved_tick_permissions_keep_their_action_boundaries() {
        let loop_id = Uuid::new_v4();
        for decision in [Decision::Allow, Decision::Deny, Decision::Ask] {
            let rule = PermissionRule {
                id: Uuid::new_v4(),
                decision,
                tool: "SubscribeTick".into(),
                specifier: None,
                input: None,
                loop_id: None,
                persistent: true,
            };
            assert!(rule_matches_call(
                &rule,
                "TickManage",
                &serde_json::json!({"action":"subscribe","tick_instance_id":"hosted:one"}),
                loop_id
            ));
            assert!(!rule_matches_call(
                &rule,
                "TickManage",
                &serde_json::json!({"action":"close","tick_instance_id":"hosted:one"}),
                loop_id
            ));
            let rule = PermissionRule {
                tool: "ModifyTick".into(),
                ..rule
            };
            assert!(!rule_matches_call(
                &rule,
                "TickManage",
                &serde_json::json!({"action":"subscribe","tick_instance_id":"hosted:one"}),
                loop_id
            ));
            assert!(rule_matches_call(
                &rule,
                "TickManage",
                &serde_json::json!({"action":"close","tick_instance_id":"hosted:one"}),
                loop_id
            ));
        }
    }
}
