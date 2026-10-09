//! The detailed transcript Ctrl+O opens, after Claude Code's fullscreen transcript mode: the
//! conversation laid out by the conversation renderer with every block expanded, read with
//! less-style keys and searched from the footer. Its content is fixed when it opens.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::{
    display::DisplayProfile,
    hyperlink::Hyperlink,
    scrollback::Scrollback,
    style::{Role, RoleLine, Span, Style, StyledLine},
};

/// A blank row, the dim rule and the hint or search row.
const FOOTER_ROWS: usize = 3;
const FOOTER_INDENT: &str = "  ";
const FOOTER_TITLE: &str = "Showing detailed transcript · ctrl+o to toggle";
const SCROLL_HINT: &str = "↑↓ scroll · home/end top/bottom";
const SEARCH_HINT: &str = "n/N to navigate";
const NO_MATCHES: &str = "no matches ";
const BADGE_MARGIN: &str = "  ";
const WHEEL_LINES: usize = 3;
const DEFAULT_WIDTH: usize = 80;
const DEFAULT_BODY_HEIGHT: usize = 20;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TranscriptEvent {
    Handled,
    Close,
}

pub(super) struct TranscriptFrame {
    pub(super) lines: Vec<StyledLine>,
    /// The visible text of `lines`, without escape sequences.
    pub(super) texts: Vec<String>,
}

/// One piece of the detailed transcript, written in order into a conversation layout.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum TranscriptPiece {
    /// Starts a block one blank row below the content above it.
    Block,
    /// Text whose lines wrap under column `hang`, painted with `fill` across each row it reaches.
    Text {
        text: String,
        role: Option<Role>,
        fill: Option<Role>,
        hang: usize,
        link: Option<Hyperlink>,
    },
    /// An assistant reply laid out as Markdown behind `lead`.
    Markdown { text: String, lead: String },
    /// A row whose text ends `right_margin` columns before the right edge, as Claude Code places
    /// a message's time and model.
    RightAligned {
        spans: Vec<(String, Option<Role>)>,
        right_margin: usize,
    },
}

impl TranscriptPiece {
    pub(super) fn text(text: impl Into<String>, role: Option<Role>) -> Self {
        Self::Text {
            text: text.into(),
            role,
            fill: None,
            hang: 0,
            link: None,
        }
    }

    pub(super) fn hanging(text: impl Into<String>, role: Option<Role>, hang: usize) -> Self {
        Self::Text {
            text: text.into(),
            role,
            fill: None,
            hang,
            link: None,
        }
    }

