//! The terminal model of a hidden Tick. Output is parsed continuously so the screen stays current
//! while nobody watches; no raw output log is kept, only the screen and bounded scrollback.
//! Key-bubbling sequences and focus-report modes in the output are taken out as signals.

use crate::{
    pty::TerminalSize,
    screen_signals::{OscLimit, ScreenSignals, SignalCollector},
};

pub const SCROLLBACK_ROWS: usize = 1000;
pub const SNAPSHOT_MAX_LINES: usize = 200;
pub const SNAPSHOT_MAX_BYTES: usize = 64 * 1024;

pub struct Screen {
    parser: vt100::Parser<SignalCollector>,
    osc_limit: OscLimit,
}

/// The latest visible text, newest lines kept when the limits cut it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScreenSnapshot {
    pub lines: Vec<String>,
    pub truncated: bool,
}

impl Screen {
    pub fn new(size: TerminalSize) -> Self {
        Self {
            parser: vt100::Parser::new_with_callbacks(
                size.rows,
                size.columns,
                SCROLLBACK_ROWS,
                SignalCollector::default(),
            ),
            osc_limit: OscLimit::default(),
        }
    }

    /// Parses the program's output and returns what it asked of the views.
    pub fn process(&mut self, bytes: &[u8]) -> ScreenSignals {
        let bytes = self.osc_limit.filter(bytes);
        self.parser.process(&bytes);
        self.parser.callbacks_mut().take()
    }

    /// Shrinking keeps each screen's cursor on screen as a terminal does: the rows above it that
    /// no longer fit scroll off the top, into the scrollback on the normal screen, where `vt100`
    /// alone would cut the bottom rows. The alternate screen follows the same rule although its
    /// program redraws anyway, and the hidden normal screen does too, so the newest output is
    /// still in view when a full-screen program returns to it. Growing adds empty rows below.
    pub fn resize(&mut self, size: TerminalSize) {
        let alternate = self.parser.screen().alternate_screen();
        self.scroll_cursor_within(size.rows);
        // `?47` switches screens without clearing either or moving a cursor.
        self.parser.process(if alternate {
            b"\x1b[?47l"
        } else {
            b"\x1b[?47h"
        });
        self.scroll_cursor_within(size.rows);
        self.parser.process(if alternate {
            b"\x1b[?47h"
        } else {
            b"\x1b[?47l"
        });
        self.parser.screen_mut().set_size(size.rows, size.columns);
        // The sequences written above end any OSC the parser was inside.
        self.osc_limit = OscLimit::default();
    }

    /// Scrolls the shown screen up until its cursor fits in `rows` rows, leaving the cursor on
    /// the last of them. The scroll margins are reset first, as a terminal resize resets them, so
    /// the whole screen scrolls.
    fn scroll_cursor_within(&mut self, rows: u16) {
        let (cursor_row, cursor_column) = self.parser.screen().cursor_position();
        let overflow = (cursor_row + 1).saturating_sub(rows);
        if overflow == 0 {
            return;
        }
        let column = cursor_column + 1;
        self.parser
            .process(format!("\x1b[r\x1b[{overflow}S\x1b[{rows};{column}H").as_bytes());
    }

    pub fn size(&self) -> TerminalSize {
        let (rows, columns) = self.parser.screen().size();
        TerminalSize { rows, columns }
    }

    /// The terminal output that brings a view showing `shown` to the current screen, with the
    /// screen it then shows. A first view, or one of another size, gets the full frame.
    pub fn frame_since(&self, shown: Option<&vt100::Screen>) -> (Vec<u8>, vt100::Screen) {
        let current = self.parser.screen();
        let bytes = match shown {
            Some(shown) if shown.size() == current.size() => {
                let mut bytes = current.contents_diff(shown);
                bytes.extend(current.input_mode_diff(shown));
                bytes
            }
            _ => {
                let mut bytes = current.contents_formatted();
                bytes.extend(current.input_mode_formatted());
                bytes
            }
        };
        (bytes, current.clone())
    }

