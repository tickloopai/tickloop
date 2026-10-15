//! What the views of a Shell task show. A Shell runs on pipes rather than a terminal, so the host
//! draws its screen itself: a header with the task's description, full command, directory and
//! status, over a terminal model of its output that fills the rows the header leaves. The view
//! is read-only; nothing typed reaches the task.

use std::io::Write;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{pty::TerminalSize, screen::Screen};

/// The most rows the description takes, so the output keeps the screen.
const DESCRIPTION_ROWS: usize = 2;
/// The most rows the full command takes; a longer one is cut with an ellipsis.
const COMMAND_ROWS: usize = 8;
const DIRECTORY_ROWS: usize = 2;
const COMMAND_PROMPT: &str = "$ ";
const DIRECTORY_PREFIX: &str = "in ";
/// Narrower screens show the header without its prefixes.
const PREFIX_MIN_COLUMNS: usize = 8;
const RUNNING_STATUS: &str = "running";
const STATUS_SEPARATOR: &str = " · ";
const ELLIPSIS: char = '…';
const RULE: char = '─';
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RUNNING_COLOUR: &str = "\x1b[33m";
const SUCCEEDED_COLOUR: &str = "\x1b[32m";
const FAILED_COLOUR: &str = "\x1b[31m";
const RESET: &str = "\x1b[m";
const HIDE_CURSOR: &str = "\x1b[?25l";

/// How the task stands, as its header says.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Standing {
    Running,
    Ended { status: String, succeeded: bool },
}

/// One header row, already within the screen's width, and its style.
struct HeaderRow {
    text: String,
    style: &'static str,
}

pub(crate) struct ShellScreen {
    description: Option<String>,
    command: String,
    directory: String,
    standing: Standing,
    size: TerminalSize,
    header: Vec<HeaderRow>,
    output: vt100::Parser,
    /// The output's last line feed, held back until more output follows so its last line stays
    /// on screen.
    holds_line_feed: bool,
    /// The screen views are sent, drawn again once something changed.
    composed: Option<Screen>,
}

impl ShellScreen {
    pub(crate) fn new(description: Option<String>, command: String, directory: String) -> Self {
        let size = TerminalSize::default();
        let mut screen = Self {
            description: description.filter(|description| !description.trim().is_empty()),
            command,
            directory,
            standing: Standing::Running,
            size,
            header: Vec::new(),
            output: vt100::Parser::new(1, size.columns, 0),
            holds_line_feed: false,
            composed: None,
        };
        screen.redraw(size, &[]);
        screen
    }

    pub(crate) fn size(&self) -> TerminalSize {
        self.size
    }

    /// Starts over at `size` with `output`, the end of what the task wrote so far.
    pub(crate) fn redraw(&mut self, size: TerminalSize, output: &[u8]) {
        self.size = size;
        self.header = self.header_rows();
        let output_rows = size.rows - self.header.len() as u16;
        self.output = vt100::Parser::new(output_rows, size.columns, 0);
        self.holds_line_feed = false;
        self.composed = None;
        self.append(output);
    }

    /// Adds what the task wrote next.
    pub(crate) fn append(&mut self, output: &[u8]) {
        if output.is_empty() {
            return;
        }
        let mut translated = Vec::with_capacity(output.len() + 2);
        if std::mem::take(&mut self.holds_line_feed) {
            translated.extend(b"\r\n");
        }
        let output = match output.strip_suffix(b"\n") {
            Some(rest) => {
                self.holds_line_feed = true;
                rest
            }
            None => output,
        };
        // Pipe output ends its lines with a bare line feed, which a terminal moves down without
        // returning to the first column.
        for byte in output {
            if *byte == b'\n' {
                translated.push(b'\r');
            }
            translated.push(*byte);
        }
        self.output.process(&translated);
        self.composed = None;
    }

    /// Shows how the task ended, and why when a reason says more, in place of `running`.
    pub(crate) fn end(&mut self, status: String, reason: Option<&str>, succeeded: bool) {
        let status = match reason {
            Some(reason) if reason != status => format!("{status}{STATUS_SEPARATOR}{reason}"),
            _ => status,
        };
        self.standing = Standing::Ended { status, succeeded };
        self.header = self.header_rows();
        self.composed = None;
    }