    fn append_to(&self, scrollback: &mut Scrollback, width: usize) {
        match self {
            Self::Block => scrollback.start_block(),
            Self::Text {
                text,
                role,
                fill,
                hang,
                link,
            } => scrollback.append_hanging(text, *role, *fill, link.as_ref(), *hang),
            Self::Markdown { text, lead } => {
                scrollback.begin_markdown(lead);
                scrollback.append_markdown(text, lead, false);
            }
            Self::RightAligned {
                spans,
                right_margin,
            } => {
                let columns =
                    spans.iter().map(|(text, _)| text.width()).sum::<usize>() + right_margin;
                scrollback.append(&" ".repeat(width.saturating_sub(columns)));
                for (text, role) in spans {
                    scrollback.append_as(text, *role, None);
                }
                scrollback.append(&format!("{}\n", " ".repeat(*right_margin)));
            }
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct TranscriptDocument {
    pub(super) pieces: Vec<TranscriptPiece>,
}

impl TranscriptDocument {
    pub(super) fn push(&mut self, piece: TranscriptPiece) {
        self.pieces.push(piece);
    }

    /// Appends dim lines after the conversation, such as connection details of this View.
    pub(super) fn push_notice(&mut self, lines: impl IntoIterator<Item = String>) {
        self.push(TranscriptPiece::Block);
        let mut text = String::new();
        for line in lines {
            text.push_str(&line);
            text.push('\n');
        }
        self.push(TranscriptPiece::text(text, Some(Role::MutedText)));
    }
}

#[cfg(test)]
impl TranscriptDocument {
    /// The rows laid out at `width`, without trailing spaces.
    pub(super) fn plain_text(&self, width: usize) -> String {
        self.rows(width)
            .iter()
            .map(|row| row.text().trim_end().to_owned())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Each linked run of the rows laid out at `width`, with its link.
    pub(super) fn links(&self, width: usize) -> Vec<(String, Hyperlink)> {
        self.rows(width)
            .iter()
            .flat_map(|row| row.spans.iter())
            .filter_map(|span| Some((span.text.clone(), span.link.clone()?)))
            .collect()
    }

    pub(super) fn rows(&self, width: usize) -> Vec<RoleLine> {
        let mut transcript = Transcript::new(self.clone(), DisplayProfile::default());
        transcript.width = width;
        transcript.layout();
        transcript.rows
    }
}

impl From<Vec<String>> for TranscriptDocument {
    fn from(lines: Vec<String>) -> Self {
        let mut document = Self::default();
        document.push_notice(lines);
        document
    }
}

/// One occurrence of the search query: a row and its display columns.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Match {
    row: usize,
    columns: (usize, usize),
}

/// What `/` interrupts, restored when the search is cancelled.
struct SearchCheckpoint {
    query: String,
    top: usize,
    at_bottom: bool,
}

pub(super) struct Transcript {
    document: TranscriptDocument,
    display: DisplayProfile,
    rows: Vec<RoleLine>,
    texts: Vec<String>,
    width: usize,
    body_height: usize,
    top: usize,
    at_bottom: bool,
    query: String,
    query_cursor: usize,
    matches: Vec<Match>,
    selected_match: Option<usize>,
    /// Set while the search bar is open.
    search: Option<SearchCheckpoint>,
    /// Something outside the transcript that needs the user, shown on the hint row.
    notice: Option<&'static str>,
}

impl Transcript {
    pub(super) fn new(document: TranscriptDocument, display: DisplayProfile) -> Self {
        let mut transcript = Self {
            document,
            display,
            rows: Vec::new(),
            texts: Vec::new(),
            width: DEFAULT_WIDTH,
            body_height: DEFAULT_BODY_HEIGHT,
            top: 0,
            at_bottom: true,
            query: String::new(),
            query_cursor: 0,
            matches: Vec::new(),
            selected_match: None,
            search: None,
            notice: None,
        };
        transcript.layout();
        transcript
    }

    pub(super) fn set_notice(&mut self, notice: Option<&'static str>) {
        self.notice = notice;
    }

    /// Replaces the content, as when the conversation has loaded; the view starts at its end.
    pub(super) fn update(&mut self, document: TranscriptDocument) {
        if self.document == document {
            return;
        }
        self.document = document;
        self.layout();
        self.at_bottom = true;
        self.clear_search();
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

    pub(super) fn wheel(&mut self, down: bool) {
        self.scroll(down, WHEEL_LINES);
    }

    pub(super) fn key(&mut self, key: KeyEvent) -> TranscriptEvent {
        if key.kind == KeyEventKind::Release {
            return TranscriptEvent::Handled;
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        // Ctrl+O is Claude Code's global toggle, so it leaves even from the search bar.
        if control && matches!(key.code, KeyCode::Char('o' | 'O')) {
            return TranscriptEvent::Close;
        }
        if self.search.is_some() {
            self.search_key(key);
            return TranscriptEvent::Handled;
        }
        let half_page = (self.body_height / 2).max(1);
        if control {
            match key.code {
                KeyCode::Char('c' | 'C') => return TranscriptEvent::Close,
                KeyCode::Char('n' | 'N') => self.scroll(true, 1),
                KeyCode::Char('p' | 'P') => self.scroll(false, 1),
                KeyCode::Char('d' | 'D') => self.scroll(true, half_page),
                KeyCode::Char('u' | 'U') => self.scroll(false, half_page),
                KeyCode::Char('f' | 'F') => self.scroll(true, self.body_height),
                KeyCode::Char('b' | 'B') => self.scroll(false, self.body_height),
                KeyCode::Home => self.scroll_to_top(),
                KeyCode::End => self.at_bottom = true,
                _ => {}
            }
            return TranscriptEvent::Handled;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return TranscriptEvent::Close,
            KeyCode::Down | KeyCode::Char('j') => self.scroll(true, 1),
            KeyCode::Up | KeyCode::Char('k') => self.scroll(false, 1),
            KeyCode::PageDown => self.scroll(true, half_page),
            KeyCode::PageUp => self.scroll(false, half_page),
            KeyCode::Char(' ') => self.scroll(true, self.body_height),
            KeyCode::Char('b') => self.scroll(false, self.body_height),
            KeyCode::Home | KeyCode::Char('g') => self.scroll_to_top(),
            KeyCode::End | KeyCode::Char('G') => self.at_bottom = true,
            KeyCode::Char('/') => {
                self.search = Some(SearchCheckpoint {
                    query: std::mem::take(&mut self.query),
                    top: self.top_row(),
                    at_bottom: self.at_bottom,
                });
                self.query_cursor = 0;
                self.matches.clear();
                self.selected_match = None;
            }
            KeyCode::Char('n') => self.next_match(true),
            KeyCode::Char('N') => self.next_match(false),
            _ => {}
        }
        TranscriptEvent::Handled
    }

    pub(super) fn frame(&mut self, width: usize, height: usize) -> TranscriptFrame {
        if width == 0 || height == 0 {
            return TranscriptFrame {
                lines: Vec::new(),
                texts: Vec::new(),
            };
        }
        if self.width != width {
            self.reflow(width);
        }
        let footer_rows = FOOTER_ROWS.min(height);
        self.body_height = height - footer_rows;
        let top = self.top_row();
        self.top = top;
        let theme = self.display.theme();
        let mut lines = Vec::with_capacity(height);
        let mut texts = Vec::with_capacity(height);
        for row in top..top + self.body_height {
            let Some(role_line) = self.rows.get(row) else {
                lines.push(StyledLine::default());
                texts.push(String::new());
                continue;
            };
            let mut role_line = role_line.clone();
            role_line.truncate(width);
            let highlights = self
                .matches
                .iter()
                .enumerate()
                .filter(|(_, found)| found.row == row)
                .map(|(index, found)| (found.columns, Some(index) == self.selected_match))
                .collect::<Vec<_>>();
            lines.push(highlight(
                role_line.resolve(theme, width),
                &highlights,
                theme,
            ));
            texts.push(role_line.text());
        }
        let (footer, cursor) = self.footer(width);
        let last = FOOTER_ROWS - 1;
        for (index, line) in footer
            .into_iter()
            .enumerate()
            .skip(FOOTER_ROWS - footer_rows)
        {
            texts.push(line.text());
            let resolved = line.resolve(theme, width);
            lines.push(match cursor.filter(|_| index == last) {
                Some(columns) => highlight(resolved, &[(columns, false)], theme),
                None => resolved,
            });
        }
        TranscriptFrame { lines, texts }
    }

    /// The blank row, the dim rule and either the hint row or the open search bar, with the
    /// columns of the search bar's cursor cell, which is drawn in reverse video.
    fn footer(&self, width: usize) -> ([RoleLine; FOOTER_ROWS], Option<(usize, usize)>) {
        let rule = RoleLine::role("─".repeat(width), Role::MutedText);
        let mut last = RoleLine::default();
        let mut cursor_columns = None;
        let badge = if self.search.is_some() {
            last.push(format!("{FOOTER_INDENT}/"), Some(Role::MutedText));
            let (before, after) = self.query.split_at(self.query_cursor);
            last.push(before, None);
            let mut rest = after.chars();
            let cursor = rest.next().map_or(" ".to_owned(), String::from);
            let start = last.text().width();
            cursor_columns = Some((start, start + cursor.width()));
            last.push(cursor, None);
            last.push(rest.as_str(), None);
            if self.query.is_empty() {
                None
            } else if self.matches.is_empty() {
                Some((NO_MATCHES.to_owned(), Role::Error))
            } else {
                Some((self.match_badge(), Role::MutedText))
            }
        } else {
            let navigates = !self.query.is_empty() && !self.matches.is_empty();
            let hint = if navigates { SEARCH_HINT } else { SCROLL_HINT };
            last.push(
                format!(
                    "{FOOTER_INDENT}{FOOTER_TITLE} · {}",
                    self.display.navigation_hint(hint)
                ),
                Some(Role::MutedText),
            );
            if let Some(notice) = self.notice {
                last.push(" · ", Some(Role::MutedText));
                last.push(notice, Some(Role::Permission));
            }
            navigates.then(|| (self.match_badge(), Role::MutedText))
        };
        if let Some((badge, role)) = badge {
            let used = last.text().width();
            let badge_columns = badge.width();
            if used + badge_columns < width {
                last.push(" ".repeat(width - used - badge_columns), None);
                last.push(badge, Some(role));
            }
        }
        last.truncate(width);
        ([RoleLine::default(), rule, last], cursor_columns)
    }

    fn match_badge(&self) -> String {
        format!(
            "{}/{}{BADGE_MARGIN}",
            self.selected_match.map_or(0, |index| index + 1),
            self.matches.len()
        )
    }

    fn search_key(&mut self, key: KeyEvent) {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if key.code == KeyCode::Esc
            || control && matches!(key.code, KeyCode::Char('c' | 'C' | 'g' | 'G'))
        {
            let checkpoint = self.search.take().unwrap();
            self.query = checkpoint.query;
            self.query_cursor = self.query.len();
            self.top = checkpoint.top;
            self.at_bottom = checkpoint.at_bottom;
            self.find_matches(checkpoint.top);
            return;
        }
        if control {
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
                if self.matches.is_empty() {
                    self.clear_search();
                }
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

    /// Searches again as the query is typed: the view jumps to the match nearest where `/` was
    /// pressed, or returns there while nothing matches.
    fn query_changed(&mut self) {
        let Some(checkpoint) = &self.search else {
            return;
        };
        let (top, at_bottom) = (checkpoint.top, checkpoint.at_bottom);
        self.find_matches(top);
        if self.matches.is_empty() {
            self.top = top;
            self.at_bottom = at_bottom;
        }
    }

    /// Finds the query in every row, case-insensitively, and selects the first match at or below
    /// `from`, or the last one above it.
    fn find_matches(&mut self, from: usize) {
        self.matches.clear();
        self.selected_match = None;
        let query = self.query.chars().map(lowercase).collect::<Vec<_>>();
        if query.is_empty() {
            return;
        }
        for (row, text) in self.texts.iter().enumerate() {
            let cells = text
                .chars()
                .map(|character| (lowercase(character), character.width().unwrap_or(0)))
                .collect::<Vec<_>>();
            let mut start = 0;
            let mut column = 0;
            while start + query.len() <= cells.len() {
                if cells[start..start + query.len()]
                    .iter()
                    .map(|(character, _)| *character)
                    .eq(query.iter().copied())
                {
                    let width = cells[start..start + query.len()]
                        .iter()
                        .map(|(_, width)| width)
                        .sum::<usize>();
                    self.matches.push(Match {
                        row,
                        columns: (column, column + width),
                    });
                    column += width;
                    start += query.len();
                } else {
                    column += cells[start].1;
                    start += 1;
                }
            }
        }
        if self.matches.is_empty() {
            return;
        }
        let index = self
            .matches
            .iter()
            .position(|found| found.row >= from)
            .unwrap_or(self.matches.len() - 1);
        self.select_match(index);
    }

    fn next_match(&mut self, forward: bool) {
        if self.query.is_empty() || self.matches.is_empty() {
            return;
        }
        let count = self.matches.len();
        let index = match self.selected_match {
            Some(index) if forward => (index + 1) % count,
            Some(index) => (index + count - 1) % count,
            None => 0,
        };
        self.select_match(index);
    }

    fn select_match(&mut self, index: usize) {
        self.selected_match = Some(index);
        let row = self.matches[index].row;
        let top = self.top_row();
        if row < top || row >= top + self.body_height {
            self.top = row.saturating_sub(self.body_height / 2);
            self.at_bottom = false;
            self.top = self.top_row();
            self.at_bottom = self.top == self.maximum_top();
        }
    }

    fn clear_search(&mut self) {
        self.search = None;
        self.query.clear();
        self.query_cursor = 0;
        self.matches.clear();
        self.selected_match = None;
    }

    fn scroll(&mut self, forward: bool, amount: usize) {
        let top = self.top_row();
        self.top = if forward {
            top.saturating_add(amount)
        } else {
            top.saturating_sub(amount)
        }
        .min(self.maximum_top());
        self.at_bottom = self.top == self.maximum_top();
    }

    fn scroll_to_top(&mut self) {
        self.top = 0;
        self.at_bottom = self.maximum_top() == 0;
    }

    fn maximum_top(&self) -> usize {
        self.rows.len().saturating_sub(self.body_height)
    }

    fn top_row(&self) -> usize {
        if self.at_bottom {
            self.maximum_top()
        } else {
            self.top.min(self.maximum_top())
        }
    }

    /// Lays the content out for a new width, keeping the reading position in proportion. As in
    /// Claude Code, a resize ends any search, since its matches belong to the old layout.
    fn reflow(&mut self, width: usize) {
        let (top, rows) = (self.top_row(), self.rows.len());
        self.width = width;
        self.layout();
        if !self.at_bottom && rows > 0 {
            self.top = top * self.rows.len() / rows;
        }
        self.clear_search();
    }

    fn layout(&mut self) {
        let mut scrollback = Scrollback::new();
        for piece in &self.document.pieces {
            piece.append_to(&mut scrollback, self.width);
        }
        let frame = scrollback.frame_with_motion(self.width, usize::MAX, true);
        self.rows = frame.styled;
        self.texts = frame.lines;
        // The footer's own blank row separates the last message from the rule.
        while self.texts.last().is_some_and(String::is_empty) {
            self.texts.pop();
            self.rows.pop();
        }
    }
}

fn lowercase(character: char) -> char {
    character.to_lowercase().next().unwrap_or(character)
}

/// Paints search matches over a resolved row: every match in reverse video, the selected one
/// bold and underlined on the warning colour, as Claude Code's transcript search does.
fn highlight(
    line: StyledLine,
    matches: &[((usize, usize), bool)],
    theme: super::style::Theme,
) -> StyledLine {
    if matches.is_empty() {
        return line;
    }
    let style_at = |column: usize, style: Style| {
        let Some((_, selected)) = matches
            .iter()
            .find(|((start, end), _)| (*start..*end).contains(&column))
        else {
            return style;
        };
        if *selected {
            Style {
                foreground: theme.style(Role::InverseText).foreground,
                background: theme.style(Role::Warning).foreground,
                bold: true,
                underline: true,
                dim: false,
                reversed: false,
                ..style
            }
        } else {
            Style {
                reversed: true,
                ..style
            }
        }
    };
    let mut highlighted = StyledLine {
        link: line.link.clone(),
        ..Default::default()
    };
    let mut column = 0;
    for span in line.spans {
        for character in span.text.chars() {
            highlighted.push(
                Span::styled(character.to_string(), style_at(column, span.style))
                    .linked(span.link.clone()),
            );
            column += character.width().unwrap_or(0);
        }
    }
    highlighted
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
    use crossterm::event::KeyModifiers;

    fn press(transcript: &mut Transcript, code: KeyCode) -> TranscriptEvent {
        transcript.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn control(transcript: &mut Transcript, character: char) -> TranscriptEvent {
        transcript.key(KeyEvent::new(
            KeyCode::Char(character),
            KeyModifiers::CONTROL,
        ))
    }

    fn numbered(count: usize) -> Transcript {
        let text = (0..count)
            .map(|index| format!("line {index}\n"))
            .collect::<String>();
        Transcript::new(
            TranscriptDocument {
                pieces: vec![TranscriptPiece::text(text, None)],
            },
            DisplayProfile::default(),
        )
    }

    fn texts(transcript: &mut Transcript, width: usize, height: usize) -> Vec<String> {
        transcript.frame(width, height).texts
    }

    #[test]
    fn the_footer_matches_claude_code_and_the_view_opens_at_the_end() {
        let mut transcript = numbered(40);
        let frame = texts(&mut transcript, 90, 12);
        assert_eq!(frame.len(), 12);
        assert_eq!(frame[8], "line 39");
        assert_eq!(frame[9], "");
        assert_eq!(frame[10], "─".repeat(90));
        assert_eq!(
            frame[11],
            "  Showing detailed transcript · ctrl+o to toggle · ↑↓ scroll · home/end top/bottom"
        );
        assert!(!frame.iter().any(|line| line.starts_with("› ")));
        assert!(!frame.iter().any(|line| line.starts_with("Transcript")));
    }

    #[test]
    fn less_style_keys_scroll_by_line_half_page_page_and_to_either_end() {
        let mut transcript = numbered(100);
        let body = |transcript: &mut Transcript| texts(transcript, 40, 13)[0].clone();
        assert_eq!(body(&mut transcript), "line 90");
        press(&mut transcript, KeyCode::Char('k'));
        assert_eq!(body(&mut transcript), "line 89");
        control(&mut transcript, 'p');
        assert_eq!(body(&mut transcript), "line 88");
        control(&mut transcript, 'u');
        assert_eq!(body(&mut transcript), "line 83");
        press(&mut transcript, KeyCode::PageUp);
        assert_eq!(body(&mut transcript), "line 78");
        press(&mut transcript, KeyCode::Char('b'));
        assert_eq!(body(&mut transcript), "line 68");
        control(&mut transcript, 'b');
        assert_eq!(body(&mut transcript), "line 58");
        press(&mut transcript, KeyCode::Char(' '));
        assert_eq!(body(&mut transcript), "line 68");
        control(&mut transcript, 'f');
        assert_eq!(body(&mut transcript), "line 78");
        control(&mut transcript, 'd');
        assert_eq!(body(&mut transcript), "line 83");
        press(&mut transcript, KeyCode::PageDown);
        assert_eq!(body(&mut transcript), "line 88");
        press(&mut transcript, KeyCode::Char('j'));
        control(&mut transcript, 'n');
        press(&mut transcript, KeyCode::Down);
        assert_eq!(body(&mut transcript), "line 90");
        press(&mut transcript, KeyCode::Char('g'));
        assert_eq!(body(&mut transcript), "line 0");
        press(&mut transcript, KeyCode::Char('G'));
        assert_eq!(body(&mut transcript), "line 90");
        press(&mut transcript, KeyCode::Home);
        assert_eq!(body(&mut transcript), "line 0");
        press(&mut transcript, KeyCode::End);
        assert_eq!(body(&mut transcript), "line 90");
        transcript.key(KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL));
        assert_eq!(body(&mut transcript), "line 0");
        transcript.wheel(true);
        assert_eq!(body(&mut transcript), "line 3");
        transcript.wheel(false);
        assert_eq!(body(&mut transcript), "line 0");
        transcript.key(KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL));
        assert_eq!(body(&mut transcript), "line 90");
    }

    #[test]
    fn escape_q_ctrl_c_and_ctrl_o_close_while_help_and_prompt_jumps_are_gone() {
        for close in [
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL),
        ] {
            assert_eq!(numbered(3).key(close), TranscriptEvent::Close);
        }
        let mut searching = numbered(3);
        press(&mut searching, KeyCode::Char('/'));
        assert_eq!(
            press(&mut searching, KeyCode::Char('q')),
            TranscriptEvent::Handled
        );
        assert_eq!(control(&mut searching, 'o'), TranscriptEvent::Close);
        let mut transcript = numbered(60);
        let before = texts(&mut transcript, 40, 13);
        for key in ['?', '{', '}'] {
            assert_eq!(
                press(&mut transcript, KeyCode::Char(key)),
                TranscriptEvent::Handled
            );
        }
        assert_eq!(texts(&mut transcript, 40, 13), before);
    }

    #[test]
    fn incremental_search_ignores_case_counts_matches_and_n_navigates() {
        let mut transcript = Transcript::new(
            vec![
                "alpha Needle".into(),
                "beta".into(),
                "needle and NEEDLE".into(),
                "gamma".into(),
            ]
            .into(),
            DisplayProfile::default(),
        );
        texts(&mut transcript, 80, 10);
        press(&mut transcript, KeyCode::Char('/'));
        for character in "needle".chars() {
            press(&mut transcript, KeyCode::Char(character));
        }
        let frame = texts(&mut transcript, 80, 10);
        assert_eq!(frame[9], format!("  /needle {}1/3  ", " ".repeat(65)));
        press(&mut transcript, KeyCode::Enter);
        let frame = texts(&mut transcript, 80, 10);
        assert_eq!(
            frame[9],
            format!(
                "  Showing detailed transcript · ctrl+o to toggle · n/N to navigate{}1/3  ",
                " ".repeat(9)
            )
        );
        press(&mut transcript, KeyCode::Char('n'));
        assert!(texts(&mut transcript, 80, 10)[9].ends_with("2/3  "));
        press(&mut transcript, KeyCode::Char('n'));
        press(&mut transcript, KeyCode::Char('n'));
        assert!(texts(&mut transcript, 80, 10)[9].ends_with("1/3  "));
        press(&mut transcript, KeyCode::Char('N'));
        assert!(texts(&mut transcript, 80, 10)[9].ends_with("3/3  "));
    }

    #[test]
    fn the_selected_match_is_bold_underlined_on_warning_and_the_rest_reversed() {
        let mut transcript =
            Transcript::new(vec!["ab ab".into()].into(), DisplayProfile::default());
        transcript.frame(20, 8);
        press(&mut transcript, KeyCode::Char('/'));
        press(&mut transcript, KeyCode::Char('a'));
        press(&mut transcript, KeyCode::Char('b'));
        let frame = transcript.frame(20, 8);
        let row = frame.texts.iter().position(|text| text == "ab ab").unwrap();
        let styles = frame.lines[row]
            .spans
            .iter()
            .flat_map(|span| span.text.chars().map(|_| span.style))
            .collect::<Vec<_>>();
        let theme = DisplayProfile::default().theme();
        for selected in &styles[0..2] {
            assert!(selected.bold && selected.underline && !selected.reversed);
            assert_eq!(selected.background, theme.style(Role::Warning).foreground);
        }
        assert!(!styles[2].reversed && !styles[2].bold);
        for other in &styles[3..5] {
            assert!(other.reversed && !other.underline);
        }
    }

    #[test]
    fn a_search_without_matches_is_not_kept_and_cancel_restores_the_previous_one() {
        let mut transcript = numbered(50);
        texts(&mut transcript, 40, 12);
        press(&mut transcript, KeyCode::Char('/'));
        press(&mut transcript, KeyCode::Char('z'));
        let frame = texts(&mut transcript, 40, 12);
        assert!(frame[11].ends_with(NO_MATCHES), "{frame:?}");
        assert_eq!(
            frame[0], "line 41",
            "no match returns to where / was pressed"
        );
        press(&mut transcript, KeyCode::Enter);
        assert_eq!(
            texts(&mut transcript, 40, 12)[11],
            "  Showing detailed transcript · ctrl+o to toggle · ↑↓ scroll · home/end top/bottom"
                .chars()
                .take(40)
                .collect::<String>()
        );

        press(&mut transcript, KeyCode::Char('/'));
        for character in "line 1".chars() {
            press(&mut transcript, KeyCode::Char(character));
        }
        press(&mut transcript, KeyCode::Enter);
        let committed = texts(&mut transcript, 40, 12);
        press(&mut transcript, KeyCode::Char('/'));
        press(&mut transcript, KeyCode::Char('4'));
        control(&mut transcript, 'g');
        assert_eq!(texts(&mut transcript, 40, 12), committed);
    }

    #[test]
    fn pasting_fills_only_an_open_search_and_a_resize_ends_it() {
        let mut transcript = numbered(30);
        texts(&mut transcript, 40, 12);
        transcript.paste("line\n2");
        assert!(transcript.query.is_empty());
        press(&mut transcript, KeyCode::Char('/'));
        transcript.paste("line\n2");
        assert_eq!(transcript.query, "line 2");
        assert!(!transcript.matches.is_empty());
        texts(&mut transcript, 41, 12);
        assert!(transcript.search.is_none() && transcript.matches.is_empty());
    }

    #[test]
    fn content_is_fixed_until_replaced_and_a_replacement_starts_at_the_end() {
        let mut transcript = numbered(30);
        press(&mut transcript, KeyCode::Char('g'));
        transcript.update(numbered(30).document);
        assert_eq!(texts(&mut transcript, 40, 12)[0], "line 0");
        transcript.update(numbered(31).document);
        assert_eq!(texts(&mut transcript, 40, 12)[8], "line 30");
    }

    #[test]
    fn rows_keep_roles_fills_and_links_from_the_conversation_layout() {
        let link = Hyperlink::web("https://example.com").unwrap();
        let mut document = TranscriptDocument::default();
        document.push(TranscriptPiece::Text {
            text: "\n❯ ".into(),
            role: Some(Role::Subtle),
            fill: Some(Role::UserMessageBg),
            hang: 0,
            link: None,
        });
        document.push(TranscriptPiece::Text {
            text: "docs\n".into(),
            role: None,
            fill: Some(Role::UserMessageBg),
            hang: 0,
            link: Some(link.clone()),
        });
        document.push(TranscriptPiece::RightAligned {
            spans: vec![("03:45 PM".into(), Some(Role::MutedText))],
            right_margin: 2,
        });
        document.push(TranscriptPiece::hanging(
            "  ⎿  a long result line that wraps\n",
            Some(Role::MutedText),
            5,
        ));
        let mut transcript = Transcript::new(document, DisplayProfile::default());
        let frame = transcript.frame(20, 12);
        let prompt = frame
            .texts
            .iter()
            .position(|text| text == "❯ docs")
            .unwrap();
        assert_eq!(frame.texts[prompt + 1], "          03:45 PM  ");
        assert_eq!(frame.texts[prompt + 2], "  ⎿  a long result");
        assert_eq!(frame.texts[prompt + 3], "     line that wraps");
        let row = transcript.rows[prompt].clone();
        assert_eq!(row.fill, Some(Role::UserMessageBg));
        assert!(
            row.spans
                .iter()
                .any(|span| span.link.as_ref() == Some(&link))
        );
    }
}
