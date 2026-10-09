use super::{
    console::{AgentRow, MIN_REQUEST_AGENT_TITLE_COLUMNS, WIDE_TERMINAL_COLUMNS, truncate_width},
    display::{DisplayProfile, Glyph},
    style::{Role, RoleLine},
};
use tickloop_agent_protocol::LoopState;
use unicode_width::UnicodeWidthStr;

const NAME_DESCRIPTION_GAP: &str = "  ";
const STATUS_GAP_COLUMNS: usize = 2;

fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

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
    let marker = if selected { "› " } else { "  " };
    let dot = display.glyph(if viewed {
        Glyph::AgentViewed
    } else {
        Glyph::AgentUnviewed
    });
    let mut line = RoleLine::role(marker, role);
    line.push(
        dot,
        Some(if row.parent_id.is_some() {
            agent_color(
                row.subagent
                    .as_ref()
                    .map_or(row.agent_id, |registration| registration.child_chat_id),
            )
        } else {
            role
        }),
    );
    line.push(" ", Some(role));
    if row.parent_id.is_none() && row.subagent.is_none() {
        line.push("main", Some(role));
        line.truncate(width);
        return line;
    }
    let status = if row.approvals + row.questions > 0 {
        [
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
        .join(" ")
    } else if row.state == LoopState::Running {
        row.subagent
            .as_ref()
            .map(|registration| format_duration(now.saturating_sub(registration.created_at)))
            .unwrap_or_else(|| "running".into())
    } else if row.state == LoopState::Idle {
        "idle".into()
    } else {
        row.status.to_ascii_lowercase()
    };
    let suffix = format!("{} {status}", row.state_glyph(display, now));
    let prefix_columns = display_width(&line.text());
    let suffix = truncate_width(
        &suffix,
        width.saturating_sub(prefix_columns + MIN_REQUEST_AGENT_TITLE_COLUMNS),
    );
    let free = width.saturating_sub(prefix_columns + display_width(&suffix) + STATUS_GAP_COLUMNS);
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
        " ".repeat(width.saturating_sub(display_width(&line.text()) + display_width(&suffix))),
        None,
    );
    line.push(suffix, Some(row.status_role()));
    line.truncate(width);
    line
}

fn format_duration(milliseconds: u64) -> String {
    let seconds = if milliseconds < 60_000 {
        milliseconds / 1000
    } else {
        milliseconds.saturating_add(500) / 1000
    };
    let days = seconds / 86400;
    let hours = seconds % 86400 / 3600;
    let minutes = seconds % 3600 / 60;
    let remainder = seconds % 60;
    if days > 0 {
        format!("{days}d {hours}h {minutes}m")
    } else if hours > 0 {
        format!("{hours}h {minutes}m {remainder}s")
    } else if minutes > 0 {
        format!("{minutes}m {remainder}s")
    } else {
        format!("{remainder}s")
    }
}

pub(super) fn agent_color(agent: uuid::Uuid) -> Role {
    let roles = [
        Role::Accent,
        Role::Permission,
        Role::Success,
        Role::PlanMode,
    ];
    roles[(agent.as_u128() % roles.len() as u128) as usize]
}

pub(super) fn child_badge(
    line: &mut RoleLine,
    available: usize,
    handle: &str,
    role: Role,
    border: Role,
    protected_end: usize,
) {
    let badge = format!(" @{handle} ");
    let columns = display_width(&badge);
    if columns + 1 > available || available.saturating_sub(columns) < protected_end {
        return;
    }
    line.truncate(available - columns);
    line.push(badge, Some(role));
    line.push("─", Some(border));
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
