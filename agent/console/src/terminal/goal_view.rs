use super::{
    display::{DisplayProfile, Glyph},
    list_row::{self, RowFlash},
    loop_view_text::{clipped, ellipsize, wrapped},
    presentation::compact_count,
    style::{Role, RoleLine},
};

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

    fn title(self) -> &'static str {
        match self {
            Self::Active => "Pursuing goal",
            Self::Paused => "Goal paused",
            Self::Blocked => "Goal stalled",
            Self::BudgetLimited => "Goal unmet",
            Self::Complete => "Goal achieved",
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

impl<'a> GoalView<'a> {
    pub(super) fn from_snapshot(goal: &'a tickloop_agent_protocol::GoalSnapshot) -> Self {
        use tickloop_agent_protocol::{GoalStatus, GoalVerdict};
        Self {
            condition: &goal.objective,
            state: match goal.status {
                GoalStatus::Active => GoalState::Active,
                GoalStatus::Paused { .. } => GoalState::Paused,
                GoalStatus::Blocked { .. } => GoalState::Blocked,
                GoalStatus::BudgetLimited => GoalState::BudgetLimited,
                GoalStatus::Complete => GoalState::Complete,
            },
            tokens_used: goal.tokens_used,
            token_budget: goal.token_budget,
            elapsed_ms: goal.time_used_ms,
            check_reason: goal.last_check.as_ref().map(|check| match &check.verdict {
                GoalVerdict::Met => "Goal met",
                GoalVerdict::NotMet { reason } | GoalVerdict::Impossible { reason } => {
                    reason.as_str()
                }
            }),
            continued_at_ms: None,
        }
    }

    fn usage(&self) -> String {
        match self.token_budget {
            Some(budget) => format!("{} / {}", tokens(self.tokens_used), tokens(budget)),
            None => format_elapsed(self.elapsed_ms),
        }
    }

    pub(super) fn monitor_lines(
        &self,
        width: usize,
        selected: bool,
        display: DisplayProfile,
        ascii: bool,
        now_ms: u64,
    ) -> Vec<RoleLine> {
        if width == 0 {
            return Vec::new();
        }
        let glyph = if ascii {
            "o"
        } else {
            display.glyph(Glyph::Goal)
        };
        let indent = list_row::compact_name_column(glyph).min(width);
        let time = ellipsize(&format_elapsed(self.elapsed_ms), width);
        let title = ellipsize(
            self.state.title(),
            width.saturating_sub(unicode_width::UnicodeWidthStr::width(time.as_str()) + 1),
        );
        let mut header = RoleLine::role(title, self.state.role());
        header.preserve_selection_colours = true;
        header.push(
            " ".repeat(width.saturating_sub(
                unicode_width::UnicodeWidthStr::width(header.text().as_str())
                    + unicode_width::UnicodeWidthStr::width(time.as_str()),
            )),
            None,
        );
        header.push(time, Some(Role::MutedText));
        let mut condition = RoleLine::role(glyph, self.state.role());
        condition.preserve_selection_colours = true;
        if width > unicode_width::UnicodeWidthStr::width(glyph) {
            condition.push(" ", Some(self.state.role()));
        }
        condition.push(
            ellipsize(&super::safe_line(self.condition), width - indent),
            Some(if selected {
                Role::Text
            } else {
                Role::MutedText
            }),
        );
        let fill = list_row::flash_fill(
            self.continued_at_ms.map(|since| (RowFlash::TakeIn, since)),
            display,
            now_ms,
        );
        [header, condition]
            .into_iter()
            .map(|line| {
                if selected {
                    line.filled(Role::Selection)
                } else if let Some(fill) = fill {
                    line.filled(fill)
                } else {
                    line
                }
            })
            .collect()
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
}

pub(super) fn receiving_status_line(triggers_paused: bool, width: usize) -> Option<RoleLine> {
    triggers_paused.then(|| clipped(TRIGGERS_PAUSED, width, Role::MutedText))
}

fn tokens(count: u64) -> String {
    compact_count(count).to_ascii_uppercase()
}

fn format_elapsed(milliseconds: u64) -> String {
    let seconds = milliseconds / 1000;
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    let hours = minutes / 60;
    let remaining_minutes = minutes % 60;
    if hours >= 24 {
        let days = hours / 24;
        let remaining_hours = hours % 24;
        return format!("{days}d {remaining_hours}h {remaining_minutes}m");
    }
    if remaining_minutes == 0 {
        format!("{hours}h")
    } else {
        format!("{hours}h {remaining_minutes}m")
    }
}

#[cfg(test)]
mod tests;

pub(super) fn notice_pieces(
    notice: &tickloop_agent_protocol::GoalNotice,
    objective: &str,
) -> Vec<super::console::ConversationPiece> {
    use tickloop_agent_protocol::{GoalBlock, GoalNotice};
    let (mut text, role): (String, Role) = match notice {
        GoalNotice::Set => ("Goal set".into(), Role::Accent),
        GoalNotice::Completed => ("Goal achieved".into(), Role::Success),
        GoalNotice::Blocked { .. } => ("Goal stalled".into(), Role::Warning),
        GoalNotice::BudgetLimited => ("Goal token budget exhausted".into(), Role::Warning),
        GoalNotice::Paused { .. } => ("Goal paused · /goal resume".into(), Role::MutedText),
        GoalNotice::Cleared => ("Goal cleared".into(), Role::MutedText),
    };
    if matches!(
        notice,
        GoalNotice::Set
            | GoalNotice::Completed
            | GoalNotice::Blocked { .. }
            | GoalNotice::BudgetLimited
    ) {
        let condition = super::conversation_picker::compact_line(&super::safe_text(objective));
        if !condition.is_empty() {
            text.push(' ');
            text.push_str(&condition);
        }
    }
    if let GoalNotice::Blocked { by } = notice {
        let reason = match by {
            GoalBlock::Reported { reason } | GoalBlock::CheckImpossible { reason } => {
                super::safe_line(reason)
            }
            GoalBlock::ExecutionFailed { error } => super::safe_line(error),
            GoalBlock::NoProgress => "no progress in three continuations".into(),
        };
        text.push_str(": ");
        text.push_str(&reason);
    }
    vec![
        super::console::ConversationPiece::Block,
        super::console::ConversationPiece::Notice {
            text: format!("● {text}"),
            role,
        },
    ]
}

pub(super) fn continuation_runs() -> Vec<(String, Option<Role>)> {
    vec![(
        format!("\n{}\n", tickloop_agent_protocol::GOAL_CONTINUATION_EVENT),
        Some(Role::MutedText),
    )]
}
