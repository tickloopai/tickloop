//! Claude Code's hook matchers: empty or `*` matches everything, a plain list of names
//! separated by `|` (and `,` for tool events) matches those names exactly, and anything else is a
//! regular expression searched in the name. A tool's former names match it too, so `Agent` and
//! `Task` select the Subagent tool.
//!
//! A hook's `if` then narrows a tool event to calls a Permission Rule such as `Bash(git push*)`
//! may match, judged as Tickloop's own Deny and Ask rules judge calls and failing closed.

use serde_json::Value;
use tickloop_harness_protocol::{Decision, PermissionRule, SUBAGENT_TOOL_NAME, hooks::HookEvent};
use tickloop_harness_tools::permissions::{
    is_supported_rule_text, parse_rule_text, restriction_may_match_call,
};

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

/// Whether `matcher` selects `query` for `event`; an invalid regular expression selects nothing.
pub(crate) fn matches(event: HookEvent, matcher: Option<&str>, query: &str) -> bool {
    let Some(matcher) = matcher.filter(|matcher| !matcher.is_empty() && *matcher != "*") else {
        return true;
    };
    let names = std::iter::once(query).chain(aliases(query).iter().copied());
    let allows_commas = event.is_tool_event();
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

/// Whether the call of `tool` with `input` may match a hook's `if`, failing closed so a guard
/// never misses a call: a compound command matches when any of its commands does, one that
/// cannot be told apart matches, and an `if` Tickloop cannot match, such as a file path pattern,
/// matches every call.
pub(crate) fn condition_matches(condition: &str, tool: &str, input: &Value) -> bool {
    if !is_supported_rule_text(condition) {
        return true;
    }
    let Some((rule_tool, specifier)) = parse_rule_text(condition) else {
        return true;
    };
    let rule = PermissionRule {
        id: uuid::Uuid::nil(),
        decision: Decision::Deny,
        tool: if aliases(tool).contains(&rule_tool.as_str()) {
            tool.to_owned()
        } else {
            rule_tool
        },
        specifier,
        input: None,
        loop_id: None,
        persistent: false,
    };
    restriction_may_match_call(&rule, tool, input, uuid::Uuid::nil())
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
        assert!(!condition_matches(
            "Bash(git push*)",
            "Bash",
            &json!({"command": "ls > out"})
        ));
    }

    #[test]
    fn an_if_that_cannot_be_judged_lets_the_hook_run() {
        let push = json!({"command": "git push origin main"});
        for uncertain in [
            "cd repo && git push > push.log",
            "echo $(git push)",
            "echo 'unclosed",
        ] {
            assert!(
                condition_matches("Bash(git push*)", "Bash", &json!({"command": uncertain})),
                "{uncertain}"
            );
        }
        assert!(condition_matches("Bash(git push*", "Bash", &push));
        assert!(condition_matches("(git push)", "Bash", &push));
        assert!(condition_matches(
            "Edit(src/**)",
            "Edit",
            &json!({"file_path": "/elsewhere/a.rs"})
        ));
        assert!(condition_matches(
            "mcp__github",
            "mcp__github__create_issue",
            &json!({})
        ));
        assert!(!condition_matches(
            "mcp__github",
            "mcp__slack__post",
            &json!({})
        ));
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
