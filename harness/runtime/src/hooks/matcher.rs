//! Claude Code's hook matchers: empty or `*` matches everything, a plain list of names
//! separated by `|` (and `,` for tool events) matches those names exactly, and anything else is a
//! regular expression searched in the name. A tool's former names match it too, so `Agent` and
//! `Task` select the Subagent tool.

use tickloop_harness_protocol::{SUBAGENT_TOOL_NAME, hooks::HookEvent};

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

fn is_tool_event(event: HookEvent) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;

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