    pub(crate) fn screen(&mut self) -> &Screen {
        self.composed.get_or_insert_with(|| {
            let mut bytes = HIDE_CURSOR.as_bytes().to_vec();
            for (row, header) in self.header.iter().enumerate() {
                let _ = write!(
                    bytes,
                    "\x1b[{};1H{}{}{RESET}",
                    row + 1,
                    header.style,
                    header.text
                );
            }
            for (row, line) in self
                .output
                .screen()
                .rows_formatted(0, self.size.columns)
                .enumerate()
            {
                let _ = write!(bytes, "\x1b[{};1H", self.header.len() + row + 1);
                bytes.extend(line);
                bytes.extend(RESET.as_bytes());
            }
            let mut screen = Screen::new(self.size);
            screen.process(&bytes);
            screen
        })
    }

    /// The header at this size, leaving the output at least one row. Short screens give up the
    /// rule first, then the directory, then the description, then the rows of the command; the
    /// status always stays.
    fn header_rows(&self) -> Vec<HeaderRow> {
        let columns = usize::from(self.size.columns);
        let prefix = |prefix: &'static str| {
            if columns < PREFIX_MIN_COLUMNS {
                ""
            } else {
                prefix
            }
        };
        let styled = |rows: Vec<String>, style| {
            rows.into_iter()
                .map(|text| HeaderRow { text, style })
                .collect::<Vec<_>>()
        };
        let (status, colour) = match &self.standing {
            Standing::Running => (RUNNING_STATUS, RUNNING_COLOUR),
            Standing::Ended { status, succeeded } => (
                status.as_str(),
                if *succeeded {
                    SUCCEEDED_COLOUR
                } else {
                    FAILED_COLOUR
                },
            ),
        };
        let description = styled(
            self.description
                .as_deref()
                .map(|description| wrap(description, columns, DESCRIPTION_ROWS, ""))
                .unwrap_or_default(),
            BOLD,
        );
        let command = wrap(&self.command, columns, COMMAND_ROWS, prefix(COMMAND_PROMPT));
        let directory = styled(
            wrap(
                &self.directory,
                columns,
                DIRECTORY_ROWS,
                prefix(DIRECTORY_PREFIX),
            ),
            DIM,
        );
        let status = styled(wrap(status, columns, 1, ""), colour);
        let rule = styled(vec![RULE.to_string().repeat(columns)], DIM);
        let available = usize::from(self.size.rows) - 1;
        // In the order they give way.
        let mut optional = [rule, directory, description];
        let optional_rows =
            |optional: &[Vec<HeaderRow>; 3]| optional.iter().map(Vec::len).sum::<usize>();
        for dropped in 0..optional.len() {
            if status.len() + command.len() + optional_rows(&optional) <= available {
                break;
            }
            optional[dropped].clear();
        }
        let command_rows = available.saturating_sub(status.len() + optional_rows(&optional));
        let command = if command.len() > command_rows {
            wrap(&self.command, columns, command_rows, prefix(COMMAND_PROMPT))
        } else {
            command
        };
        let [rule, directory, description] = optional;
        description
            .into_iter()
            .chain(styled(command, ""))
            .chain(directory)
            .chain(status)
            .chain(rule)
            .collect()
    }
}

