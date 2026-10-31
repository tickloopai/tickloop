//! What the views of a Shell task show. A Shell runs on pipes rather than a terminal, so the host
//! draws its screen itself: a header naming the task with how it stands, over a terminal model of
//! its output that keeps its newest lines and can be scrolled back, and a last row left blank for
//! the view's own key hints. The view is read-only; nothing typed reaches the task.

use std::{io::Write, time::Instant};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{pty::TerminalSize, screen::Screen};

/// The lines of output a view can scroll back through.
pub(crate) const OUTPUT_HISTORY_ROWS: usize = 10_000;
/// The most rows the full command takes under the key command; a longer one is cut with an
/// ellipsis.
const COMMAND_ROWS: usize = 3;
/// The last row, which the view draws its key hints on.
const HINT_ROWS: u16 = 1;
/// The fewest rows that keep a hint row; a smaller screen gives it to the output.
const HINT_MIN_ROWS: u16 = 3;
/// The narrowest screen whose first row holds the status beside the key command; a narrower one
/// gives the status its own row.
const STATUS_BESIDE_COMMAND_MIN_COLUMNS: usize = 40;
const STATUS_GAP_COLUMNS: usize = 2;
const COMMAND_PROMPT: &str = "$ ";
const COMMAND_INDENT: &str = "  ";
const DIRECTORY_PREFIX: &str = "in ";
/// Narrower screens show the header without its prefixes.
const PREFIX_MIN_COLUMNS: usize = 8;
const RUNNING_STATUS: &str = "running";
const STOPPED_STATUS: &str = "stopped";
/// Why a task was stopped when a person or the Agent stopped it.
const CANCELLED_REASON: &str = "cancelled";
const STATUS_SEPARATOR: &str = " · ";
const ELLIPSIS: char = '…';
const RULE: char = '─';
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RUNNING_COLOUR: &str = "\x1b[36m";
const STOPPED_COLOUR: &str = "\x1b[33m";
const SUCCEEDED_COLOUR: &str = "\x1b[32m";
const FAILED_COLOUR: &str = "\x1b[31m";
const RESET: &str = "\x1b[m";
const HIDE_CURSOR: &str = "\x1b[?25l";
const SECONDS_PER_MINUTE: u64 = 60;
const SECONDS_PER_HOUR: u64 = 60 * 60;

/// How the task stands, as its header says.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Standing {
    Running,
    Ended {
        status: String,
        colour: &'static str,
    },
}

