//! Claude Code's hook matchers: empty or `*` matches everything, a plain list of names
//! separated by `|` (and `,` for tool events) matches those names exactly, and anything else is a
//! regular expression searched in the name. A tool's former names match it too, so `Agent` and
//! `Task` select the Subagent tool.
//!
//! A hook's `if` then narrows a tool event to calls a Permission Rule such as `Bash(git push*)`
//! matches, judged as Tickloop's own Permission Rules judge calls.

use serde_json::Value;
use tickloop_harness_protocol::{Decision, PermissionRule, SUBAGENT_TOOL_NAME, hooks::HookEvent};
use tickloop_harness_tools::permissions::rule_matches_call;

/// The names Claude Code users write for a Tickloop tool, besides its own.
fn aliases(name: &str) -> &'static [&'static str] {
    if name == SUBAGENT_TOOL_NAME {
        &["Agent", "Task"]
    } else {
        &[]
    }
}

fn is_name_list(matcher: &str, allows_commas: bool) -> bool {
    !matcher.is_empty()
        && matcher.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || character == '_'
                || character == '|'
                || (allows_commas && matches!(character, ',' | ' ' | '-'))
        })
}

pub(crate) fn is_tool_event(event: HookEvent) -> bool {
    matches!(
        event,
        HookEvent::PreToolUse | HookEvent::PostToolUse | HookEvent::PostToolUseFailure
    )
}

/// Whether `matcher` selects `query` for `event`; an invalid regular expression selects nothing.
pub(crate) fn matches(event: HookEvent, matcher: Option<&str>, query: &str) -> bool {
    let Some(matcher) = matcher.filter(|matcher| !matcher.is_empty() && *matcher != "*") else {
        return true;
    };
    let names = std::iter::once(query).chain(aliases(query).iter().copied());
    let allows_commas = is_tool_event(event);
    if is_name_list(matcher, allows_commas) {
        let listed = matcher
            .split(|character| character == '|' || (allows_commas && character == ','))
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .collect::<Vec<_>>();
        return names.into_iter().any(|name| listed.contains(&name));
    }
    match regex::Regex::new(matcher) {
        Ok(pattern) => names.into_iter().any(|name| pattern.is_match(name)),
        Err(error) => {
            tracing::warn!("invalid hook matcher {matcher:?}: {error}");
            false
        }
    }
}

/// The tool and content of a Permission Rule written as `Tool` or `Tool(content)`, with `\(` and
/// `\)` standing for parentheses in the content; `Tool()` and `Tool(*)` name the whole tool.
fn parse_rule(rule: &str) -> Option<(&str, Option<String>)> {
    let rule = rule.trim();
    let Some(open) = rule.find('(') else {
        return (!rule.is_empty()).then_some((rule, None));
    };
    let content = rule[open + 1..].strip_suffix(')')?;
    let tool = rule[..open].trim();
    if tool.is_empty() {
        return None;
    }
    let content = content.replace("\\(", "(").replace("\\)", ")");
    let content = content.trim();
    Some((
        tool,
        (!content.is_empty() && content != "*").then(|| content.to_owned()),
    ))
}

/// Whether the call of `tool` with `input` matches a hook's `if`; an `if` that is not a
/// Permission Rule matches nothing.
pub(crate) fn condition_matches(condition: &str, tool: &str, input: &Value) -> bool {
    let Some((rule_tool, specifier)) = parse_rule(condition) else {
        tracing::warn!("hook if condition {condition:?} is not a Permission Rule");
        return false;
    };
    let rule_tool = if aliases(tool).contains(&rule_tool) {
        tool
    } else {
        rule_tool
    };
    let rule = PermissionRule {
        id: uuid::Uuid::nil(),
        decision: Decision::Allow,
        tool: rule_tool.to_owned(),
        specifier,
        input: None,
        loop_id: None,
        persistent: false,
    };
    rule_matches_call(&rule, tool, input, uuid::Uuid::nil())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn an_if_condition_matches_as_a_permission_rule_does() {
        let push = json!({"command": "git push origin main"});
        assert!(condition_matches("Bash(git push*)", "Bash", &push));
        assert!(condition_matches("Bash(git push:*)", "Bash", &push));
        assert!(condition_matches(
            "Bash(git push*)",
            "Bash",
            &json!({"command": "cargo test && git push"})
        ));
        assert!(!condition_matches("Bash(git commit*)", "Bash", &push));
        assert!(condition_matches("Bash", "Bash", &push));
        assert!(condition_matches("Bash(*)", "Bash", &push));
        assert!(!condition_matches("Bash(git push*)", "Write", &push));
        assert!(condition_matches(
            "WebFetch(domain:example.com)",
            "WebFetch",
            &json!({"url": "https://example.com/a", "prompt": "Read"})
        ));
        assert!(condition_matches(
            "Agent",
            "Subagent",
            &json!({"task": "Look"})
        ));
        assert!(!condition_matches("Bash(git push*", "Bash", &push));
        assert!(!condition_matches("(git push)", "Bash", &push));
    }

    #[test]
    fn empty_and_star_match_every_tool() {
        assert!(matches(HookEvent::PreToolUse, None, "Bash"));
        assert!(matches(HookEvent::PreToolUse, Some(""), "Bash"));
        assert!(matches(HookEvent::PreToolUse, Some("*"), "Bash"));
    }

    #[test]
    fn a_name_list_matches_names_exactly() {
        assert!(matches(HookEvent::PostToolUse, Some("Edit|Write"), "Write"));
        assert!(!matches(
            HookEvent::PostToolUse,
            Some("Edit|Write"),
            "WriteFile"
        ));
        assert!(matches(HookEvent::PreToolUse, Some("Edit, Write"), "Write"));
        assert!(!matches(HookEvent::PreToolUse, Some("Bash"), "BashOutput"));
    }

    #[test]
    fn a_regular_expression_is_searched_in_the_name() {
        assert!(matches(
            HookEvent::PreToolUse,
            Some("mcp__github__.*"),
            "mcp__github__issue"
        ));
        assert!(matches(HookEvent::PreToolUse, Some("^Bas"), "Bash"));
        assert!(!matches(
            HookEvent::PreToolUse,
            Some("^Read$"),
            "ReadMcpResourceTool"
        ));
        assert!(!matches(HookEvent::PreToolUse, Some("(unclosed"), "Bash"));
    }

    #[test]
    fn agent_and_task_select_the_subagent_tool() {
        assert!(matches(HookEvent::PreToolUse, Some("Agent"), "Subagent"));
        assert!(matches(
            HookEvent::PreToolUse,
            Some("Task|Bash"),
            "Subagent"
        ));
        assert!(matches(HookEvent::PreToolUse, Some("^Ag.*"), "Subagent"));
        assert!(!matches(HookEvent::PreToolUse, Some("Agent"), "Bash"));
    }
}
