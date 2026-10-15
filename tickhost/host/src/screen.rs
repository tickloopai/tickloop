//! The terminal model of a hidden Tick. Output is parsed continuously so the screen stays current
//! while nobody watches; no raw output log is kept, only the screen and bounded scrollback.

use crate::pty::TerminalSize;

pub const SCROLLBACK_ROWS: usize = 1000;
pub const SNAPSHOT_MAX_LINES: usize = 200;
pub const SNAPSHOT_MAX_BYTES: usize = 64 * 1024;

pub struct Screen {
    parser: vt100::Parser,
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
            parser: vt100::Parser::new(size.rows, size.columns, SCROLLBACK_ROWS),
        }
    }

    pub fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    pub fn resize(&mut self, size: TerminalSize) {
        self.parser.screen_mut().set_size(size.rows, size.columns);
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
