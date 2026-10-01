use std::ops::Range;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use unicode_width::UnicodeWidthChar;

use super::hyperlink::Hyperlink;

const TAB_COLUMNS: usize = 4;
const DEFAULT_WIDTH: usize = 80;
const DEFAULT_BODY_HEIGHT: usize = 22;
const HELP_LINES: &[&str] = &[
    "Reading",
    "Up/Down or k/j: previous/next line",
    "Ctrl+B/Ctrl+F or b/Space: previous/next page",
    "PgUp/PgDn or Ctrl+U/Ctrl+D: previous/next half page",
    "Home/End, Ctrl+Home/Ctrl+End or g/G: first/last page",
    "End resumes following new output; scrolling back keeps your reading position.",
    "{ / }: previous/next user prompt",
    "",
    "Search (literal and case-sensitive)",
    "/: open search",
    "Enter: accept the search; press again for the next match",
    "n/N: next/previous match",
    "Esc, Ctrl+C or Ctrl+G while searching: cancel and restore the previous view",
    "Left/Right, Home/End, Backspace/Delete: edit the search",
    "Pasting fills the search without accepting it.",
    "",
    "Leaving the view",
    "Esc, q, Ctrl+C or Ctrl+O: close the transcript",
    "?: open this help",
    "Esc, q or ? in this help: return to your reading position",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TranscriptEvent {
    Handled,
    Close,
}

pub(super) struct TranscriptFrame {
    pub(super) lines: Vec<String>,
    pub(super) cursor: Option<(usize, usize)>,
    /// Linked ranges of `lines`; the text itself never carries escape sequences.
    pub(super) links: Vec<LineLink>,
}

/// A byte range of one line that opens `link`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct LineLink {
    pub(super) line: usize,
    pub(super) range: Range<usize>,
    pub(super) link: Hyperlink,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct TranscriptDocument {
    pub(super) lines: Vec<String>,
    pub(super) prompts: Vec<usize>,
    pub(super) links: Vec<LineLink>,
}

impl From<Vec<String>> for TranscriptDocument {
    fn from(lines: Vec<String>) -> Self {
        Self {
            lines,
            ..Self::default()
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
struct Position {
    line: usize,
    byte: usize,
    tab_cell: usize,
}

struct Row {
    start: Position,
    end: Position,
    text: String,
    links: Vec<(Range<usize>, Hyperlink)>,
}

struct SearchCheckpoint {
    query: String,
    selected: Option<Position>,
    anchor: Position,
}

pub(super) struct Transcript {
    lines: Vec<String>,
    prompts: Vec<usize>,
    links: Vec<LineLink>,
    rows: Vec<Row>,
    width: usize,
    body_height: usize,
    anchor: Position,
    following: bool,
    query: String,
    query_cursor: usize,
    matches: Vec<Position>,
    selected_match: Option<usize>,
    search: Option<SearchCheckpoint>,
    help_offset: Option<usize>,
}

impl Transcript {
    pub(super) fn new(document: TranscriptDocument) -> Self {
        let document = logical_document(document);
        let mut transcript = Self {
            lines: document.lines,
            prompts: document.prompts,
            links: document.links,
            rows: Vec::new(),
            width: DEFAULT_WIDTH,
            body_height: DEFAULT_BODY_HEIGHT,
            anchor: Position::default(),
            following: true,
            query: String::new(),
            query_cursor: 0,
            matches: Vec::new(),
            selected_match: None,
            search: None,
            help_offset: None,
        };
        transcript.reflow();
        transcript
    }

    pub(super) fn update(&mut self, document: TranscriptDocument) {
        let document = logical_document(document);
        self.prompts = document.prompts;
        if self.lines == document.lines && self.links == document.links {
            return;
        }
        let selected = self.selected_position();
        self.lines = document.lines;
        self.links = document.links;
        self.reflow();
        self.find_matches(selected);
    }

    pub(super) fn paste(&mut self, text: &str) {
        if self.search.is_none() {
            return;
        }
        let text = text
            .replace("\r\n", " ")
            .replace(['\r', '\n'], " ")
            .chars()
            .filter(|character| !character.is_control() || *character == '\t')
            .collect::<String>();
        self.query.insert_str(self.query_cursor, &text);
        self.query_cursor += text.len();
        self.query_changed();
    }

    pub(super) fn key(&mut self, key: KeyEvent) -> TranscriptEvent {
        if key.kind == KeyEventKind::Release {
            return TranscriptEvent::Handled;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('o' | 'O'))
        {
            return TranscriptEvent::Close;
        }
        if self.search.is_some() {
            self.search_key(key);
            return TranscriptEvent::Handled;
        }
        if self.help_offset.is_some() {
            return self.help_key(key);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c' | 'C') => return TranscriptEvent::Close,
                KeyCode::Char('u' | 'U') => self.scroll(false, (self.body_height / 2).max(1)),
                KeyCode::Char('d' | 'D') => self.scroll(true, (self.body_height / 2).max(1)),
                KeyCode::Char('b' | 'B') => self.scroll(false, self.body_height),
                KeyCode::Char('f' | 'F') => self.scroll(true, self.body_height),
                KeyCode::Home => {
                    self.anchor = Position::default();
                    self.following = false;
                }
                KeyCode::End => self.following = true,
                _ => {}
            }
            return TranscriptEvent::Handled;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return TranscriptEvent::Close,
            KeyCode::Up | KeyCode::Char('k') => self.scroll(false, 1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll(true, 1),
            KeyCode::PageUp => self.scroll(false, (self.body_height / 2).max(1)),
            KeyCode::PageDown => self.scroll(true, (self.body_height / 2).max(1)),
            KeyCode::Char('b') => self.scroll(false, self.body_height),
            KeyCode::Char(' ') => self.scroll(true, self.body_height),
            KeyCode::Home | KeyCode::Char('g') => {
                self.anchor = Position::default();
                self.following = false;
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.following = true;
            }
            KeyCode::Char('/') => {
                self.search = Some(SearchCheckpoint {
                    query: self.query.clone(),
                    selected: self.selected_position(),
                    anchor: self.visible_anchor(),
                });
                self.query.clear();
                self.query_cursor = 0;
                self.matches.clear();
                self.selected_match = None;
                self.anchor = self.visible_anchor();
                self.following = false;
            }
            KeyCode::Enter | KeyCode::Char('n') => self.next_match(true),
            KeyCode::Char('N') => self.next_match(false),
            KeyCode::Char('{') => self.prompt(false),
            KeyCode::Char('}') => self.prompt(true),
            KeyCode::Char('?') => {
                self.anchor = self.visible_anchor();
                self.following = false;
                self.help_offset = Some(0);
            }
            _ => {}
        }
        TranscriptEvent::Handled
    }

    pub(super) fn frame(&mut self, width: usize, height: usize) -> TranscriptFrame {
        if height == 0 || width == 0 {
            return TranscriptFrame {
                lines: Vec::new(),
                cursor: None,
                links: Vec::new(),
            };
        }
        if self.width != width {
            self.width = width;
            self.reflow();
        }
        self.body_height = height.saturating_sub(2).max(1);
        if self.help_offset.is_some() {
            return self.help_frame(width, height);
        }
        let top = self.top_row();
        let visible = self
            .rows
            .len()
            .saturating_sub(top)
            .min(height.saturating_sub(2));
        let search_status = if self.query.is_empty() {
            String::new()
        } else {
            format!(
                " · {}/{} matches",
                self.selected_match.map_or(0, |index| index + 1),
                self.matches.len(),
            )
        };
        let mut lines = vec![truncate(
            &format!(
                "Transcript{search_status} · {}–{}/{} · {}",
                if visible == 0 { 0 } else { top + 1 },
                if visible == 0 { 0 } else { top + visible },
                self.rows.len(),
                if self.following {
                    "following"
                } else {
                    "browsing"
                },
            ),
            width,
        )];
        if height == 1 {
            return TranscriptFrame {
                lines,
                cursor: None,
                links: Vec::new(),
            };
        }
        let selected = self.selected_position();
        let mut links = Vec::new();
        for row in self.rows.iter().skip(top).take(height.saturating_sub(2)) {
            let marker = if width < 3 {
                ""
            } else if selected.is_some_and(|position| row.start <= position && position < row.end) {
                "› "
            } else {
                "  "
            };
            links.extend(row.links.iter().map(|(range, link)| LineLink {
                line: lines.len(),
                range: range.start + marker.len()..range.end + marker.len(),
                link: link.clone(),
            }));
            lines.push(format!("{marker}{}", row.text));
        }
        while lines.len() < height - 1 {
            lines.push(String::new());
        }
        let cursor = if self.search.is_some() {
            let (input, column) = search_input(&self.query, self.query_cursor, width);
            lines.push(input);
            Some((column, height - 1))
        } else {
            let footer = if self.query.is_empty() {
                "↑↓/jk scroll · / find · ? help · Esc/q close".to_owned()
            } else {
                format!(
                    "Search: {} · n/N next/previous · / find · Esc/q close",
                    expand_tabs(&self.query)
                )
            };
            lines.push(truncate(&footer, width));
            None
        };
        TranscriptFrame {
            lines,
            cursor,
            links,
        }
    }

    fn search_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc
            || key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('c' | 'C' | 'g' | 'G'))
        {
            let checkpoint = self.search.take().unwrap();
            self.query = checkpoint.query;
            self.query_cursor = self.query.len();
            self.anchor = checkpoint.anchor;
            self.following = false;
            self.find_matches(checkpoint.selected);
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('u' | 'U') => {
                    self.query.clear();
                    self.query_cursor = 0;
                    self.query_changed();
                }
                KeyCode::Char('a' | 'A') => self.query_cursor = 0,
                KeyCode::Char('e' | 'E') => self.query_cursor = self.query.len(),
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Enter => {
                self.search = None;
            }
            KeyCode::Left => self.query_cursor = previous_boundary(&self.query, self.query_cursor),
            KeyCode::Right => self.query_cursor = next_boundary(&self.query, self.query_cursor),
            KeyCode::Home => self.query_cursor = 0,
            KeyCode::End => self.query_cursor = self.query.len(),
            KeyCode::Backspace => {
                let previous = previous_boundary(&self.query, self.query_cursor);
                self.query.drain(previous..self.query_cursor);
                self.query_cursor = previous;
                self.query_changed();
            }
            KeyCode::Delete => {
                self.query
                    .drain(self.query_cursor..next_boundary(&self.query, self.query_cursor));
                self.query_changed();
            }
            KeyCode::Char(character)
                if !key.modifiers.contains(KeyModifiers::ALT) && !character.is_control() =>
            {
                self.query.insert(self.query_cursor, character);
                self.query_cursor += character.len_utf8();
                self.query_changed();
            }
            _ => {}
        }
    }

    fn help_key(&mut self, key: KeyEvent) -> TranscriptEvent {
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('q' | '?')) {
            self.help_offset = None;
            return TranscriptEvent::Handled;
        }
        let maximum = self.help_rows().len().saturating_sub(self.body_height);
        let offset = self.help_offset.as_mut().unwrap();
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c' | 'C') => return TranscriptEvent::Close,
                KeyCode::Char('u' | 'U') => {
                    *offset = offset.saturating_sub((self.body_height / 2).max(1))
                }
                KeyCode::Char('d' | 'D') => {
                    *offset = offset
                        .saturating_add((self.body_height / 2).max(1))
                        .min(maximum)
                }
                KeyCode::Char('b' | 'B') => *offset = offset.saturating_sub(self.body_height),
                KeyCode::Char('f' | 'F') => {
                    *offset = offset.saturating_add(self.body_height).min(maximum)
                }
                KeyCode::Home => *offset = 0,
                KeyCode::End => *offset = maximum,
                _ => {}
            }
        } else {
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => *offset = offset.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => {
                    *offset = offset.saturating_add(1).min(maximum)
                }
                KeyCode::PageUp => *offset = offset.saturating_sub((self.body_height / 2).max(1)),
                KeyCode::PageDown => {
                    *offset = offset
                        .saturating_add((self.body_height / 2).max(1))
                        .min(maximum)
                }
                KeyCode::Char('b') => *offset = offset.saturating_sub(self.body_height),
                KeyCode::Char(' ') => {
                    *offset = offset.saturating_add(self.body_height).min(maximum)
                }
                KeyCode::Home | KeyCode::Char('g') => *offset = 0,
                KeyCode::End | KeyCode::Char('G') => *offset = maximum,
                _ => {}
            }
        }
        TranscriptEvent::Handled
    }

    fn help_frame(&mut self, width: usize, height: usize) -> TranscriptFrame {
        let rows = self.help_rows();
        let offset = self.help_offset.as_mut().unwrap();
        *offset = (*offset).min(rows.len().saturating_sub(self.body_height));
        let visible = rows
            .len()
            .saturating_sub(*offset)
            .min(height.saturating_sub(2));
        let mut lines = vec![truncate(
            &format!(
                "Transcript help · {}–{}/{}",
                if visible == 0 { 0 } else { *offset + 1 },
                if visible == 0 { 0 } else { *offset + visible },
                rows.len(),
            ),
            width,
        )];
        if height == 1 {
            return TranscriptFrame {
                lines,
                cursor: None,
                links: Vec::new(),
            };
        }
        for row in rows.iter().skip(*offset).take(height.saturating_sub(2)) {
            lines.push(if width >= 3 {
                format!("  {}", row.text)
            } else {
                row.text.clone()
            });
        }
        while lines.len() < height - 1 {
            lines.push(String::new());
        }
        lines.push(truncate(
            "↑↓/PgUp/PgDn · Esc/q/? return · Ctrl+O close",
            width,
        ));
        TranscriptFrame {
            lines,
            cursor: None,
            links: Vec::new(),
        }
    }

    fn help_rows(&self) -> Vec<Row> {
        wrap_lines(
            &HELP_LINES
                .iter()
                .map(|line| (*line).to_owned())
                .collect::<Vec<_>>(),
            &[],
            self.width
                .saturating_sub(if self.width >= 3 { 2 } else { 0 })
                .max(1),
        )
    }

    fn query_changed(&mut self) {
        self.find_matches(None);
        let origin = self.search.as_ref().unwrap().anchor;
        if let Some(index) = self
            .matches
            .iter()
            .position(|position| *position >= origin)
            .or_else(|| (!self.matches.is_empty()).then_some(0))
        {
            self.selected_match = Some(index);
            self.anchor = self.matches[index];
        } else {
            self.anchor = origin;
        }
        self.following = false;
    }

    fn find_matches(&mut self, selected: Option<Position>) {
        self.matches.clear();
        if !self.query.is_empty() {
            for (line, text) in self.lines.iter().enumerate() {
                self.matches
                    .extend(text.match_indices(&self.query).map(|(byte, _)| Position {
                        line,
                        byte,
                        tab_cell: 0,
                    }));
            }
        }
        self.selected_match =
            selected.and_then(|position| self.matches.binary_search(&position).ok());
    }

    fn next_match(&mut self, forward: bool) {
        if self.matches.is_empty() {
            return;
        }
        let index = match self.selected_match {
            Some(index) if forward => (index + 1) % self.matches.len(),
            Some(index) => (index + self.matches.len() - 1) % self.matches.len(),
            None if forward => self
                .matches
                .iter()
                .position(|position| *position >= self.visible_anchor())
                .unwrap_or(0),
            None => self
                .matches
                .iter()
                .rposition(|position| *position < self.visible_anchor())
                .unwrap_or(self.matches.len() - 1),
        };
        self.selected_match = Some(index);
        self.anchor = self.matches[index];
        self.following = false;
    }

    fn selected_position(&self) -> Option<Position> {
        self.selected_match
            .and_then(|index| self.matches.get(index).copied())
    }

    fn scroll(&mut self, forward: bool, amount: usize) {
        let top = self.top_row();
        let maximum = self.rows.len().saturating_sub(self.body_height);
        let target = if forward {
            top.saturating_add(amount).min(maximum)
        } else {
            top.saturating_sub(amount)
        };
        self.anchor = self
            .rows
            .get(target)
            .map_or(Position::default(), |row| row.start);
        self.following = forward && target == maximum;
    }

    fn prompt(&mut self, forward: bool) {
        let origin = self.visible_anchor();
        let mut prompts = self.prompts.iter().copied().map(|line| Position {
            line,
            byte: 0,
            tab_cell: 0,
        });
        let target = if forward {
            prompts.find(|position| *position > origin)
        } else {
            prompts.rev().find(|position| *position < origin)
        };
        if let Some(target) = target {
            self.anchor = target;
            self.following = false;
        }
    }

    fn visible_anchor(&self) -> Position {
        self.rows
            .get(self.top_row())
            .map_or(Position::default(), |row| row.start)
    }

    fn top_row(&self) -> usize {
        let maximum = self.rows.len().saturating_sub(self.body_height);
        if self.following {
            return maximum;
        }
        self.rows
            .partition_point(|row| row.start <= self.anchor)
            .saturating_sub(1)
            .min(maximum)
    }

    fn reflow(&mut self) {
        let content_width = self
            .width
            .saturating_sub(if self.width >= 3 { 2 } else { 0 })
            .max(1);
        self.rows = wrap_lines(&self.lines, &self.links, content_width);
    }
}

