#![cfg_attr(
    not(test),
    expect(dead_code, reason = "tickets 07 and 09 wire the views")
)]

use time::OffsetDateTime;

use super::{
    display::{DisplayProfile, Glyph},
    list_row::{self, ListRow, RowFlash},
    loop_view_text::{clipped, wrapped},
    style::{Role, RoleLine},
};
use crate::monitor_rows::NextCheck;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ScheduleView<'a> {
    Interval { minutes: u32, cron: &'a str },
    Cron(&'a str),
    SelfPaced,
}

impl ScheduleView<'_> {
    fn label(self) -> String {
        match self {
            Self::Interval { minutes, .. } => format!("every {minutes}m"),
            Self::Cron(cron) => super::safe_line(cron),
            Self::SelfPaced => "self-paced".into(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct ScheduledTaskRender {
    pub(super) display: DisplayProfile,
    pub(super) ascii: bool,
    pub(super) now: OffsetDateTime,
    pub(super) now_ms: u64,
}

/// Display timestamps carry their local offsets at each instant, including across DST changes.
#[derive(Clone, Debug)]
pub(super) struct ScheduledTaskView<'a> {
    pub(super) schedule: ScheduleView<'a>,
    pub(super) instruction: &'a str,
    pub(super) created_at: OffsetDateTime,
    pub(super) expires_at: Option<OffsetDateTime>,
    pub(super) last_trigger_at: Option<OffsetDateTime>,
    pub(super) next_trigger_at: Option<OffsetDateTime>,
    pub(super) skipped: u64,
    pub(super) pending_confirmation: bool,
    pub(super) triggers_paused: bool,
    /// A live trigger's display-clock timestamp, independent of the schedule's wall clock.
    pub(super) triggered_at_ms: Option<u64>,
}

impl ScheduledTaskView<'_> {
    pub(super) fn row_lines(
        &self,
        width: usize,
        selected: bool,
        render: ScheduledTaskRender,
    ) -> Vec<RoleLine> {
        if width == 0 {
            return Vec::new();
        }
        let ScheduledTaskRender {
            display,
            ascii,
            now,
            now_ms,
        } = render;
        let glyph = if ascii {
            "@"
        } else {
            display.glyph(Glyph::AgentWaiting)
        };
        let name = format!(
            "{} · {}",
            self.schedule.label(),
            super::safe_line(self.instruction)
        );
        let next = self.next_trigger_at.map_or_else(
            || "not scheduled".into(),
            |next| format!("{:02}:{:02}", next.hour(), next.minute()),
        );
        let details = if self.pending_confirmation {
            format!("{next} · pending confirmation")
        } else {
            next
        };
        let columns = width.saturating_sub(list_row::name_column(glyph));
        let bar_columns = self.progress(columns, now);
        let fill = if self.triggers_paused {
            None
        } else {
            list_row::flash_fill(
                self.triggered_at_ms.map(|since| (RowFlash::TakeIn, since)),
                display,
                now_ms,
            )
        };
        let mut lines = ListRow {
            selected,
            glyph,
            glyph_role: if self.triggers_paused {
                Role::MutedText
            } else if self.pending_confirmation {
                Role::WarningMark
            } else {
                Role::Accent
            },
            name: &name,
            details,
            fill,
            subscription: None,
            bar_columns,
        }
        .lines(width);
        if self.triggers_paused {
            for line in &mut lines {
                if let Some(band) = &mut line.band {
                    band.role = Role::UserMessageBg;
                }
                line.fill = selected.then_some(Role::Selection);
                for span in &mut line.spans {
                    span.role = Some(Role::MutedText);
                }
            }
        }
        lines
    }

    fn progress(&self, columns: usize, now: OffsetDateTime) -> Option<usize> {
        if self.pending_confirmation {
            return None;
        }
        let next = self.next_trigger_at?;
        if now >= next {
            return Some(columns);
        }
        let start = self.last_trigger_at.unwrap_or(self.created_at);
        let wait_ms = u64::try_from((next - start).whole_milliseconds()).ok()?;
        if wait_ms == 0 {
            return None;
        }
        let elapsed_ms = u64::try_from((now - start).whole_milliseconds()).unwrap_or(0);
        Some(
            NextCheck {
                at_ms: wait_ms,
                wait_ms,
            }
            .elapsed_columns(columns, elapsed_ms),
        )
    }

    pub(super) fn detail_lines(&self, width: usize, top: usize, height: usize) -> Vec<RoleLine> {
        if width == 0 {
            return Vec::new();
        }
        let mut lines = vec![clipped("Scheduled task", width, Role::Title)];
        let cron = match self.schedule {
            ScheduleView::Interval { cron, .. } | ScheduleView::Cron(cron) => cron,
            ScheduleView::SelfPaced => "self-paced",
        };
        for (label, value) in [
            ("Cron", cron.to_owned()),
            ("Instruction", self.instruction.to_owned()),
            ("Created", timestamp(self.created_at)),
            (
                "Expires",
                self.expires_at.map_or_else(|| "none".into(), timestamp),
            ),
            ("Skipped", self.skipped.to_string()),
        ] {
            lines.extend(wrapped(&format!("{label}: {value}"), width, Role::Text));
        }
        lines.into_iter().skip(top).take(height).collect()
    }
}

fn timestamp(at: OffsetDateTime) -> String {
    format!("{} {:02}:{:02}", at.date(), at.hour(), at.minute())
}

#[cfg(test)]
mod tests;
