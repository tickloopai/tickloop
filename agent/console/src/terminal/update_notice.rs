use tickloop_agent_updater::UpdateNotice;

use super::console::display_single_line;
use super::style::{Role, RoleLine};
use unicode_width::UnicodeWidthStr;

pub(super) fn line(notice: &UpdateNotice, width: usize) -> RoleLine {
    let (message, compact, role) = match notice {
        UpdateNotice::Checking => ("Checking for updates…".into(), "Checking…", Role::MutedText),
        UpdateNotice::Installing => ("Updating Tickloop…".into(), "Updating…", Role::MutedText),
        UpdateNotice::Ready { version } => (
            format!("Updated to {} · next launch", display_single_line(version)),
            "Update ready",
            Role::Success,
        ),
        UpdateNotice::Failed => (
            "Update failed · tickloop update".into(),
            "Update failed",
            Role::Warning,
        ),
    };
    let text = if UnicodeWidthStr::width(message.as_str()) <= width {
        message.as_str()
    } else if UnicodeWidthStr::width(compact) <= width {
        compact
    } else {
        return RoleLine::default();
    };
    let mut line = RoleLine::role(text, role);
    line.truncate(width);
    line
}