/// Splits lines at newlines and shows control characters as `�`; links follow their text.
fn logical_document(mut document: TranscriptDocument) -> TranscriptDocument {
    document.prompts.sort_unstable();
    document.prompts.dedup();
    let mut output = TranscriptDocument::default();
    for (index, line) in document.lines.into_iter().enumerate() {
        if document.prompts.binary_search(&index).is_ok() {
            output.prompts.push(output.lines.len());
        }
        let mut piece_start = 0;
        for piece in line.split('\n') {
            let piece_end = piece_start + piece.len();
            let sanitized_offset = |offset: usize| {
                piece
                    .get(..offset.checked_sub(piece_start)?)
                    .map(|prefix| sanitize(prefix).len())
            };
            let piece_links = document.links.iter().filter_map(|link| {
                if link.line != index
                    || link.range.start < piece_start
                    || link.range.end > piece_end
                {
                    return None;
                }
                Some(LineLink {
                    line: output.lines.len(),
                    range: sanitized_offset(link.range.start)?..sanitized_offset(link.range.end)?,
                    link: link.link.clone(),
                })
            });
            output.links.extend(piece_links);
            output.lines.push(sanitize(piece));
            piece_start = piece_end + 1;
        }
    }
    output
}

fn sanitize(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_control() && character != '\t' {
                '�'
            } else {
                character
            }
        })
        .collect()
}