    pub fn snapshot(&self) -> ScreenSnapshot {
        let text = self.parser.screen().contents();
        let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
        while lines.last().is_some_and(|line| line.trim().is_empty()) {
            lines.pop();
        }
        let mut truncated = false;
        if lines.len() > SNAPSHOT_MAX_LINES {
            lines.drain(..lines.len() - SNAPSHOT_MAX_LINES);
            truncated = true;
        }
        let mut bytes: usize = lines.iter().map(|line| line.len() + 1).sum();
        while bytes > SNAPSHOT_MAX_BYTES && !lines.is_empty() {
            bytes -= lines.remove(0).len() + 1;
            truncated = true;
        }
        ScreenSnapshot { lines, truncated }
    }

    #[cfg(test)]
    fn scrollback_rows(&mut self) -> usize {
        let screen = self.parser.screen_mut();
        let position = screen.scrollback();
        screen.set_scrollback(usize::MAX);
        let available = screen.scrollback();
        screen.set_scrollback(position);
        available
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pty::{DEFAULT_COLUMNS, DEFAULT_ROWS};

    #[test]
    fn scrollback_is_bounded_and_snapshots_keep_the_newest_lines() {
        let mut screen = Screen::new(TerminalSize::default());
        for line in 0..1500 {
            screen.process(format!("line {line}\r\n").as_bytes());
        }
        assert_eq!(screen.scrollback_rows(), SCROLLBACK_ROWS);
        let snapshot = screen.snapshot();
        assert!(snapshot.lines.len() <= SNAPSHOT_MAX_LINES);
        assert_eq!(snapshot.lines.last().unwrap(), "line 1499");
    }

    #[test]
    fn wide_characters_and_the_alternate_screen_are_modelled() {
        let mut screen = Screen::new(TerminalSize::default());
        screen.process("\x1b[?1049h行情 CJK".as_bytes());
        assert!(screen.parser.screen().alternate_screen());
        assert_eq!(screen.snapshot().lines[0], "行情 CJK");
        screen.resize(TerminalSize::validated(10, 40).unwrap());
        assert_eq!(screen.size(), TerminalSize::validated(10, 40).unwrap());
    }

    #[test]
    fn a_view_gets_the_full_frame_first_and_then_only_changes() {
        let mut screen = Screen::new(TerminalSize::default());
        screen.process(b"\x1b[?1000hfirst line");
        let (full, shown) = screen.frame_since(None);
        let mut view = vt100::Parser::new(DEFAULT_ROWS, DEFAULT_COLUMNS, 0);
        view.process(&full);
        assert_eq!(view.screen().contents(), "first line");
        assert_eq!(
            view.screen().mouse_protocol_mode(),
            vt100::MouseProtocolMode::PressRelease
        );
        let (unchanged, shown) = screen.frame_since(Some(&shown));
        assert!(unchanged.is_empty());
        screen.process(b"\r\nsecond");
        let (change, _) = screen.frame_since(Some(&shown));
        assert!(change.len() < full.len());
        view.process(&change);
        assert_eq!(view.screen().contents(), "first line\nsecond");
        screen.resize(TerminalSize::validated(10, 40).unwrap());
        let (resized, _) = screen.frame_since(Some(&shown));
        let mut view = vt100::Parser::new(10, 40, 0);
        view.process(&resized);
        assert_eq!(view.screen().contents(), "first line\nsecond");
    }

    fn numbered_lines(screen: &mut Screen, lines: std::ops::RangeInclusive<usize>) {
        for line in lines {
            screen.process(format!("line {line:02}\r\n").as_bytes());
        }
    }

    /// The screen's contents with these numbered lines and then `last`, if any.
    fn expected_lines(lines: std::ops::RangeInclusive<usize>, last: &str) -> String {
        lines
            .map(|line| format!("line {line:02}"))
            .chain((!last.is_empty()).then(|| last.to_owned()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn shrinking_scrolls_the_top_rows_off_so_the_newest_output_stays_on_screen() {
        let mut screen = Screen::new(TerminalSize::validated(30, 40).unwrap());
        numbered_lines(&mut screen, 1..=38);
        screen.process(b"\x1b[31mline 39\x1b[m\r\n$ ");
        let scrolled_before = screen.scrollback_rows();

        screen.resize(TerminalSize::validated(10, 40).unwrap());
        assert_eq!(
            screen.parser.screen().contents(),
            expected_lines(31..=39, "$ ")
        );
        assert_eq!(screen.parser.screen().cursor_position(), (9, 2));
        assert_eq!(
            screen.parser.screen().cell(8, 0).unwrap().fgcolor(),
            vt100::Color::Idx(1),
            "rows keep their colours as they scroll"
        );
        assert_eq!(
            screen.scrollback_rows(),
            scrolled_before + 20,
            "the rows scrolled off the top go into the scrollback"
        );

        screen.resize(TerminalSize::validated(8, 40).unwrap());
        assert_eq!(
            screen.parser.screen().contents(),
            expected_lines(33..=39, "$ ")
        );
        assert_eq!(screen.parser.screen().cursor_position(), (7, 2));
        screen.process(b"next");
        assert_eq!(screen.snapshot().lines.last().unwrap(), "$ next");
    }

    #[test]
    fn shrinking_above_the_cursor_and_growing_leave_the_rows_in_place() {
        let mut screen = Screen::new(TerminalSize::validated(30, 40).unwrap());
        numbered_lines(&mut screen, 1..=3);
        screen.resize(TerminalSize::validated(10, 40).unwrap());
        assert_eq!(screen.parser.screen().contents(), expected_lines(1..=3, ""));
        assert_eq!(screen.parser.screen().cursor_position(), (3, 0));
        screen.resize(TerminalSize::validated(30, 40).unwrap());
        assert_eq!(screen.parser.screen().contents(), expected_lines(1..=3, ""));
        assert_eq!(screen.parser.screen().cursor_position(), (3, 0));
    }

    /// A resize resets the scroll margins, as terminals do, so the whole screen scrolls rather
    /// than only the program's scroll region.
    #[test]
    fn shrinking_scrolls_the_whole_screen_whatever_scroll_region_the_program_set() {
        let mut screen = Screen::new(TerminalSize::validated(30, 40).unwrap());
        numbered_lines(&mut screen, 1..=29);
        screen.process(b"line 30\x1b[2;29r\x1b[30;8H");
        screen.resize(TerminalSize::validated(10, 40).unwrap());
        assert_eq!(
            screen.parser.screen().contents(),
            expected_lines(21..=29, "line 30")
        );
        assert_eq!(screen.parser.screen().cursor_position(), (9, 7));
    }

    /// Each screen keeps its own cursor on screen: the alternate screen a full-screen program
    /// shows, which it redraws anyway, and the normal screen it returns to.
    #[test]
    fn shrinking_keeps_the_cursor_rows_of_the_alternate_and_the_normal_screen() {
        let mut screen = Screen::new(TerminalSize::validated(30, 40).unwrap());
        numbered_lines(&mut screen, 1..=40);
        screen.process(b"\x1b[?1049h\x1b[H");
        for row in 1..=30 {
            screen.process(format!("\x1b[{row};1Hrow {row:02}").as_bytes());
        }
        screen.resize(TerminalSize::validated(10, 40).unwrap());
        assert!(screen.parser.screen().alternate_screen());
        let alternate: Vec<String> = (21..=30).map(|row| format!("row {row:02}")).collect();
        assert_eq!(screen.parser.screen().contents(), alternate.join("\n"));
        assert_eq!(screen.parser.screen().cursor_position(), (9, 6));

        screen.process(b"\x1b[?1049l");
        assert_eq!(
            screen.parser.screen().contents(),
            expected_lines(32..=40, "")
        );
        assert_eq!(screen.parser.screen().cursor_position(), (9, 0));
    }

    #[test]
    fn snapshot_bytes_are_bounded() {
        let mut screen = Screen::new(TerminalSize::validated(300, 500).unwrap());
        let wide = "x".repeat(499);
        for _ in 0..300 {
            screen.process(format!("{wide}\r\n").as_bytes());
        }
        let snapshot = screen.snapshot();
        assert!(snapshot.truncated);
        assert!(
            snapshot
                .lines
                .iter()
                .map(|line| line.len() + 1)
                .sum::<usize>()
                <= SNAPSHOT_MAX_BYTES
        );
    }
}
