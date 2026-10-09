//! A hosted Tick's snapshot detail: its facts and a text snapshot of its screen, which never
//! counts as using the Tick or as a Tick Update.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use uuid::Uuid;

use super::{
    monitor::{MonitorEntry, member_facts, relative_time},
    tick_actions::{TickDetailKey, TickDetailLine, TickDetailView, builtin},
};
use tickloop_agent_protocol::HostedSnapshot;

pub(super) const REFRESH_KEY: char = 'r';
const MILLISECONDS_PER_SECOND: u64 = 1000;

enum Screen {
    Loading,
    Shown(HostedSnapshot),
    Unavailable(String),
}

pub(super) struct HostedDetail {
    id: Uuid,
    chat: Uuid,
    entry: MonitorEntry,
    screen: Screen,
}

impl HostedDetail {
    pub(super) fn new(chat: Uuid, entry: MonitorEntry) -> Self {
        Self {
            id: Uuid::new_v4(),
            chat,
            entry,
            screen: Screen::Loading,
        }
    }

    pub(super) fn show(&mut self, snapshot: Result<HostedSnapshot, String>) {
        self.screen = match snapshot {
            Ok(snapshot) => Screen::Shown(snapshot),
            Err(error) => Screen::Unavailable(error),
        };
    }

    fn screen_header(&self, now_ms: u64) -> String {
        match &self.screen {
            Screen::Loading => "Screen    loading…".into(),
            Screen::Unavailable(error) => {
                format!("Screen    unavailable: {}", super::safe_line(error))
            }
            Screen::Shown(snapshot) => format!(
                "Screen    {}×{} snapshot · taken {}{}",
                snapshot.screen.columns,
                snapshot.screen.rows,
                if now_ms.saturating_sub(snapshot.taken_at_ms) < MILLISECONDS_PER_SECOND {
                    "now".to_owned()
                } else {
                    format!("{} ago", relative_time(Some(snapshot.taken_at_ms), now_ms))
                },
                if snapshot.screen.truncated {
                    " · older lines cut"
                } else {
                    ""
                }
            ),
        }
    }
}

impl TickDetailView for HostedDetail {
    fn id(&self) -> Uuid {
        self.id
    }

    fn chat(&self) -> Uuid {
        self.chat
    }

    fn lines(&mut self, width: usize, height: usize, now: u64) -> Vec<TickDetailLine> {
        let mut lines = member_facts(&self.entry, now);
        lines.push(String::new());
        lines.extend(super::monitor::wrap_words(&self.screen_header(now), width));
        if let Screen::Shown(snapshot) = &self.screen {
            let room = height.saturating_sub(lines.len());
            lines.extend(fitted_screen(&snapshot.screen.lines, width, room));
        }
        lines
            .into_iter()
            .take(height)
            .map(|text| {
                let mut line = TickDetailLine::new(text);
                line.line.truncate(width);
                line
            })
            .collect()
    }

    fn hint(&self) -> String {
        format!("{REFRESH_KEY} refresh · esc back")
    }

    fn key(&mut self, key: KeyEvent, _width: usize, _height: usize) -> TickDetailKey {
        match (key.code, key.modifiers) {
            (KeyCode::Esc | KeyCode::Left, _) => TickDetailKey::Back,
            (KeyCode::Char(REFRESH_KEY), KeyModifiers::NONE) => TickDetailKey::Action(
                builtin::load_hosted_snapshot(self.entry.instance.clone(), self.id),
            ),
            _ => TickDetailKey::None,
        }
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

/// The snapshot lines that fit `rows` rows of `width` columns. When they do not all fit, the
/// newest lines are kept and a first line says what is shown, so the cut is never silent.
fn fitted_screen(screen: &[String], width: usize, rows: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthStr;
    let screen: Vec<String> = screen.iter().map(|line| super::safe_line(line)).collect();
    if screen.len() <= rows && screen.iter().all(|line| line.width() <= width) {
        return screen;
    }
    let disclosure = |shown: usize| {
        format!(
            "Showing the newest {shown} of {} lines, cut at {width} columns",
            screen.len()
        )
    };
    let disclosure_rows = super::monitor::wrap_words(&disclosure(screen.len()), width).len();
    let shown = rows.saturating_sub(disclosure_rows).min(screen.len());
    let mut fitted = super::monitor::wrap_words(&disclosure(shown), width);
    fitted.extend(screen[screen.len() - shown..].iter().cloned());
    fitted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_screen_too_big_for_the_detail_keeps_its_newest_lines_and_says_so() {
        let screen: Vec<String> = (1..=30)
            .map(|row| format!("row {row} {}", "x".repeat(60)))
            .collect();
        let fitted = fitted_screen(&screen, 40, 10);
        assert_eq!(fitted.len(), 10);
        assert_eq!(fitted[0], "Showing the newest 8 of 30 lines, cut at");
        assert_eq!(fitted[1], "40 columns");
        assert!(fitted[9].starts_with("row 30 "), "the newest line is kept");
        let small: Vec<String> = vec!["size 120x30".into(), "keys".into()];
        assert_eq!(
            fitted_screen(&small, 40, 10),
            small,
            "a fitting screen is shown whole"
        );
    }

    fn detail() -> HostedDetail {
        HostedDetail::new(Uuid::new_v4(), entry())
    }

    fn entry() -> MonitorEntry {
        MonitorEntry {
            instance: "hosted:1a26e59c-a887-457d-8540-5079aea79ca0"
                .parse()
                .unwrap(),
            group: crate::monitor_rows::TickGroup::Ticks,
            glyph: super::super::display::Glyph::ProtocolTick,
            title: "Keys".into(),
            kind: "Tick".into(),
            status: "live".into(),
            notes: Vec::new(),
            owner: crate::monitor_rows::TickOwner::ThisChat,
            tone: crate::monitor_rows::StatusTone::Neutral,
            updated_at_ms: None,
            update: None,
            resources: None,
            actions: Vec::new(),
        }
    }

    fn text(detail: &mut HostedDetail) -> String {
        detail
            .lines(60, 40, 10_000)
            .into_iter()
            .map(|line| line.line.text())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_snapshot_says_when_it_was_taken_and_that_older_lines_were_cut() {
        let mut detail = detail();
        assert!(text(&mut detail).contains("Screen    loading…"));
        assert_eq!(detail.hint(), "r refresh · esc back");
        detail.show(Ok(HostedSnapshot {
            screen: tickloop_agent_protocol::HostedScreenSnapshot {
                lines: vec!["newest".into()],
                truncated: true,
                rows: 30,
                columns: 120,
                alternate_screen: false,
            },
            taken_at_ms: 7_000,
        }));
        let shown = text(&mut detail);
        assert!(
            shown.contains("Screen    120×30 snapshot · taken 3s ago · older lines cut"),
            "{shown}"
        );
        assert!(shown.ends_with("newest"));
        detail.show(Err("TargetGone: the Tick is not running".into()));
        assert!(text(&mut detail).contains("Screen    unavailable: TargetGone"));
        assert!(matches!(
            detail.key(KeyEvent::from(KeyCode::Char(REFRESH_KEY)), 60, 40),
            TickDetailKey::Action(action) if action.key == REFRESH_KEY
        ));
        assert!(matches!(
            detail.key(KeyEvent::from(KeyCode::Esc), 60, 40),
            TickDetailKey::Back
        ));
    }
}