fn wrap_lines(lines: &[String], links: &[LineLink], width: usize) -> Vec<Row> {
    let mut rows = Vec::new();
    for (line, text) in lines.iter().enumerate() {
        let mut start = Position {
            line,
            byte: 0,
            tab_cell: 0,
        };
        let mut end = start;
        let mut output = String::new();
        let mut row_links: Vec<(Range<usize>, Hyperlink)> = Vec::new();
        let mut columns = 0;
        let mut logical_column = 0;
        let line_links = links
            .iter()
            .filter(|link| link.line == line)
            .collect::<Vec<_>>();
        for (byte, character) in text.char_indices() {
            let link = line_links
                .iter()
                .find(|link| link.range.contains(&byte))
                .map(|link| &link.link);
            let tab_cells = if character == '\t' {
                TAB_COLUMNS - logical_column % TAB_COLUMNS
            } else {
                1
            };
            for cell in 0..tab_cells {
                let position = Position {
                    line,
                    byte,
                    tab_cell: cell,
                };
                let displayed = if character == '\t' { ' ' } else { character };
                let displayed = if displayed.width().unwrap_or(0) > width {
                    '�'
                } else {
                    displayed
                };
                let character_width = displayed.width().unwrap_or(0);
                if columns > 0 && columns + character_width > width {
                    rows.push(Row {
                        start,
                        end: position,
                        text: std::mem::take(&mut output),
                        links: std::mem::take(&mut row_links),
                    });
                    start = position;
                    columns = 0;
                }
                let displayed_start = output.len();
                output.push(displayed);
                if let Some(link) = link {
                    match row_links.last_mut() {
                        Some((range, last)) if range.end == displayed_start && last == link => {
                            range.end = output.len();
                        }
                        _ => row_links.push((displayed_start..output.len(), link.clone())),
                    }
                }
                columns += character_width;
                logical_column += if character == '\t' {
                    1
                } else {
                    character.width().unwrap_or(0)
                };
                end = if character == '\t' && cell + 1 < tab_cells {
                    Position {
                        line,
                        byte,
                        tab_cell: cell + 1,
                    }
                } else {
                    Position {
                        line,
                        byte: byte + character.len_utf8(),
                        tab_cell: 0,
                    }
                };
            }
        }
        rows.push(Row {
            start,
            end,
            text: output,
            links: row_links,
        });
    }
    rows
}

