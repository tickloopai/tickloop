//! A hosted Tick's Monitor detail: its facts and a text snapshot of its screen, which never
//! counts as using the Tick or as a Tick Update, and a full-screen attach of the same process.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tickloop_agent::{hosted::tools::HostedSnapshot, settings::MonitorPopup};
use uuid::Uuid;

use super::{
    monitor::{MonitorEntry, member_facts, relative_time},
    tick_actions::{TickDetailKey, TickDetailLine, TickDetailView, builtin},
};

pub(super) const ATTACH_KEY: char = 'a';
pub(super) const REFRESH_KEY: char = 'r';
const MILLISECONDS_PER_SECOND: u64 = 1000;

/// Why Enter shows a snapshot instead of floating the Tick in a popup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum NotFloated {
    NoHerdr,
    PopupOff,
    /// Herdr 0.9 can neither list nor target clients, so the popup could open in front of
    /// another user of the session.
    ClientUnverifiable,
}

impl NotFloated {
    pub(super) fn of(in_herdr: bool, popup: MonitorPopup) -> Self {
        match (in_herdr, popup) {
            (false, _) => Self::NoHerdr,
            (true, MonitorPopup::Disabled) => Self::PopupOff,
            (true, MonitorPopup::Enabled) => Self::ClientUnverifiable,
        }
    }

    fn reason(self) -> &'static str {
        match self {
            Self::NoHerdr => "No Herdr session to float it in",
            Self::PopupOff => {
                "The Monitor popup is off; `tickloop monitor popup install --herdr-socket SOCKET` turns it on"
            }
            Self::ClientUnverifiable => {
                "Herdr cannot tell which client asked, so a popup could open elsewhere"
            }
        }
    }
}

enum Screen {
    Loading,
    Shown(HostedSnapshot),
    Unavailable(String),
}

pub(super) struct HostedDetail {
    id: Uuid,
    chat: Uuid,
    entry: MonitorEntry,
    not_floated: NotFloated,
    /// Only a View with its own interactive terminal can hand that terminal to the Tick.
    can_take_over_terminal: bool,
    screen: Screen,
    note: Option<String>,
}

impl HostedDetail {
    pub(super) fn new(
        chat: Uuid,
        entry: MonitorEntry,
        not_floated: NotFloated,
        can_take_over_terminal: bool,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            chat,
            entry,
            not_floated,
            can_take_over_terminal,
            screen: Screen::Loading,
            note: None,
        }
    }

    pub(super) fn show(&mut self, snapshot: Result<HostedSnapshot, String>) {
        self.screen = match snapshot {
            Ok(snapshot) => Screen::Shown(snapshot),
            Err(error) => Screen::Unavailable(error),
        };
    }

    pub(super) fn set_note(&mut self, note: String) {
        self.note = Some(note);
    }

    /// Only a Tick whose screen the host just showed is running and can be attached.
    fn can_attach(&self) -> bool {
        self.can_take_over_terminal && matches!(self.screen, Screen::Shown(_))
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
        let explanations = [
            Some(format!("Not floated · {}", self.not_floated.reason())),
            self.note.clone(),
            Some(self.screen_header(now)),
        ];
        for explanation in explanations.into_iter().flatten() {
            lines.extend(wrap_words(&explanation, width));
        }
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
        if self.can_attach() {
            format!("{ATTACH_KEY} attach full screen · {REFRESH_KEY} refresh · Esc back")
        } else {
            format!("{REFRESH_KEY} refresh · Esc back")
        }
    }

    fn key(&mut self, key: KeyEvent, _width: usize, _height: usize) -> TickDetailKey {
        match (key.code, key.modifiers) {
            (KeyCode::Esc | KeyCode::Left, _) => TickDetailKey::Back,
            (KeyCode::Char(REFRESH_KEY), KeyModifiers::NONE) => TickDetailKey::Action(
                builtin::load_hosted_snapshot(self.entry.instance.clone(), self.id),
            ),
            (KeyCode::Char(ATTACH_KEY), KeyModifiers::NONE) if self.can_attach() => {
                TickDetailKey::Action(builtin::attach_hosted_full_screen(
                    self.entry.instance.clone(),
                    self.id,
                ))
            }
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
    let disclosure_rows = wrap_words(&disclosure(screen.len()), width).len();
    let shown = rows.saturating_sub(disclosure_rows).min(screen.len());
    let mut fitted = wrap_words(&disclosure(shown), width);
    fitted.extend(screen[screen.len() - shown..].iter().cloned());
    fitted
}

/// Breaks `text` between words so each line fits `width` columns; a longer word is cut later.
fn wrap_words(text: &str, width: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthStr;
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        if !line.is_empty() && line.width() + 1 + word.width() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    lines.push(line);
    lines
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

    #[test]
    fn explanations_break_between_words_to_fit_the_detail() {
        assert_eq!(
            wrap_words("Not floated · No Herdr session to float it in", 20),
            ["Not floated · No", "Herdr session to", "float it in"]
        );
    }

    fn detail(can_take_over_terminal: bool) -> HostedDetail {
        let entry = MonitorEntry {
            instance: "hosted:1a26e59c-a887-457d-8540-5079aea79ca0"
                .parse()
                .unwrap(),
            group: crate::tick_adapter::TickGroup::Ticks,
            glyph: super::super::display::Glyph::ProtocolTick,
            title: "Keys".into(),
            kind: "Tick".into(),
            status: "live".into(),
            owner: crate::tick_adapter::TickOwner::ThisChat,
            updated_at_ms: None,
            resources: None,
            actions: Vec::new(),
        };
        HostedDetail::new(
            Uuid::new_v4(),
            entry,
            NotFloated::NoHerdr,
            can_take_over_terminal,
        )
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
        let mut detail = detail(true);
        assert!(text(&mut detail).contains("Screen    loading…"));
        assert_eq!(
            detail.hint(),
            "r refresh · Esc back",
            "nothing to attach yet"
        );
        detail.show(Ok(HostedSnapshot {
            screen: tick_host::screen::ScreenSnapshot {
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
        assert_eq!(detail.hint(), "a attach full screen · r refresh · Esc back");
        detail.show(Err("TargetGone: the Tick is not running".into()));
        assert!(text(&mut detail).contains("Screen    unavailable: TargetGone"));
        assert_eq!(
            detail.hint(),
            "r refresh · Esc back",
            "a gone Tick is not attached"
        );
    }

    #[test]
    fn a_view_without_its_own_terminal_never_offers_to_attach() {
        let mut detail = detail(false);
        detail.show(Ok(HostedSnapshot {
            screen: tick_host::screen::ScreenSnapshot {
                lines: Vec::new(),
                truncated: false,
                rows: 30,
                columns: 120,
                alternate_screen: false,
            },
            taken_at_ms: 10_000,
        }));
        assert_eq!(detail.hint(), "r refresh · Esc back");
        assert!(matches!(
            detail.key(KeyEvent::from(KeyCode::Char('a')), 60, 40),
            TickDetailKey::None
        ));
    }

    #[test]
    fn the_reason_names_what_keeps_the_tick_from_floating() {
        assert_eq!(
            NotFloated::of(false, MonitorPopup::Enabled),
            NotFloated::NoHerdr
        );
        assert_eq!(
            NotFloated::of(true, MonitorPopup::Disabled),
            NotFloated::PopupOff
        );
        assert_eq!(
            NotFloated::of(true, MonitorPopup::Enabled),
            NotFloated::ClientUnverifiable,
            "an installed popup still cannot prove which client asked"
        );
    }
}
