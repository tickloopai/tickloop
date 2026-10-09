use super::{
    console::{AgentRow, MIN_REQUEST_AGENT_TITLE_COLUMNS, WIDE_TERMINAL_COLUMNS, truncate_width},
    display::{DisplayProfile, Glyph},
    list_row,
    monitor::relative_time,
    style::{Role, RoleLine},
};
use crate::agent_console::AttentionGroup;
use unicode_width::UnicodeWidthStr;

const NAME_DESCRIPTION_GAP: &str = "  ";
const STATUS_GAP_COLUMNS: usize = 2;

fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// One member of the Subagents panel. The dot is filled for the member this View shows and
/// hollow for the others; its steady colour is the member's state, as the glyph of a Monitor row
/// except that it never blinks. The right side keeps only details: how long a working member has run,
/// how long ago any other member last changed state, or its waiting requests.
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
    let identity = agent_role(row);
    let marker = if selected { "› " } else { "  " };
    let dot = display.glyph(if viewed {
        Glyph::AgentViewed
    } else {
        Glyph::AgentUnviewed
    });
    let mut line = RoleLine::role(marker, identity.unwrap_or(role));
    line.push(dot, Some(state_mark(row)));
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
        (time_detail(row, now), Role::MutedText)
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
    line.push(
        name,
        Some(if selected {
            role
        } else {
            identity.unwrap_or(role)
        }),
    );
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

/// The colour of a member's dot: accent while it works, warning while it needs
/// input or after it stopped, error after it failed, success after it completed, muted otherwise.
fn state_mark(row: &AgentRow) -> Role {
    if row.group == AttentionGroup::Working {
        return Role::RunningMark;
    }
    match row.glyph_role() {
        Role::Success => Role::SuccessMark,
        Role::Error => Role::ErrorMark,
        Role::Warning => Role::WarningMark,
        _ => Role::MutedText,
    }
}

/// How long a working member has run, or how long ago another member last changed state.
fn time_detail(row: &AgentRow, now: u64) -> String {
    match row.group {
        AttentionGroup::Working => row
            .subagent
            .as_ref()
            .map(|registration| {
                list_row::format_elapsed(now.saturating_sub(registration.created_at))
            })
            .unwrap_or_default(),
        _ => row
            .updated_at
            .map(|updated_at| relative_time(Some(updated_at), now))
            .unwrap_or_default(),
    }
}

/// When time alone next changes `row`'s line: the next second of its run while it works,
/// otherwise the next change of its age.
pub(super) fn next_change_ms(row: &AgentRow, now: u64) -> Option<u64> {
    if row.group == AttentionGroup::Working {
        const SECOND: u64 = 1000;
        return row.subagent.as_ref().map(|registration| {
            let since = registration.created_at;
            since + (now.saturating_sub(since) / SECOND + 1) * SECOND
        });
    }
    let is_family_member = row.parent_id.is_some() || row.subagent.is_some();
    let shows_age = is_family_member && row.approvals + row.questions == 0;
    row.updated_at
        .filter(|_| shows_age)
        .map(|updated_at| next_age_change_ms(updated_at, now))
}

/// When the age [`relative_time`] shows for `since` next changes: each second under a minute,
/// each minute under an hour, then each hour and each day.
fn next_age_change_ms(since: u64, now: u64) -> u64 {
    const SECOND: u64 = 1000;
    let age = now.saturating_sub(since);
    let unit = match age / SECOND {
        0..60 => SECOND,
        60..3600 => 60 * SECOND,
        3600..86400 => 3600 * SECOND,
        _ => 86400 * SECOND,
    };
    since + (age / unit + 1) * unit
}

/// The member's Agent Color, when the runtime assigned one.
pub(super) fn agent_role(row: &AgentRow) -> Option<Role> {
    row.subagent
        .as_ref()
        .and_then(|registration| registration.color)
        .map(Role::agent)
}

fn ellipsize(text: &str, width: usize) -> String {
    if display_width(text) <= width {
        text.to_owned()
    } else if width == 0 {
        String::new()
    } else {
        format!("{}…", truncate_width(text, width - 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_age_changes_each_second_then_each_minute_hour_and_day() {
        for (age, next) in [
            (0, 1_000),
            (1_500, 2_000),
            (59_999, 60_000),
            (60_000, 120_000),
            (3_599_000, 3_600_000),
            (3_600_000, 7_200_000),
            (90_000_000, 172_800_000),
        ] {
            assert_eq!(next_age_change_ms(10_000, 10_000 + age), 10_000 + next);
        }
    }
}
