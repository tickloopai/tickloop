use time::OffsetDateTime;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::{
    display::{DisplayProfile, Glyph},
    list_row::{self, RowFlash},
    loop_view_text::{clipped, ellipsize, wrapped},
    style::{Role, RoleLine},
};
use crate::monitor_rows::NextCheck;

pub(super) const PROGRESS_WINDOW_MS: u64 = 30 * 60 * 1000;
const SEGMENT_SEPARATOR: &str = " · ";
const MINUTES_PER_HOUR: u32 = 60;
const MINUTES_PER_DAY: u32 = 24 * MINUTES_PER_HOUR;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ScheduleView<'a> {
    Interval { minutes: u32, cron: &'a str },
    Cron(&'a str),
    Once(&'a str),
    SelfPaced,
}

impl ScheduleView<'_> {
    fn label(self) -> String {
        match self {
            Self::Interval { minutes, .. } => {
                if minutes.is_multiple_of(MINUTES_PER_DAY) {
                    format!("{}d", minutes / MINUTES_PER_DAY)
                } else if minutes.is_multiple_of(MINUTES_PER_HOUR) {
                    format!("{}h", minutes / MINUTES_PER_HOUR)
                } else {
                    format!("{minutes}m")
                }
            }
            Self::Cron(cron) => super::safe_line(cron),
            Self::Once(_) => "once".into(),
            Self::SelfPaced => "auto".into(),
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
    pub(super) scheduled_at: Option<OffsetDateTime>,
    pub(super) source: &'a str,
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
        let indent = list_row::compact_name_column(glyph).min(width);
        let next = if self.triggers_paused {
            "paused".into()
        } else {
            self.scheduled_at.map_or_else(
                || {
                    if self.schedule == ScheduleView::SelfPaced {
                        "~".into()
                    } else {
                        "not scheduled".into()
                    }
                },
                |next| {
                    let time = if next.date() == now.date() {
                        format!("{:02}:{:02}", next.hour(), next.minute())
                    } else {
                        format!(
                            "{:02}-{:02} {:02}:{:02}",
                            u8::from(next.month()),
                            next.day(),
                            next.hour(),
                            next.minute()
                        )
                    };
                    if self.schedule == ScheduleView::SelfPaced {
                        format!("~{time}")
                    } else {
                        time
                    }
                },
            )
        };
        let next = ellipsize(&next, width - indent);
        let separator_width = SEGMENT_SEPARATOR.width();
        let cadence_columns = width.saturating_sub(indent + next.width() + separator_width);
        let cadence = ellipsize(
            &format!(
                "{}{}",
                if self.pending_confirmation {
                    "pending "
                } else {
                    ""
                },
                self.schedule.label()
            ),
            cadence_columns,
        );
        let role = if selected {
            Role::Text
        } else {
            Role::MutedText
        };
        let mut header = RoleLine::role(
            glyph,
            if self.triggers_paused {
                Role::MutedText
            } else if self.pending_confirmation {
                Role::WarningMark
            } else {
                Role::Accent
            },
        );
        if width > glyph.width() {
            header.push(" ", Some(role));
        }
        header.push(cadence, Some(Role::MutedText));
        if cadence_columns > 0 {
            header.push(SEGMENT_SEPARATOR, Some(Role::MutedText));
        }
        header.push(next, Some(Role::MutedText));
        let mut instruction = RoleLine::role(" ".repeat(indent), role);
        instruction.push(
            ellipsize(&super::safe_line(self.instruction), width - indent),
            Some(role),
        );
        let fill = if self.triggers_paused {
            None
        } else {
            list_row::flash_fill(
                self.triggered_at_ms.map(|since| (RowFlash::TakeIn, since)),
                display,
                now_ms,
            )
        };
        if fill.is_none()
            && let Some(columns) = self
                .progress(width - indent, now)
                .filter(|columns| *columns > 0)
        {
            let end = floor_display_column(&header.text(), indent + columns);
            if end > indent {
                header = header.banded(
                    indent..end,
                    if self.triggers_paused {
                        Role::UserMessageBg
                    } else {
                        Role::UpdateProgress
                    },
                );
            }
        }
        [header, instruction]
            .into_iter()
            .map(|mut line| {
                if self.triggers_paused {
                    for span in &mut line.spans {
                        span.role = Some(Role::MutedText);
                    }
                }
                if selected {
                    line.filled(Role::Selection)
                } else if let Some(fill) = fill {
                    line.filled_from(fill, indent)
                } else {
                    line
                }
            })
            .collect()
    }

    fn progress(&self, columns: usize, now: OffsetDateTime) -> Option<usize> {
        if self.pending_confirmation || self.triggers_paused {
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
        let window_ms = wait_ms.min(PROGRESS_WINDOW_MS);
        let remaining_ms = u64::try_from((next - now).whole_milliseconds()).ok()?;
        Some(
            NextCheck {
                at_ms: window_ms,
                wait_ms: window_ms,
            }
            .elapsed_columns(columns, window_ms.saturating_sub(remaining_ms)),
        )
    }

    pub(super) fn detail_lines(&self, width: usize, top: usize, height: usize) -> Vec<RoleLine> {
        if width == 0 {
            return Vec::new();
        }
        let mut lines = vec![clipped("Scheduled task", width, Role::Title)];
        let cron = match self.schedule {
            ScheduleView::Interval { cron, .. }
            | ScheduleView::Cron(cron)
            | ScheduleView::Once(cron) => cron,
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
            (
                "Next run",
                self.next_trigger_at.map_or_else(
                    || "not scheduled".into(),
                    |at| {
                        at.format(&time::format_description::well_known::Rfc3339)
                            .unwrap_or_else(|_| timestamp(at))
                    },
                ),
            ),
            ("Source", self.source.into()),
            ("Skipped", self.skipped.to_string()),
        ] {
            lines.extend(wrapped(&format!("{label}: {value}"), width, Role::Text));
        }
        lines.into_iter().skip(top).take(height).collect()
    }
}

fn floor_display_column(text: &str, boundary: usize) -> usize {
    let mut column = 0;
    for grapheme in text.graphemes(true) {
        let end = column + grapheme.width();
        if end > boundary {
            return column;
        }
        column = end;
    }
    boundary
}

fn timestamp(at: OffsetDateTime) -> String {
    format!("{} {:02}:{:02}", at.date(), at.hour(), at.minute())
}

#[cfg(test)]
mod tests;