fn expand_tabs(text: &str) -> String {
    let mut output = String::new();
    let mut column = 0;
    for character in text.chars() {
        if character == '\t' {
            let spaces = TAB_COLUMNS - column % TAB_COLUMNS;
            output.extend(std::iter::repeat_n(' ', spaces));
            column += spaces;
        } else {
            output.push(character);
            column += character.width().unwrap_or(0);
        }
    }
    output
}

fn truncate(text: &str, width: usize) -> String {
    let mut output = String::new();
    let mut columns = 0;
    for character in text.chars() {
        let character_width = character.width().unwrap_or(0);
        if columns + character_width > width {
            break;
        }
        output.push(character);
        columns += character_width;
    }
    output
}

fn search_input(query: &str, cursor: usize, width: usize) -> (String, usize) {
    let prefix = if width >= 3 { "/ " } else { "/" };
    let available = width.saturating_sub(prefix.len());
    let display = expand_tabs(query);
    let cursor_column: usize = expand_tabs(&query[..cursor])
        .chars()
        .map(|character| character.width().unwrap_or(0))
        .sum();
    let desired_start = cursor_column.saturating_add(1).saturating_sub(available);
    let mut start_column = 0;
    let mut start_byte = 0;
    for (byte, character) in display.char_indices() {
        if start_column >= desired_start {
            start_byte = byte;
            break;
        }
        start_column += character.width().unwrap_or(0);
        start_byte = byte + character.len_utf8();
    }
    (
        truncate(
            &format!("{prefix}{}", truncate(&display[start_byte..], available)),
            width,
        ),
        (prefix.len() + cursor_column.saturating_sub(start_column)).min(width.saturating_sub(1)),
    )
}