/// One header row, already within the screen's width, and its style.
struct HeaderRow {
    text: String,
    style: &'static str,
    /// Text at the row's right edge, in its own style: the status beside the key command.
    right: Option<(String, &'static str)>,
}

impl HeaderRow {
    fn plain(text: String, style: &'static str) -> Self {
        Self {
            text,
            style,
            right: None,
        }
    }
}

pub(crate) struct ShellScreen {
    description: Option<String>,
    command: String,
    directory: String,
    standing: Standing,
    started: Instant,
    /// The running time the header shows, in whole seconds.
    elapsed_seconds: u64,
    size: TerminalSize,
    header: Vec<HeaderRow>,
    output: vt100::Parser,
    /// The output's last line feed, held back until more output follows so its last line stays
    /// on screen.
    holds_line_feed: bool,
    /// The screen of a view that follows the newest output, drawn again once something changed.
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
            started: Instant::now(),
            elapsed_seconds: 0,
            size,
            header: Vec::new(),
            output: vt100::Parser::new(1, size.columns, OUTPUT_HISTORY_ROWS),
            holds_line_feed: false,
            composed: None,
        };
        screen.redraw(size, &[]);
        screen
    }

    pub(crate) fn size(&self) -> TerminalSize {
        self.size
    }

    /// The rows the output shows at this size.
    pub(crate) fn output_rows(&self) -> usize {
        usize::from(self.output.screen().size().0)
    }

    /// Starts over at `size` with `output`, the end of what the task wrote so far.
    pub(crate) fn redraw(&mut self, size: TerminalSize, output: &[u8]) {
        self.size = size;
        self.header = self.header_rows();
        let hint_rows = if size.rows >= HINT_MIN_ROWS {
            HINT_ROWS
        } else {
            0
        };
        let output_rows = (size.rows - hint_rows).saturating_sub(self.header.len() as u16);
        self.output = vt100::Parser::new(output_rows.max(1), size.columns, OUTPUT_HISTORY_ROWS);
        self.holds_line_feed = false;
        self.composed = None;
        self.append(output);
    }

    /// Adds what the task wrote next; returns how many lines it pushed into the history, which a
    /// view scrolled back moves by to keep showing the same lines.
    pub(crate) fn append(&mut self, output: &[u8]) -> usize {
        if output.is_empty() {
            return 0;
        }
        let history = self.history_rows();
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
        self.history_rows().saturating_sub(history)
    }

    /// The output lines above the rows shown, as far as a view can scroll back.
    pub(crate) fn history_rows(&mut self) -> usize {
        let screen = self.output.screen_mut();
        screen.set_scrollback(usize::MAX);
        let rows = screen.scrollback();
        screen.set_scrollback(0);
        rows
    }

    /// Shows how the task ended, and why when a reason says more, in place of `running`.
    pub(crate) fn end(&mut self, status: String, reason: Option<&str>, succeeded: bool) {
        self.standing = match reason {
            Some(CANCELLED_REASON) => Standing::Ended {
                status: STOPPED_STATUS.into(),
                colour: STOPPED_COLOUR,
            },
            Some(reason) if reason != status => Standing::Ended {
                status: format!("{status}{STATUS_SEPARATOR}{reason}"),
                colour: FAILED_COLOUR,
            },
            _ => Standing::Ended {
                status,
                colour: if succeeded {
                    SUCCEEDED_COLOUR
                } else {
                    FAILED_COLOUR
                },
            },
        };
        self.header = self.header_rows();
        self.composed = None;
    }

    /// Moves the running time the header shows to `now`; returns whether it changed.
    pub(crate) fn tick(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.started).as_secs();
        if self.standing != Standing::Running || elapsed == self.elapsed_seconds {
            return false;
        }
        self.elapsed_seconds = elapsed;
        self.header = self.header_rows();
        self.composed = None;
        true
    }

    /// The terminal output that brings a view showing `shown`, scrolled `back` lines from the
    /// newest output, to its screen now, with the screen it then shows.
    pub(crate) fn frame_since(
        &mut self,
        back: usize,
        shown: Option<&vt100::Screen>,
    ) -> (Vec<u8>, vt100::Screen) {
        if back > 0 {
            self.output.screen_mut().set_scrollback(back);
            let screen = self.compose();
            self.output.screen_mut().set_scrollback(0);
            return screen.frame_since(shown);
        }
        if self.composed.is_none() {
            self.composed = Some(self.compose());
        }
        self.composed
            .as_ref()
            .expect("composed above")
            .frame_since(shown)
    }

    fn compose(&self) -> Screen {
        let mut bytes = HIDE_CURSOR.as_bytes().to_vec();
        let columns = usize::from(self.size.columns);
        for (row, header) in self.header.iter().enumerate() {
            let _ = write!(
                bytes,
                "\x1b[{};1H{}{}{RESET}",
                row + 1,
                header.style,
                header.text
            );
            if let Some((right, style)) = &header.right {
                let column = columns.saturating_sub(right.width()) + 1;
                let _ = write!(bytes, "\x1b[{};{column}H{style}{right}{RESET}", row + 1);
            }
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
    }

    /// The status the header shows, in its colour.
    fn status(&self) -> (String, &'static str) {
        match &self.standing {
            Standing::Running => (
                format!("{RUNNING_STATUS} {}", elapsed(self.elapsed_seconds)),
                RUNNING_COLOUR,
            ),
            Standing::Ended { status, colour } => (status.clone(), colour),
        }
    }

    /// The header at this size, leaving the output at least one row: the key command with the
    /// status at its right, the description's first line, the full command when it says more,
    /// the directory and a rule. Short screens give up the rule first, then the directory, the
    /// description and the full command; the key command and the status always stay.
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
                .map(|text| HeaderRow::plain(text, style))
                .collect::<Vec<_>>()
        };
        let (status, colour) = self.status();
        let key_command = key_command(&self.command);
        let title = format!("{}{}", prefix(COMMAND_PROMPT), one_line(&key_command));
        let mut first = Vec::new();
        if columns >= STATUS_BESIDE_COMMAND_MIN_COLUMNS {
            let room = columns.saturating_sub(status.width() + STATUS_GAP_COLUMNS);
            first.push(HeaderRow {
                text: ellipsize(&title, room),
                style: BOLD,
                right: Some((status, colour)),
            });
        } else {
            first.push(HeaderRow::plain(ellipsize(&title, columns), BOLD));
            first.push(HeaderRow::plain(ellipsize(&status, columns), colour));
        }
        let description = styled(
            self.description
                .as_deref()
                .and_then(|description| {
                    description
                        .lines()
                        .map(str::trim)
                        .find(|line| !line.is_empty())
                })
                .map(|line| vec![ellipsize(&one_line(line), columns)])
                .unwrap_or_default(),
            "",
        );
        let full_command = if key_command.trim() == self.command.trim() {
            Vec::new()
        } else {
            wrap(&self.command, columns, COMMAND_ROWS, prefix(COMMAND_INDENT))
        };
        let directory = styled(
            wrap(&self.directory, columns, 1, prefix(DIRECTORY_PREFIX)),
            DIM,
        );
        let rule = styled(vec![RULE.to_string().repeat(columns)], DIM);
        let hint_rows = if self.size.rows >= HINT_MIN_ROWS {
            usize::from(HINT_ROWS)
        } else {
            0
        };
        let available = usize::from(self.size.rows)
            .saturating_sub(hint_rows + 1)
            .max(first.len());
        // In the order they give way.
        let mut optional = [rule, directory, description];
        let optional_rows =
            |optional: &[Vec<HeaderRow>; 3]| optional.iter().map(Vec::len).sum::<usize>();
        for dropped in 0..optional.len() {
            if first.len() + full_command.len() + optional_rows(&optional) <= available {
                break;
            }
            optional[dropped].clear();
        }
        let command_rows = available.saturating_sub(first.len() + optional_rows(&optional));
        let full_command = if full_command.len() > command_rows {
            wrap(&self.command, columns, command_rows, prefix(COMMAND_INDENT))
        } else {
            full_command
        };
        let [rule, directory, description] = optional;
        first
            .into_iter()
            .chain(description)
            .chain(styled(full_command, DIM))
            .chain(directory)
            .chain(rule)
            .collect()
    }
}

