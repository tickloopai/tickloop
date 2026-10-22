#![cfg_attr(
    not(test),
    expect(dead_code, reason = "tickets 07 and 09 wire the views")
)]

use unicode_width::UnicodeWidthStr;

use super::{
    display::DisplayProfile,
    list_row::{self, RowFlash},
    loop_view_text::{clipped, wrapped},
    presentation::compact_count,
    style::{Role, RoleLine},
};

const CONDITION_ROWS: usize = 3;
const TRIGGERS_PAUSED: &str = "Triggers paused · send a message to resume";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum GoalState {
    Active,
    Paused,
    Blocked,
    BudgetLimited,
    Complete,
}

impl GoalState {
    fn label(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Blocked => "blocked",
            Self::BudgetLimited => "budget_limited",
            Self::Complete => "complete",
        }
    }

    fn role(self) -> Role {
        match self {
            Self::Active => Role::Accent,
            Self::Paused => Role::MutedText,
            Self::Blocked | Self::BudgetLimited => Role::Warning,
            Self::Complete => Role::Success,
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct GoalView<'a> {
    pub(super) condition: &'a str,
    pub(super) state: GoalState,
    pub(super) tokens_used: u64,
    pub(super) token_budget: Option<u64>,
    pub(super) elapsed_ms: u64,
    pub(super) check_reason: Option<&'a str>,
    /// A live continuation's display-clock timestamp; historical renders leave it empty.
    pub(super) continued_at_ms: Option<u64>,
}

impl GoalView<'_> {
    fn usage(&self) -> String {
        match self.token_budget {
            Some(budget) => format!("{} / {}", tokens(self.tokens_used), tokens(budget)),
            None => list_row::format_elapsed(self.elapsed_ms),
        }
    }

    pub(super) fn monitor_lines(
        &self,
        width: usize,
        display: DisplayProfile,
        now_ms: u64,
    ) -> Vec<RoleLine> {
        if width == 0 {
            return Vec::new();
        }
        let mut lines = vec![clipped("Goal", width, Role::Title)];
        let mut condition = wrapped(self.condition, width, Role::Text);
        if condition.len() > CONDITION_ROWS {
            condition.truncate(CONDITION_ROWS);
            let last = condition.last_mut().expect("condition has three rows");
            let text = ellipsis(&last.text(), width);
            *last = RoleLine::role(text, Role::Text);
        }
        lines.extend(condition);
        lines.push(clipped(
            &format!("{} · {}", self.state.label(), self.usage()),
            width,
            self.state.role(),
        ));
        if let Some(reason) = self.check_reason {
            lines.push(clipped(&format!("Check: {reason}"), width, Role::MutedText));
        }
        if let Some(fill) = list_row::flash_fill(
            self.continued_at_ms.map(|since| (RowFlash::TakeIn, since)),
            display,
            now_ms,
        ) {
            lines = lines.into_iter().map(|line| line.filled(fill)).collect();
        }
        lines
    }

    pub(super) fn detail_lines(&self, width: usize, top: usize, height: usize) -> Vec<RoleLine> {
        if width == 0 {
            return Vec::new();
        }
        let mut lines = vec![clipped("Goal", width, Role::Title)];
        lines.extend(wrapped(self.condition, width, Role::Text));
        lines.extend(wrapped(
            &format!("Status: {}", self.state.label()),
            width,
            self.state.role(),
        ));
        lines.extend(wrapped(
            &format!("Usage: {}", self.usage()),
            width,
            Role::MutedText,
        ));
        if let Some(reason) = self.check_reason {
            lines.extend(wrapped(&format!("Check: {reason}"), width, Role::MutedText));
        }
        lines.into_iter().skip(top).take(height).collect()
    }

    fn status(&self) -> String {
        match self.state {
            GoalState::Active => format!("Pursuing goal ({})", self.usage()),
            GoalState::Paused => "Goal paused (/goal resume)".into(),
            GoalState::Blocked => "Goal stalled (/goal resume)".into(),
            GoalState::BudgetLimited => {
                let usage = self
                    .token_budget
                    .map_or_else(|| tokens(self.tokens_used), |_| self.usage());
                format!("Goal unmet ({usage} tokens)")
            }
            GoalState::Complete => {
                let usage = self.token_budget.map_or_else(
                    || self.usage(),
                    |_| format!("{} tokens", tokens(self.tokens_used)),
                );
                format!("Goal achieved ({usage})")
            }
        }
    }
}

pub(super) fn status_line(
    goal: Option<&GoalView<'_>>,
    triggers_paused: bool,
    width: usize,
) -> Option<RoleLine> {
    if triggers_paused {
        return Some(clipped(TRIGGERS_PAUSED, width, Role::MutedText));
    }
    goal.map(|goal| clipped(&goal.status(), width, goal.state.role()))
}

fn tokens(count: u64) -> String {
    compact_count(count).to_ascii_uppercase()
}

fn ellipsis(text: &str, width: usize) -> String {
    let mut line = RoleLine::plain(text);
    line.truncate(width.saturating_sub("…".width()));
    format!("{}…", line.text())
}

#[cfg(test)]
mod tests;