/// `text` in rows of at most `columns` cells, the first after `prefix` and the rest indented as
/// far, its own line breaks kept and control characters shown as spaces. At most `rows` rows; a
/// cut last row ends with an ellipsis.
fn wrap(text: &str, columns: usize, rows: usize, prefix: &str) -> Vec<String> {
    if rows == 0 {
        return Vec::new();
    }
    let indent = " ".repeat(prefix.width());
    let mut wrapped = Vec::new();
    let mut row = prefix.to_owned();
    let mut width = prefix.width();
    for character in text.trim_end().chars() {
        if character == '\n' {
            wrapped.push(std::mem::replace(&mut row, indent.clone()));
            width = indent.len();
            continue;
        }
        let character = if character.is_control() {
            ' '
        } else {
            character
        };
        let cells = character.width().unwrap_or(0);
        if width + cells > columns && width > indent.len() {
            wrapped.push(std::mem::replace(&mut row, indent.clone()));
            width = indent.len();
        }
        row.push(character);
        width += cells;
    }
    wrapped.push(row);
    if wrapped.len() > rows {
        wrapped.truncate(rows);
        let last = wrapped.last_mut().expect("at least one row is kept");
        while last.width() + 1 > columns && last.pop().is_some() {}
        last.push(ELLIPSIS);
    }
    wrapped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size(rows: u16, columns: u16) -> TerminalSize {
        TerminalSize::validated(rows, columns).unwrap()
    }

    fn lines(screen: &mut ShellScreen) -> Vec<String> {
        screen
            .screen()
            .snapshot()
            .lines
            .into_iter()
            .map(|line| line.trim_end().to_owned())
            .collect()
    }

    #[test]
    fn the_header_names_the_task_over_its_output_and_then_how_it_ended() {
        let mut screen = ShellScreen::new(
            Some("Run the tests".into()),
            "cargo test\n  -p console".into(),
            "/work/tree".into(),
        );
        screen.redraw(size(12, 40), b"compiling\nrunning 3 tests\n");
        assert_eq!(
            lines(&mut screen),
            [
                "Run the tests",
                "$ cargo test",
                "    -p console",
                "in /work/tree",
                "running",
                &"─".repeat(40),
                "compiling",
                "running 3 tests",
            ]
        );
        screen.append(b"ok\n");
        screen.end("exit 0".into(), None, true);
        let shown = lines(&mut screen);
        assert_eq!(shown[4], "exit 0");
        assert_eq!(shown.last().unwrap(), "ok");
        screen.end("signal 15".into(), Some("cancelled"), false);
        assert_eq!(lines(&mut screen)[4], "signal 15 · cancelled");
    }

    #[test]
    fn the_output_keeps_its_newest_rows_below_the_header() {
        let mut screen = ShellScreen::new(None, "seq 100".into(), "/".into());
        let output = (1..=100)
            .map(|line| format!("{line}\n"))
            .collect::<String>();
        screen.redraw(size(8, 20), output.as_bytes());
        assert_eq!(
            lines(&mut screen),
            [
                "$ seq 100",
                "in /",
                "running",
                &"─".repeat(20),
                "97",
                "98",
                "99",
                "100"
            ]
        );
    }

    #[test]
    fn a_long_command_is_wrapped_and_cut_and_a_short_screen_keeps_the_status() {
        let command = "x".repeat(200);
        let mut screen = ShellScreen::new(Some("Build".into()), command.clone(), "/".into());
        screen.redraw(size(30, 20), b"");
        let shown = lines(&mut screen);
        let command_rows = shown
            .iter()
            .filter(|line| line.contains('x'))
            .collect::<Vec<_>>();
        assert_eq!(command_rows.len(), COMMAND_ROWS);
        assert!(command_rows.last().unwrap().ends_with(ELLIPSIS));
        assert!(
            shown
                .iter()
                .all(|line| UnicodeWidthStr::width(line.as_str()) <= 20)
        );

        screen.redraw(size(3, 20), b"out\n");
        assert_eq!(
            lines(&mut screen),
            [
                format!("$ {}{ELLIPSIS}", "x".repeat(17)).as_str(),
                "running",
                "out"
            ]
        );
        screen.redraw(size(2, 20), b"out\n");
        assert_eq!(lines(&mut screen), ["running", "out"]);
    }

    #[test]
    fn control_characters_in_the_command_are_shown_as_spaces_not_run() {
        let mut screen = ShellScreen::new(None, "printf '\x1b[2J'\tdone".into(), "/".into());
        screen.redraw(size(10, 40), b"");
        assert_eq!(lines(&mut screen)[0], "$ printf ' [2J' done");
    }
}
