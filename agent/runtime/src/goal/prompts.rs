//! Goal steering texts adapted from Codex's `ext/goal/templates/goals/`: the continuation sent
//! when the host continues an active goal, the wrap-up when its token budget runs out, and the
//! notice when the user edits its objective. Tickloop's UpdateGoal reports only `complete` with
//! evidence, which the host's completion check then verifies, or `blocked` with a reason; the
//! user alone pauses, resumes, edits and clears a goal.

use std::time::Duration;

const CONTINUATION_TEMPLATE: &str = include_str!("templates/continuation.md");
const BUDGET_LIMIT_TEMPLATE: &str = include_str!("templates/budget_limit.md");
const OBJECTIVE_UPDATED_TEMPLATE: &str = include_str!("templates/objective_updated.md");

const PLACEHOLDER_OPEN: &str = "{{ ";
const PLACEHOLDER_CLOSE: &str = " }}";

/// What the steering texts say about a goal.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GoalPromptContext<'a> {
    pub objective: &'a str,
    /// Why the last completion check did not accept the goal's completion report.
    pub last_check: Option<&'a str>,
    pub tokens_used: u64,
    pub token_budget: Option<u64>,
    pub time_used: Duration,
}

pub(crate) fn continuation_prompt(goal: &GoalPromptContext<'_>) -> String {
    let objective = escape_xml_text(goal.objective);
    let last_check = goal.last_check.map_or_else(String::new, |reason| {
        format!(
            "The host's completion check did not accept the last completion report:\n\n<last_check>\n{}\n</last_check>\n\nAddress this before reporting completion again.\n\n",
            escape_xml_text(reason)
        )
    });
    render(
        CONTINUATION_TEMPLATE,
        &[
            ("objective", &objective),
            ("last_check", &last_check),
            ("usage", &usage_lines(goal)),
        ],
    )
}

pub(crate) fn budget_limit_prompt(goal: &GoalPromptContext<'_>) -> String {
    render(
        BUDGET_LIMIT_TEMPLATE,
        &[
            ("objective", &escape_xml_text(goal.objective)),
            ("usage", &usage_lines(goal)),
        ],
    )
}

pub(crate) fn objective_updated_prompt(goal: &GoalPromptContext<'_>) -> String {
    render(
        OBJECTIVE_UPDATED_TEMPLATE,
        &[
            ("objective", &escape_xml_text(goal.objective)),
            ("usage", &usage_lines(goal)),
        ],
    )
}

fn usage_lines(goal: &GoalPromptContext<'_>) -> String {
    let mut lines = format!(
        "- Time spent pursuing goal: {} seconds\n- Tokens used: {}\n",
        goal.time_used.as_secs(),
        goal.tokens_used
    );
    match goal.token_budget {
        Some(budget) => lines.push_str(&format!(
            "- Token budget: {budget}\n- Tokens remaining: {}",
            budget.saturating_sub(goal.tokens_used)
        )),
        None => lines.push_str("- Token budget: none"),
    }
    lines
}

/// Fills each `{{ name }}` of an embedded template in one pass, so placeholder-like text inside a
/// value stays literal.
fn render(template: &str, values: &[(&str, &str)]) -> String {
    let mut rendered = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find(PLACEHOLDER_OPEN) {
        rendered.push_str(&rest[..start]);
        let placeholder = &rest[start + PLACEHOLDER_OPEN.len()..];
        let end = placeholder
            .find(PLACEHOLDER_CLOSE)
            .expect("embedded goal template closes its placeholder");
        let name = &placeholder[..end];
        let (_, value) = values
            .iter()
            .find(|(key, _)| *key == name)
            .unwrap_or_else(|| panic!("embedded goal template has no value for {name}"));
        rendered.push_str(value);
        rest = &placeholder[end + PLACEHOLDER_CLOSE.len()..];
    }
    rendered.push_str(rest);
    rendered.truncate(rendered.trim_end().len());
    rendered
}

fn escape_xml_text(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
#[path = "prompts_tests.rs"]
mod tests;
