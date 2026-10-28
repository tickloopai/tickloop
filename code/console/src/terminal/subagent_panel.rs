use super::{
    console::{AgentRow, MIN_REQUEST_AGENT_TITLE_COLUMNS, WIDE_TERMINAL_COLUMNS, truncate_width},
    display::{DisplayProfile, Glyph},
    list_row,
    loop_view_text::ellipsize,
    style::{Role, RoleLine},
};
use crate::agent_console::{AgentState, AttentionGroup};
use unicode_width::UnicodeWidthStr;

const NAME_DESCRIPTION_GAP: &str = "  ";
const STATUS_GAP_COLUMNS: usize = 2;

fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// One member of the Subagents panel. The dot is filled for the member this View shows and
/// hollow for the others, and only it carries the member's Agent Color; `main` has none, so its
/// dot keeps the row's colour, as does every name. The right side reports availability, the
/// current running turn's duration, or waiting requests.
pub(super) fn family_line(
    row: &AgentRow,
    selected: bool,
    viewed: bool,
    layout: (usize, usize, DisplayProfile, u64),
) -> RoleLine {
    let (width, name_columns, display, now) = layout;
    let role = if viewed {
        Role::Title
    } else if selected {
        Role::Text
    } else {
        Role::MutedText
    };
    let dot = display.glyph(if viewed {
        Glyph::AgentViewed
    } else {
        Glyph::AgentUnviewed
    });
    let mut line = RoleLine::role(list_row::ROW_INDENT, role);
    line.push(dot, Some(agent_role(row).unwrap_or(role)));
    line.push(" ", Some(role));
    if row.parent_id.is_none() && row.subagent.is_none() {
        line.push("main", Some(role));
        line.truncate(width);
        return line;
    }
    let (details, details_role) = if row.approvals + row.questions > 0 {
        let requests = [
            ("approvals", "A", row.approvals),
            ("questions", "Q", row.questions),
        ]
        .into_iter()
        .filter(|(_, _, count)| *count > 0)
        .map(|(word, short, count)| {
            if width >= WIDE_TERMINAL_COLUMNS {
                format!("{word} {count}")
            } else {
                format!("{short}{count}")
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
        (requests, row.status_role())
    } else {
        (
            state_detail(row, now),
            if row.agent_state == AgentState::Completed
                && row.state == tickloop_harness::protocol::LoopState::Idle
            {
                Role::MutedText
            } else {
                row.status_role()
            },
        )
    };
    let prefix_columns = display_width(&line.text());
    let details = truncate_width(
        &details,
        width.saturating_sub(prefix_columns + MIN_REQUEST_AGENT_TITLE_COLUMNS),
    );
    let free = width.saturating_sub(prefix_columns + display_width(&details) + STATUS_GAP_COLUMNS);
    let name_width = name_columns.min(free);
    let name = ellipsize(
        row.subagent
            .as_ref()
            .map_or("child", |registration| registration.handle.as_str()),
        name_width,
    );
    line.push(name, Some(role));
    if let Some(description) = row
        .subagent
        .as_ref()
        .map(|registration| registration.description.as_str())
    {
        let description_width =
            free.saturating_sub(name_width + display_width(NAME_DESCRIPTION_GAP));
        if description_width > 0 {
            let padding = name_width
                .saturating_sub(display_width(&line.text()).saturating_sub(prefix_columns));
            line.push(
                format!(
                    "{}{NAME_DESCRIPTION_GAP}{}",
                    " ".repeat(padding),
                    ellipsize(&super::safe_line(description), description_width)
                ),
                Some(Role::MutedText),
            );
        }
    }
    line.push(
        " ".repeat(width.saturating_sub(display_width(&line.text()) + display_width(&details))),
        None,
    );
    line.push(details, Some(details_role));
    line.truncate(width);
    line
}

fn state_detail(row: &AgentRow, now: u64) -> String {
    if row.group == AttentionGroup::Working {
        return row.run_started_at.map_or_else(
            || row.status.clone(),
            |started_at| list_row::format_elapsed(now.saturating_sub(started_at)),
        );
    }
    if row.agent_state == AgentState::Idle
        || (row.agent_state == AgentState::Completed
            && row.state == tickloop_harness::protocol::LoopState::Idle)
    {
        return "idle".into();
    }
    row.status.clone()
}

pub(super) fn next_change_ms(row: &AgentRow, now: u64) -> Option<u64> {
    if row.group != AttentionGroup::Working
        || row.approvals + row.questions > 0
        || (row.parent_id.is_none() && row.subagent.is_none())
    {
        return None;
    }
    const SECOND: u64 = 1000;
    row.run_started_at
        .map(|since| since + (now.saturating_sub(since) / SECOND + 1) * SECOND)
}

/// The member's Agent Color, when the runtime assigned one.
pub(super) fn agent_role(row: &AgentRow) -> Option<Role> {
    row.subagent
        .as_ref()
        .and_then(|registration| registration.color)
        .map(Role::agent)
}