/// The command a person recognises the task by: without a leading `cd <dir> &&`, which only
/// says where it ran.
fn key_command(command: &str) -> String {
    let trimmed = command.trim();
    if let Some(rest) = trimmed.strip_prefix("cd ")
        && let Some((_, after)) = rest.split_once("&&")
        && !after.trim().is_empty()
    {
        return after.trim().to_owned();
    }
    trimmed.to_owned()
}

/// `text` on one row: line breaks and other control characters as spaces.
fn one_line(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

/// `text` cut to `columns` cells, ending in an ellipsis when cut.
fn ellipsize(text: &str, columns: usize) -> String {
    if text.width() <= columns {
        return text.to_owned();
    }
    let mut cut = String::new();
    let mut width = 0;
    for character in text.chars() {
        let cells = character.width().unwrap_or(0);
        if width + cells + 1 > columns {
            break;
        }
        cut.push(character);
        width += cells;
    }
    if columns > 0 {
        cut.push(ELLIPSIS);
    }
    cut
}

/// A running time as the header shows it: `12s`, `3m 4s`, `1h 2m`.
fn elapsed(seconds: u64) -> String {
    if seconds < SECONDS_PER_MINUTE {
        format!("{seconds}s")
    } else if seconds < SECONDS_PER_HOUR {
        format!(
            "{}m {}s",
            seconds / SECONDS_PER_MINUTE,
            seconds % SECONDS_PER_MINUTE
        )
    } else {
        format!(
            "{}h {}m",
            seconds / SECONDS_PER_HOUR,
            seconds % SECONDS_PER_HOUR / SECONDS_PER_MINUTE
        )
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

/// How a view's keys move its place in a Shell's output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Scroll {
    Lines(isize),
    Pages(isize),
    Oldest,
    /// Back to the newest output, which the view follows again.
    Newest,
}

/// The scroll keys in a view's input, in order; every other key does nothing. `↑`/`↓` and
/// `Ctrl+P`/`Ctrl+N` move a line, `PageUp`/`PageDown` a page, `Home` to the oldest line kept and
/// `End` or `G` back to the newest.
pub(crate) fn scroll_keys(input: &[u8]) -> Vec<Scroll> {
    const KEYS: &[(&[u8], Scroll)] = &[
        (b"\x1b[A", Scroll::Lines(-1)),
        (b"\x1bOA", Scroll::Lines(-1)),
        (b"\x10", Scroll::Lines(-1)),
        (b"\x1b[B", Scroll::Lines(1)),
        (b"\x1bOB", Scroll::Lines(1)),
        (b"\x0e", Scroll::Lines(1)),
        (b"\x1b[5~", Scroll::Pages(-1)),
        (b"\x1b[6~", Scroll::Pages(1)),
        (b"\x1b[H", Scroll::Oldest),
        (b"\x1bOH", Scroll::Oldest),
        (b"\x1b[1~", Scroll::Oldest),
        (b"\x1b[F", Scroll::Newest),
        (b"\x1bOF", Scroll::Newest),
        (b"\x1b[4~", Scroll::Newest),
        (b"G", Scroll::Newest),
    ];
    let mut scrolls = Vec::new();
    let mut rest = input;
    while !rest.is_empty() {
        match KEYS.iter().find(|(key, _)| rest.starts_with(key)) {
            Some((key, scroll)) => {
                scrolls.push(*scroll);
                rest = &rest[key.len()..];
            }
            None => rest = &rest[1..],
        }
    }
    scrolls
}

/// Where a view scrolled `back` lines from the newest output lands after `scroll`, with
/// `history` lines to scroll through and `page` rows shown.
pub(crate) fn scrolled(back: usize, scroll: Scroll, history: usize, page: usize) -> usize {
    let page = page.saturating_sub(1).max(1) as isize;
    let moved = match scroll {
        // Up is back in time.
        Scroll::Lines(lines) => back as isize - lines,
        Scroll::Pages(pages) => back as isize - pages * page,
        Scroll::Oldest => history as isize,
        Scroll::Newest => 0,
    };
    moved.clamp(0, history as isize) as usize
}

#[cfg(test)]
#[path = "shell_screen_tests.rs"]
mod tests;