fn previous_boundary(text: &str, cursor: usize) -> usize {
    text[..cursor]
        .char_indices()
        .next_back()
        .map_or(0, |(index, _)| index)
}

fn next_boundary(text: &str, cursor: usize) -> usize {
    text[cursor..]
        .chars()
        .next()
        .map_or(cursor, |character| cursor + character.len_utf8())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(transcript: &mut Transcript, code: KeyCode) -> TranscriptEvent {
        transcript.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn body(frame: &TranscriptFrame) -> String {
        frame.lines[1..frame.lines.len() - 1].join("\n")
    }

    #[test]
    fn wide_characters_and_tabs_wrap_without_emitting_controls() {
        let mut transcript = Transcript::new(vec!["You: 中\t文\u{1b}[31m".into()].into());
        press(&mut transcript, KeyCode::Home);
        let frame = transcript.frame(8, 12);
        assert!(body(&frame).contains('中'));
        assert!(body(&frame).contains('文'));
        assert_eq!(transcript.lines[0], "You: 中\t文�[31m");
        for line in frame.lines {
            assert!(!line.chars().any(char::is_control));
            assert!(
                line.chars()
                    .map(|character| character.width().unwrap_or(0))
                    .sum::<usize>()
                    <= 8
            );
        }
    }

    #[test]
    fn resize_restores_the_same_logical_byte_anchor() {
        let mut transcript =
            Transcript::new(vec!["abcdefghijklmnopqrstuvwx".into(), "tail".into()].into());
        transcript.frame(8, 4);
        press(&mut transcript, KeyCode::Home);
        press(&mut transcript, KeyCode::Down);
        let original = transcript.frame(8, 4);
        assert!(body(&original).starts_with("  ghijkl"));
        let narrow = transcript.frame(6, 4);
        assert!(body(&narrow).starts_with("  efgh"));
        let restored = transcript.frame(8, 4);
        assert_eq!(body(&restored), body(&original));
    }

    #[test]
    fn updates_follow_the_bottom_only_until_the_user_scrolls() {
        let lines = (0..20)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>();
        let mut transcript = Transcript::new(lines.clone().into());
        assert!(body(&transcript.frame(40, 6)).contains("line 19"));
        press(&mut transcript, KeyCode::Home);
        let before = body(&transcript.frame(40, 6));
        let mut appended = lines;
        appended.push("line 20".into());
        transcript.update(appended.clone().into());
        assert_eq!(body(&transcript.frame(40, 6)), before);
        press(&mut transcript, KeyCode::End);
        assert!(body(&transcript.frame(40, 6)).contains("line 20"));
        appended.push("line 21".into());
        transcript.update(appended.into());
        assert!(body(&transcript.frame(40, 6)).contains("line 21"));
    }

    #[test]
    fn chinese_search_navigates_occurrences_and_cancel_restores_the_view() {
        let mut transcript = Transcript::new(
            (0..20)
                .map(|index| format!("row {index} 中文"))
                .collect::<Vec<_>>()
                .into(),
        );
        transcript.frame(50, 6);
        press(&mut transcript, KeyCode::Home);
        let before = body(&transcript.frame(50, 6));
        press(&mut transcript, KeyCode::Char('/'));
        transcript.paste("中文");
        let editing = transcript.frame(50, 6);
        assert!(editing.lines[0].contains("1/20 matches"));
        assert!(editing.cursor.is_some());
        press(&mut transcript, KeyCode::Enter);
        press(&mut transcript, KeyCode::Char('n'));
        assert!(transcript.frame(50, 6).lines[0].contains("2/20 matches"));
        press(&mut transcript, KeyCode::Char('N'));
        assert!(transcript.frame(50, 6).lines[0].contains("1/20 matches"));
        press(&mut transcript, KeyCode::Home);
        press(&mut transcript, KeyCode::Char('/'));
        transcript.paste("row 15");
        assert!(body(&transcript.frame(50, 6)).contains("row 15"));
        assert_eq!(
            press(&mut transcript, KeyCode::Esc),
            TranscriptEvent::Handled
        );
        assert_eq!(body(&transcript.frame(50, 6)).replace("› ", "  "), before);
        assert_eq!(press(&mut transcript, KeyCode::Esc), TranscriptEvent::Close);
    }

    #[test]
    fn pasted_keys_cannot_navigate_close_or_accept_a_search() {
        let mut transcript = Transcript::new(vec!["You: q g 中文 n N".into()].into());
        transcript.frame(30, 5);
        transcript.paste("q");
        assert!(transcript.search.is_none());
        assert_eq!(
            transcript.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            TranscriptEvent::Close,
        );
        press(&mut transcript, KeyCode::Char('/'));
        transcript.paste("q\ng 中文 n N");
        assert_eq!(transcript.query, "q g 中文 n N");
        assert!(transcript.search.is_some());
        let frame = transcript.frame(12, 5);
        let (column, row) = frame.cursor.unwrap();
        assert!(column < 12);
        assert_eq!(row, 4);
        assert_eq!(
            transcript.key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL)),
            TranscriptEvent::Handled
        );
        assert!(transcript.search.is_none());
    }

    #[test]
    fn cancel_search_restores_the_old_bottom_when_new_output_arrives() {
        let mut lines = (0..20)
            .map(|index| format!("row {index}"))
            .collect::<Vec<_>>();
        let mut transcript = Transcript::new(lines.clone().into());
        let before = body(&transcript.frame(50, 6));
        press(&mut transcript, KeyCode::Char('/'));
        transcript.paste("row 1");
        lines.push("new output".into());
        transcript.update(lines.into());
        press(&mut transcript, KeyCode::Esc);
        assert_eq!(body(&transcript.frame(50, 6)), before);
        press(&mut transcript, KeyCode::End);
        assert!(body(&transcript.frame(50, 6)).contains("new output"));
    }

    #[test]
    fn official_paging_keys_and_prompt_jumps_share_the_scroll_state() {
        let lines = (0..40)
            .map(|index| {
                if index % 10 == 0 {
                    format!("You: prompt {index}")
                } else {
                    format!("Assistant: row {index}")
                }
            })
            .collect();
        let mut transcript = Transcript::new(TranscriptDocument {
            lines,
            prompts: vec![0, 10, 20, 30],
            ..TranscriptDocument::default()
        });
        transcript.frame(50, 8);
        press(&mut transcript, KeyCode::Char('g'));
        press(&mut transcript, KeyCode::Char('}'));
        assert!(body(&transcript.frame(50, 8)).starts_with("  You: prompt 10"));
        transcript.key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
        assert!(body(&transcript.frame(50, 8)).starts_with("  Assistant: row 16"));
        press(&mut transcript, KeyCode::Char('{'));
        assert!(body(&transcript.frame(50, 8)).starts_with("  You: prompt 10"));
        transcript.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert!(body(&transcript.frame(50, 8)).starts_with("  Assistant: row 7"));
        press(&mut transcript, KeyCode::PageDown);
        assert!(body(&transcript.frame(50, 8)).starts_with("  You: prompt 10"));
        press(&mut transcript, KeyCode::PageUp);
        assert!(body(&transcript.frame(50, 8)).starts_with("  Assistant: row 7"));
        transcript.key(KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL));
        assert!(body(&transcript.frame(50, 8)).starts_with("  You: prompt 0"));
        transcript.key(KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL));
        assert!(body(&transcript.frame(50, 8)).contains("Assistant: row 39"));
        assert!(transcript.search.is_none());
    }

    #[test]
    fn links_follow_their_text_through_newlines_controls_and_wrapping() {
        let link = Hyperlink::file(std::path::Path::new("/cache/1.png")).unwrap();
        let document = TranscriptDocument {
            lines: vec!["intro\nYou: \u{1b}see [Image #1] now".into()],
            links: vec![LineLink {
                line: 0,
                range: 16..26,
                link: link.clone(),
            }],
            ..TranscriptDocument::default()
        };
        assert_eq!(&document.lines[0][16..26], "[Image #1]");
        let mut transcript = Transcript::new(document);
        let linked = |frame: &TranscriptFrame| {
            frame
                .links
                .iter()
                .map(|line_link| {
                    assert_eq!(line_link.link, link);
                    frame.lines[line_link.line][line_link.range.clone()].to_owned()
                })
                .collect::<Vec<_>>()
        };

        assert_eq!(linked(&transcript.frame(40, 6)), ["[Image #1]"]);
        assert_eq!(linked(&transcript.frame(15, 8)), ["[Im", "age #1]"]);
        assert!(
            transcript
                .frame(15, 8)
                .lines
                .iter()
                .all(|line| !line.contains('\u{1b}'))
        );
    }

    #[test]
    fn prompt_navigation_uses_metadata_and_normalizes_multiline_input() {
        let document = TranscriptDocument {
            lines: vec![
                "You: first\ncontinuation".into(),
                "You: untrusted tool output".into(),
                "You: second".into(),
                "tail 1\ntail 2\ntail 3".into(),
            ],
            prompts: vec![2, 0, 2, 99],
            ..TranscriptDocument::default()
        };
        let mut transcript = Transcript::new(document);
        transcript.frame(60, 4);
        press(&mut transcript, KeyCode::Home);
        press(&mut transcript, KeyCode::Char('}'));
        assert!(body(&transcript.frame(60, 4)).starts_with("  You: second"));
        press(&mut transcript, KeyCode::Char('{'));
        assert!(body(&transcript.frame(60, 4)).starts_with("  You: first"));
        transcript.update(TranscriptDocument {
            lines: transcript.lines.clone(),
            prompts: vec![0],
            ..TranscriptDocument::default()
        });
        press(&mut transcript, KeyCode::Char('}'));
        assert!(body(&transcript.frame(60, 4)).starts_with("  You: first"));
    }

    #[test]
    fn help_pages_without_moving_the_transcript_and_search_keeps_question_marks() {
        let mut lines = (0..30)
            .map(|index| format!("row {index} 中文"))
            .collect::<Vec<_>>();
        let mut transcript = Transcript::new(lines.clone().into());
        transcript.frame(70, 8);
        press(&mut transcript, KeyCode::Home);
        press(&mut transcript, KeyCode::Down);
        let before = body(&transcript.frame(70, 8));
        press(&mut transcript, KeyCode::Char('?'));
        let help = transcript.frame(70, 8);
        assert!(help.lines[0].starts_with("Transcript help"));
        assert!(body(&help).contains("Ctrl+B/Ctrl+F"));
        press(&mut transcript, KeyCode::End);
        assert!(body(&transcript.frame(70, 8)).contains("Leaving the view"));
        lines.push("new output".into());
        transcript.update(lines.into());
        transcript.frame(25, 8);
        assert_eq!(
            press(&mut transcript, KeyCode::Esc),
            TranscriptEvent::Handled
        );
        assert_eq!(body(&transcript.frame(70, 8)), before);
        for close in [KeyCode::Char('q'), KeyCode::Char('?')] {
            press(&mut transcript, KeyCode::Char('?'));
            assert_eq!(press(&mut transcript, close), TranscriptEvent::Handled);
            assert_eq!(body(&transcript.frame(70, 8)), before);
        }
        press(&mut transcript, KeyCode::Char('/'));
        press(&mut transcript, KeyCode::Char('?'));
        assert_eq!(transcript.query, "?");
        assert!(transcript.help_offset.is_none());
        press(&mut transcript, KeyCode::Esc);
        press(&mut transcript, KeyCode::Char('?'));
        assert_eq!(
            transcript.key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL)),
            TranscriptEvent::Close,
        );
    }
}
