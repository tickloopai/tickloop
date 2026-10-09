//! The saved-conversation picker that `/resume` opens.
//!
//! As in Claude Code's resume picker, each saved conversation reads as one title row and one
//! muted metadata row under a search field, newest activity first. Search matches the complete
//! underlying title, prompt and metadata even where a row shows only a shortened title. Every
//! row keeps the index of its saved conversation, so filtering, scrolling and duplicate titles
//! never change which conversation Enter opens; the caller maps that index to the conversation.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_width::UnicodeWidthChar;
use uuid::Uuid;

use super::console::truncate_width;
use super::display::{DisplayProfile, Glyph};
use super::editor::{Editor, EditorCommand, EditorOutcome};
use super::style::{Role, RoleLine};

const ELLIPSIS: &str = "…";
const SEARCH_PREFIX: &str = "⌕ ";
const SEARCH_PLACEHOLDER: &str = "Search…";
const ROW_INDENT: &str = "  ";
/// Rows each conversation takes: its title and its metadata.
const ENTRY_ROWS: usize = 2;
/// The purpose line under the title only appears once the list keeps several conversations.
const CONTEXT_MIN_HEIGHT: usize = 10;
const HINT_MIN_HEIGHT: usize = 3;
const TITLE_MIN_HEIGHT: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ConversationPickerPurpose {
    Resume,
    CopyAsNew,
}

impl ConversationPickerPurpose {
    fn title(self) -> &'static str {
        match self {
            Self::Resume => "Resume Chat",
            Self::CopyAsNew => "Copy Chat",
        }
    }

    fn context(self) -> &'static str {
        match self {
            Self::Resume => "Opens saved history; starts no work.",
            Self::CopyAsNew => "Copies to a new Chat; original kept.",
        }
    }

    fn open_action(self) -> &'static str {
        match self {
            Self::Resume => "open",
            Self::CopyAsNew => "copy",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ConversationPickerFocus {
    List,
    Search,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ConversationPickerEffect {
    None,
    /// Opens the conversation at this index of the entries the picker was given.
    Open(usize),
    Cancel,
    /// Edits the search query in the external editor (`Ctrl+G`), as every text input does.
    EditQuery,
}

/// One saved conversation as the picker shows and searches it. It carries no identity: its
/// position among the entries is the index the picker reports.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ConversationEntry {
    title: String,
    metadata: String,
    search_text: String,
}

impl ConversationEntry {
    /// `title` and each `metadata` part are shown on one line each. `searchable` adds text
    /// matched but not shown, such as the first prompt behind a custom title.
    pub(super) fn new(title: &str, metadata: &[String], searchable: &[&str]) -> Self {
        let title = compact_line(title);
        let metadata = metadata
            .iter()
            .map(|part| compact_line(part))
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" · ");
        let search_text = std::iter::once(title.as_str())
            .chain(std::iter::once(metadata.as_str()))
            .map(str::to_owned)
            .chain(searchable.iter().map(|text| compact_line(text)))
            .collect::<Vec<_>>()
            .join("\n")
            .to_lowercase();
        Self {
            title,
            metadata,
            search_text,
        }
    }

    /// The complete title and metadata on one line, for output that is not width-bounded.
    pub(super) fn plain_row(&self) -> String {
        if self.metadata.is_empty() {
            self.title.clone()
        } else {
            format!("{} · {}", self.title, self.metadata)
        }
    }

    fn matches(&self, query: &str) -> bool {
        self.search_text.contains(query)
    }
}

pub(super) struct ConversationPickerFrame {
    pub(super) lines: Vec<RoleLine>,
    /// Row and column of the text cursor while the search field has focus.
    pub(super) cursor: Option<(usize, usize)>,
}

pub(super) struct ConversationPicker {
    id: Uuid,
    purpose: ConversationPickerPurpose,
    entries: Vec<ConversationEntry>,
    query_editor: Editor,
    focus: ConversationPickerFocus,
    /// Index into `entries`. It is remembered while a search shows no match, so clearing the
    /// search returns to the same conversation.
    selected: usize,
    /// Indices into `entries` that match the query, in the entries' order.
    matches: Vec<usize>,
    /// Position in `matches` of the first conversation in view.
    top: usize,
    row_targets: Vec<(std::ops::Range<usize>, usize)>,
    /// Replaces the key hint until the next key, such as an external editor failure.
    notice: Option<String>,
}

impl ConversationPicker {
    /// Entries come newest activity first and keep that order; the list starts focused on the
    /// first one.
    pub(super) fn new(purpose: ConversationPickerPurpose, entries: Vec<ConversationEntry>) -> Self {
        let matches = (0..entries.len()).collect();
        Self {
            id: Uuid::new_v4(),
            purpose,
            entries,
            query_editor: Editor::new(),
            focus: ConversationPickerFocus::List,
            selected: 0,
            matches,
            top: 0,
            row_targets: Vec::new(),
            notice: None,
        }
    }

    pub(super) fn replace_entries(&mut self, entries: Vec<ConversationEntry>) {
        self.entries = entries;
        self.refilter();
    }
    pub(super) fn matching_indices(&self) -> &[usize] {
        &self.matches
    }
    pub(super) fn focus_list(&mut self) {
        self.focus = ConversationPickerFocus::List;
    }
    pub(super) fn id(&self) -> Uuid {
        self.id
    }

    pub(super) fn title(&self) -> &'static str {
        self.purpose.title()
    }

    pub(super) fn context(&self) -> &'static str {
        self.purpose.context()
    }

    pub(super) fn entries(&self) -> &[ConversationEntry] {
        &self.entries
    }

    pub(super) fn focus(&self) -> ConversationPickerFocus {
        self.focus
    }

    pub(super) fn query(&self) -> &str {
        self.query_editor.draft()
    }

    /// Takes the text an external editor returned for the search query.
    pub(super) fn replace_query_text(&mut self, text: &str) {
        self.focus = ConversationPickerFocus::Search;
        let query = compact_line(text);
        let cursor = query.len();
        self.query_editor.replace_draft(query, cursor);
        self.refilter();
    }

    pub(super) fn set_notice(&mut self, notice: String) {
        self.notice = Some(notice);
    }

    /// The entry index Enter opens, if any conversation matches.
    pub(super) fn selection(&self) -> Option<usize> {
        self.selected_position().map(|_| self.selected)
    }

    pub(super) fn key(&mut self, key: KeyEvent) -> ConversationPickerEffect {
        self.notice = None;
        if key.code == KeyCode::Char('g') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.focus = ConversationPickerFocus::Search;
            return ConversationPickerEffect::EditQuery;
        }
        match self.focus {
            ConversationPickerFocus::List => self.list_key(key),
            ConversationPickerFocus::Search => self.search_key(key),
        }
    }

    /// Pasted text goes into the search field as one line.
    pub(super) fn paste(&mut self, text: &str) {
        self.focus = ConversationPickerFocus::Search;
        self.edit_query(EditorCommand::InsertPasted(compact_line(text)));
    }

    fn list_key(&mut self, key: KeyEvent) -> ConversationPickerEffect {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Up if self.selected_position() == Some(0) || self.matches.is_empty() => {
                self.focus = ConversationPickerFocus::Search;
            }
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
            KeyCode::Char('p') if control => self.move_selection(-1),
            KeyCode::Char('n') if control => self.move_selection(1),
            KeyCode::Enter => return self.open(),
            KeyCode::Esc => return ConversationPickerEffect::Cancel,
            KeyCode::Char(_) if control || key.modifiers.contains(KeyModifiers::ALT) => {}
            KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Char('/') => self.focus = ConversationPickerFocus::Search,
            KeyCode::Char(character) if !character.is_whitespace() => {
                self.focus = ConversationPickerFocus::Search;
                self.insert(character);
            }
            _ => {}
        }
        ConversationPickerEffect::None
    }

    fn search_key(&mut self, key: KeyEvent) -> ConversationPickerEffect {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            let command = match key.code {
                KeyCode::Char('n' | 'p') => {
                    self.move_selection(if key.code == KeyCode::Char('n') {
                        1
                    } else {
                        -1
                    });
                    None
                }
                KeyCode::Char('a') => Some(EditorCommand::MoveLineStart),
                KeyCode::Char('e') => Some(EditorCommand::MoveLineEnd),
                KeyCode::Char('b') => Some(EditorCommand::MoveLeft),
                KeyCode::Char('f') => Some(EditorCommand::MoveRight),
                KeyCode::Left => Some(EditorCommand::MoveWordBackward),
                KeyCode::Right => Some(EditorCommand::MoveWordForward),
                KeyCode::Char('h') => Some(EditorCommand::DeleteBackward),
                KeyCode::Char('d') => Some(EditorCommand::DeleteForward),
                KeyCode::Backspace => Some(EditorCommand::KillWordBackward),
                KeyCode::Char('u') => Some(EditorCommand::KillToLineStart),
                KeyCode::Char('k') => Some(EditorCommand::KillToLineEnd),
                KeyCode::Char('w') => Some(EditorCommand::KillToPreviousWhitespace),
                KeyCode::Char('y') => Some(EditorCommand::Yank),
                KeyCode::Char('_' | '7') => Some(EditorCommand::Undo),
                KeyCode::Char('-') if key.modifiers.contains(KeyModifiers::SHIFT) => {
                    Some(EditorCommand::Undo)
                }
                _ => None,
            };
            if let Some(command) = command {
                self.edit_query(command);
            }
            return ConversationPickerEffect::None;
        }
        if key.modifiers.contains(KeyModifiers::ALT) {
            let command = match key.code {
                KeyCode::Left | KeyCode::Char('b') => Some(EditorCommand::MoveWordBackward),
                KeyCode::Right | KeyCode::Char('f') => Some(EditorCommand::MoveWordForward),
                KeyCode::Backspace => Some(EditorCommand::KillWordBackward),
                KeyCode::Delete => Some(EditorCommand::KillToLineEnd),
                KeyCode::Char('d') => Some(EditorCommand::KillWordForward),
                KeyCode::Char('y') => Some(EditorCommand::YankPrevious),
                _ => None,
            };
            if let Some(command) = command {
                self.edit_query(command);
            }
            return ConversationPickerEffect::None;
        }
        match key.code {
            KeyCode::Enter => return self.open(),
            KeyCode::Down => self.focus = ConversationPickerFocus::List,
            KeyCode::Esc if self.query().is_empty() => self.focus = ConversationPickerFocus::List,
            KeyCode::Esc => self.edit_query(EditorCommand::Clear),
            KeyCode::Backspace if self.query().is_empty() => {
                self.focus = ConversationPickerFocus::List;
            }
            KeyCode::Backspace => self.edit_query(EditorCommand::DeleteBackward),
            KeyCode::Delete => self.edit_query(EditorCommand::DeleteForward),
            KeyCode::Left => self.edit_query(EditorCommand::MoveLeft),
            KeyCode::Right => self.edit_query(EditorCommand::MoveRight),
            KeyCode::Home => self.edit_query(EditorCommand::MoveLineStart),
            KeyCode::End => self.edit_query(EditorCommand::MoveLineEnd),
            KeyCode::Char(character) => self.insert(character),
            _ => {}
        }
        ConversationPickerEffect::None
    }

    fn open(&self) -> ConversationPickerEffect {
        self.selection().map_or(
            ConversationPickerEffect::None,
            ConversationPickerEffect::Open,
        )
    }

    fn insert(&mut self, character: char) {
        if character.is_control() {
            return;
        }
        self.edit_query(EditorCommand::Insert(character.to_string()));
    }

    fn edit_query(&mut self, command: EditorCommand) {
        let moves_cursor = matches!(
            command,
            EditorCommand::MoveLeft
                | EditorCommand::MoveRight
                | EditorCommand::MoveLineStart
                | EditorCommand::MoveLineEnd
                | EditorCommand::MoveWordBackward
                | EditorCommand::MoveWordForward
        );
        if self.query_editor.apply(command) == EditorOutcome::Changed && !moves_cursor {
            self.refilter();
        }
    }

    /// Keeps the selected conversation while it still matches, otherwise moves to the first
    /// match. With no match the previous selection is remembered rather than replaced.
    fn refilter(&mut self) {
        let query = compact_line(self.query()).to_lowercase();
        self.matches = (0..self.entries.len())
            .filter(|index| query.is_empty() || self.entries[*index].matches(&query))
            .collect();
        if self.selected_position().is_none()
            && let Some(first) = self.matches.first()
        {
            self.selected = *first;
        }
        self.top = 0;
    }

    fn selected_position(&self) -> Option<usize> {
        self.matches
            .iter()
            .position(|index| *index == self.selected)
    }

    fn move_selection(&mut self, delta: isize) {
        let Some(position) = self.selected_position() else {
            return;
        };
        let last = self.matches.len() - 1;
        let next = position.saturating_add_signed(delta).min(last);
        self.selected = self.matches[next];
    }

    /// Draws the whole screen: title, search field, conversations and, after them, the key
    /// hint, each row cut to `width` columns. Short screens drop the purpose line, then the
    /// title and hint, and keep the selected conversation and the search field.
    pub(super) fn content_rows(&self) -> usize {
        3 + (self.matches.len() * ENTRY_ROWS).max(1)
    }

    pub(super) fn frame_in_dialog(
        &mut self,
        width: usize,
        height: usize,
        display: DisplayProfile,
    ) -> ConversationPickerFrame {
        self.frame_with_context(width, height, display, false)
    }

    pub(super) fn select_entry(&mut self, index: usize) -> bool {
        if !self.matches.contains(&index) {
            return false;
        }
        self.selected = index;
        self.focus = ConversationPickerFocus::List;
        true
    }

    pub(super) fn select_at_row(&mut self, row: usize) -> bool {
        let index = self
            .row_targets
            .iter()
            .find(|(rows, _)| rows.contains(&row))
            .map(|(_, index)| *index);
        index.is_some_and(|index| self.select_entry(index))
    }

    #[cfg(test)]
    pub(super) fn frame(
        &mut self,
        width: usize,
        height: usize,
        display: DisplayProfile,
    ) -> ConversationPickerFrame {
        self.frame_with_context(width, height, display, true)
    }

    fn frame_with_context(
        &mut self,
        width: usize,
        height: usize,
        display: DisplayProfile,
        with_context: bool,
    ) -> ConversationPickerFrame {
        let show_hint = height >= HINT_MIN_HEIGHT;
        let show_title = height >= TITLE_MIN_HEIGHT;
        let show_context = with_context && height >= CONTEXT_MIN_HEIGHT;
        let show_search = height >= 2 || self.focus == ConversationPickerFocus::Search;
        let chrome_rows = usize::from(show_hint)
            + usize::from(show_title)
            + usize::from(show_context)
            + usize::from(show_search);
        let list_rows = height.saturating_sub(chrome_rows);
        let mut lines = Vec::with_capacity(height);
        let mut cursor = None;
        if show_title {
            lines.push(self.title_line(width));
        }
        if show_context {
            lines.push(RoleLine::role(
                ellipsize(self.purpose.context(), width),
                Role::MutedText,
            ));
        }
        if show_search {
            let (line, column) = self.search_line(width);
            if self.focus == ConversationPickerFocus::Search {
                cursor = Some((lines.len(), column));
            }
            lines.push(line);
        }
        let list_start = lines.len();
        lines.extend(self.list_lines(width, list_rows, display));
        for (rows, _) in &mut self.row_targets {
            rows.start += list_start;
            rows.end += list_start;
        }
        if show_hint {
            lines.resize(height.saturating_sub(1), RoleLine::default());
            lines.push(match &self.notice {
                Some(notice) => RoleLine::role(ellipsize(notice, width), Role::Warning),
                None => RoleLine::role(
                    fit_hint(&self.hint_parts(), width, display),
                    Role::MutedText,
                ),
            });
        }
        while lines.len() < height {
            lines.push(RoleLine::default());
        }
        ConversationPickerFrame { lines, cursor }
    }

    fn title_line(&self, width: usize) -> RoleLine {
        let position = match (self.selected_position(), self.query().trim().is_empty()) {
            (Some(position), true) => format!("{} of {}", position + 1, self.matches.len()),
            (Some(position), false) => {
                format!("{} of {} matching", position + 1, self.matches.len())
            }
            (None, _) => "no matches".into(),
        };
        let title = self.purpose.title();
        let mut line = RoleLine::default();
        if display_width(title) + display_width(" · ") + display_width(&position) <= width {
            line.push(title, Some(Role::DialogTitle));
            line.push(format!(" · {position}"), Some(Role::MutedText));
        } else {
            line.push(ellipsize(&position, width), Some(Role::MutedText));
        }
        line
    }

    /// The search row and the column of the cursor in it. A query wider than the row scrolls
    /// so the cursor stays in view.
    pub(super) fn search_line(&self, width: usize) -> (RoleLine, usize) {
        let query = self.query();
        let cursor = self.query_editor.cursor();
        let mut line = RoleLine::default();
        let prefix = truncate_width(SEARCH_PREFIX, width);
        let available = width.saturating_sub(display_width(&prefix));
        line.push(prefix.clone(), Some(Role::MutedText));
        if query.is_empty() {
            line.push(
                truncate_width(SEARCH_PLACEHOLDER, available),
                Some(Role::MutedText),
            );
            return (line, display_width(&prefix).min(width.saturating_sub(1)));
        }
        let mut start = 0;
        while start < cursor && display_width(&query[start..cursor]) >= available.max(1) {
            start = next_boundary(query, start);
        }
        let visible = truncate_width(&query[start..], available);
        let column = display_width(&prefix) + display_width(&query[start..cursor]);
        line.push(visible, Some(Role::Text));
        (line, column.min(width.saturating_sub(1)))
    }

    fn list_lines(&mut self, width: usize, rows: usize, display: DisplayProfile) -> Vec<RoleLine> {
        self.row_targets.clear();
        if rows == 0 {
            return Vec::new();
        }
        let Some(selected_position) = self.selected_position() else {
            let query = ellipsize(&compact_line(self.query()), width.saturating_sub(24));
            return vec![RoleLine::role(
                ellipsize(&format!("No conversations match “{query}”"), width),
                Role::MutedText,
            )];
        };
        let with_metadata = rows >= ENTRY_ROWS;
        let capacity = if with_metadata { rows / ENTRY_ROWS } else { 1 };
        if selected_position < self.top {
            self.top = selected_position;
        } else if selected_position >= self.top + capacity {
            self.top = selected_position + 1 - capacity;
        }
        self.top = self.top.min(self.matches.len().saturating_sub(capacity));
        let marker = display.glyph(Glyph::Prompt);
        let marker_width = display_width(marker).max(1);
        let text_width = width.saturating_sub(marker_width + 1);
        let mut lines = Vec::new();
        for (position, entry_index) in self
            .matches
            .iter()
            .enumerate()
            .skip(self.top)
            .take(capacity)
        {
            let first_row = lines.len();
            let entry = &self.entries[*entry_index];
            let selected = position == selected_position;
            let mut title = RoleLine::default();
            if selected {
                title.push(format!("{marker} "), Some(Role::Suggestion));
                title.push(ellipsize(&entry.title, text_width), Some(Role::Suggestion));
            } else {
                title.push(" ".repeat(marker_width + 1), None);
                title.push(ellipsize(&entry.title, text_width), Some(Role::Text));
            }
            title.truncate(width);
            lines.push(title);
            if with_metadata {
                let mut metadata = RoleLine::role(
                    format!(
                        "{ROW_INDENT}{}",
                        ellipsize(&entry.metadata, width.saturating_sub(ROW_INDENT.len()))
                    ),
                    Role::MutedText,
                );
                metadata.truncate(width);
                lines.push(metadata);
            }
            self.row_targets
                .push((first_row..lines.len(), *entry_index));
        }
        lines
    }

    /// Only the keys the current focus actually handles, each with the priority it keeps when
    /// the row is too narrow for all of them (higher stays longer).
    fn hint_parts(&self) -> Vec<(String, u8)> {
        let open = format!("enter {}", self.purpose.open_action());
        match self.focus {
            ConversationPickerFocus::List => vec![
                (open, 3),
                ("type to search".into(), 0),
                ("esc cancel".into(), 2),
            ],
            ConversationPickerFocus::Search => vec![
                (open, 3),
                ("ctrl+g editor".into(), 0),
                (
                    if self.query().is_empty() {
                        "esc list".into()
                    } else {
                        "esc clear search".into()
                    },
                    2,
                ),
            ],
        }
    }
}

/// Joins the hint parts that fit `width`, dropping the lowest priority first, and cuts what
/// remains only when even the most important part is too wide.
pub(super) fn fit_hint(parts: &[(String, u8)], width: usize, display: DisplayProfile) -> String {
    let mut kept = parts
        .iter()
        .map(|(text, priority)| (display.navigation_hint(text), *priority))
        .collect::<Vec<_>>();
    loop {
        let hint = kept
            .iter()
            .map(|(text, _)| text.as_str())
            .collect::<Vec<_>>()
            .join(" · ");
        if kept.len() <= 1 || display_width(&hint) <= width {
            return ellipsize(&hint, width);
        }
        let lowest = kept
            .iter()
            .enumerate()
            .min_by_key(|(_, (_, priority))| *priority)
            .map(|(index, _)| index)
            .expect("hint keeps at least one part");
        kept.remove(lowest);
    }
}

/// One line of readable text: control characters dropped, every run of whitespace (including
/// line breaks) folded into a single space.
pub(super) fn compact_line(text: &str) -> String {
    text.split(|character: char| character.is_whitespace())
        .map(|word| {
            word.chars()
                .filter(|character| !character.is_control())
                .collect::<String>()
        })
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn display_width(text: &str) -> usize {
    text.chars()
        .map(|character| character.width().unwrap_or(0))
        .sum()
}

/// `text` cut to `width` columns, ending in `…` when anything was cut. A space left at the
/// cut is dropped so the ellipsis follows the last word.
pub(super) fn ellipsize(text: &str, width: usize) -> String {
    if display_width(text) <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    format!("{}{ELLIPSIS}", truncate_width(text, width - 1).trim_end())
}

fn next_boundary(text: &str, cursor: usize) -> usize {
    text[cursor..]
        .char_indices()
        .nth(1)
        .map_or(text.len(), |(index, _)| cursor + index)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn control(character: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL)
    }

    fn type_text(picker: &mut ConversationPicker, text: &str) {
        for character in text.chars() {
            picker.key(key(KeyCode::Char(character)));
        }
    }

    fn entry(title: &str, metadata: &[&str]) -> ConversationEntry {
        let metadata = metadata
            .iter()
            .map(|part| (*part).to_owned())
            .collect::<Vec<_>>();
        ConversationEntry::new(title, &metadata, &[])
    }

    fn texts(frame: &ConversationPickerFrame) -> Vec<String> {
        frame.lines.iter().map(RoleLine::text).collect()
    }

    fn hint_row(lines: &[String]) -> &str {
        lines
            .iter()
            .rev()
            .find(|line| !line.is_empty())
            .expect("the frame has a hint")
    }

    fn picker(entries: Vec<ConversationEntry>) -> ConversationPicker {
        ConversationPicker::new(ConversationPickerPurpose::Resume, entries)
    }

    #[test]
    fn long_multiline_chinese_prompts_render_as_one_bounded_row_with_muted_metadata() {
        let prompt = "请分析腾讯控股最近三个季度的财报，\n\t比较营收增长与利润率变化，并给出风险提示\u{1b}[31m";
        let mut picker = picker(vec![entry(prompt, &["2h ago", "main"])]);
        for width in [24, 40, 80] {
            let frame = picker.frame(width, 12, DisplayProfile::default());
            let lines = texts(&frame);
            assert!(
                lines.iter().all(|line| display_width(line) <= width),
                "{lines:?}"
            );
            let title = &lines[3];
            assert!(title.starts_with("❯ 请分析腾讯控股"), "{title}");
            assert!(title.ends_with('…'), "{title}");
            assert!(
                !title.contains('\n') && !title.contains('\u{1b}'),
                "{title}"
            );
            assert_eq!(lines[4], "  2h ago · main");
            assert_eq!(frame.lines[4].spans[0].role, Some(Role::MutedText));
        }
        let wide = texts(&picker.frame(200, 12, DisplayProfile::default()));
        assert_eq!(
            wide[3],
            "❯ 请分析腾讯控股最近三个季度的财报， 比较营收增长与利润率变化，并给出风险提示[31m"
        );
    }

    #[test]
    fn search_matches_complete_hidden_text_and_keeps_entry_identity() {
        let long_title = format!("{} cedar", "padding ".repeat(40));
        let mut picker = picker(vec![
            entry("Duplicate title", &["1m ago"]),
            ConversationEntry::new(
                "Duplicate title",
                &["5m ago".into()],
                &["first prompt mentions juniper"],
            ),
            entry(&long_title, &["1h ago", "feature/search"]),
            entry("Duplicate title", &["2d ago"]),
        ]);
        let narrow = texts(&picker.frame(30, 12, DisplayProfile::default()));
        assert!(
            !narrow
                .iter()
                .any(|line| line.contains("cedar") || line.contains("juniper")),
            "{narrow:?}"
        );
        picker.key(key(KeyCode::Char('/')));
        type_text(&mut picker, "CEDAR");
        assert_eq!(picker.selection(), Some(2));
        assert_eq!(
            picker.key(key(KeyCode::Enter)),
            ConversationPickerEffect::Open(2)
        );
        picker.key(key(KeyCode::Esc));
        assert_eq!(picker.selection(), Some(2));
        type_text(&mut picker, "juniper");
        assert_eq!(picker.selection(), Some(1));
        let lines = texts(&picker.frame(80, 12, DisplayProfile::default()));
        assert!(lines[0].ends_with("1 of 1 matching"), "{lines:?}");
        assert_eq!(lines[3], "❯ Duplicate title");
        assert_eq!(lines[4], "  5m ago");
        picker.key(key(KeyCode::Esc));
        type_text(&mut picker, "feature/");
        assert_eq!(
            picker.key(key(KeyCode::Enter)),
            ConversationPickerEffect::Open(2)
        );
    }

    #[test]
    fn duplicate_titles_navigate_to_distinct_entries_in_newest_first_order() {
        let mut picker = picker(vec![
            entry("Same", &["1m ago"]),
            entry("Same", &["2m ago"]),
            entry("Same", &["3m ago"]),
        ]);
        assert_eq!(picker.selection(), Some(0));
        picker.key(key(KeyCode::Char('j')));
        picker.key(control('n'));
        assert_eq!(
            picker.key(key(KeyCode::Enter)),
            ConversationPickerEffect::Open(2)
        );
        picker.key(key(KeyCode::Down));
        assert_eq!(picker.selection(), Some(2));
        picker.key(key(KeyCode::Char('k')));
        assert_eq!(picker.selection(), Some(1));
        picker.key(control('p'));
        assert_eq!(picker.selection(), Some(0));
        let lines = texts(&picker.frame(40, 12, DisplayProfile::default()));
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.trim_start_matches(['❯', ' ']) == "Same")
                .count(),
            3
        );
        assert!(lines.iter().any(|line| line.contains("3m ago")));
    }

    #[test]
    fn search_kill_yank_and_undo_preserve_the_selected_saved_record() {
        let mut picker = picker(vec![
            entry("alpha beta", &["1m ago"]),
            entry("alpha beta", &["2d ago"]),
        ]);
        picker.key(key(KeyCode::Char('j')));
        picker.key(key(KeyCode::Char('/')));
        type_text(&mut picker, "alpha beta");
        assert_eq!(picker.selection(), Some(1));

        picker.key(control('u'));
        assert_eq!(picker.query(), "");
        picker.key(control('y'));
        assert_eq!(picker.query(), "alpha beta");
        assert_eq!(picker.selection(), Some(1));

        picker.key(control('_'));
        assert_eq!(picker.query(), "");
        picker.key(control('7'));
        assert_eq!(picker.query(), "alpha beta");
        assert_eq!(
            picker.key(key(KeyCode::Enter)),
            ConversationPickerEffect::Open(1)
        );
    }

    #[test]
    fn search_word_motion_and_deletion_follow_ordinary_editing() {
        let mut picker = picker(vec![entry("alpha beta", &[])]);
        picker.key(key(KeyCode::Char('/')));
        type_text(&mut picker, "alpha beta");
        picker.key(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL));
        assert_eq!(
            picker.frame(40, 8, DisplayProfile::default()).cursor,
            Some((1, 8))
        );
        picker.key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL));
        assert_eq!(
            picker.frame(40, 8, DisplayProfile::default()).cursor,
            Some((1, 12))
        );

        picker.key(control('a'));
        picker.key(control('d'));
        assert_eq!(picker.query(), "lpha beta");
        picker.key(key(KeyCode::End));
        picker.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT));
        assert_eq!(picker.query(), "lpha ");
        picker.key(control('y'));
        assert_eq!(picker.query(), "lpha beta");
        picker.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::CONTROL));
        assert_eq!(picker.query(), "lpha ");
        picker.key(control('y'));
        assert_eq!(picker.query(), "lpha beta");
    }

    #[test]
    fn search_word_kills_yank_history_and_undo_keep_query_text_recoverable() {
        let mut picker = picker(vec![entry("alpha beta", &[])]);
        picker.key(key(KeyCode::Char('/')));
        type_text(&mut picker, "alpha beta");
        picker.key(control('w'));
        assert_eq!(picker.query(), "alpha ");
        picker.key(control('u'));
        assert_eq!(picker.query(), "");
        picker.key(control('y'));
        assert_eq!(picker.query(), "alpha ");
        picker.key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::ALT));
        assert_eq!(picker.query(), "beta");
        picker.key(KeyEvent::new(
            KeyCode::Char('-'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ));
        assert_eq!(picker.query(), "alpha ");
    }

    #[test]
    fn search_undo_restores_paste_and_external_editor_replacements() {
        let mut picker = picker(vec![entry("jk notes", &[])]);
        picker.paste("jk");
        picker.paste("notes");
        assert_eq!(picker.query(), "jknotes");
        picker.key(control('_'));
        assert_eq!(picker.query(), "jk");
        picker.replace_query_text("alpha beta");
        picker.key(control('_'));
        assert_eq!(picker.query(), "jk");
        assert_eq!(picker.selection(), Some(0));
    }

    #[test]
    fn search_focus_types_literal_j_k_and_list_focus_navigates_with_them() {
        let mut picker = picker(vec![
            entry("alpha", &[]),
            entry("jk notes", &[]),
            entry("kilo", &[]),
        ]);
        picker.key(key(KeyCode::Char('j')));
        assert_eq!(picker.selection(), Some(1));
        assert_eq!(picker.focus(), ConversationPickerFocus::List);
        picker.key(key(KeyCode::Char('/')));
        type_text(&mut picker, "jk");
        assert_eq!(picker.query(), "jk");
        assert_eq!(picker.focus(), ConversationPickerFocus::Search);
        assert_eq!(picker.selection(), Some(1));
        picker.key(key(KeyCode::Down));
        assert_eq!(picker.focus(), ConversationPickerFocus::List);
        picker.key(key(KeyCode::Char('k')));
        assert_eq!(picker.query(), "jk");
        assert_eq!(picker.selection(), Some(1));
    }

    #[test]
    fn typing_from_the_list_starts_a_search_and_up_from_the_top_focuses_it() {
        let mut picker = picker(vec![entry("alpha", &[]), entry("beta", &[])]);
        picker.key(key(KeyCode::Char('b')));
        assert_eq!(picker.focus(), ConversationPickerFocus::Search);
        assert_eq!(picker.query(), "b");
        assert_eq!(picker.selection(), Some(1));
        picker.key(key(KeyCode::Esc));
        assert_eq!(picker.query(), "");
        assert_eq!(picker.selection(), Some(1));
        picker.key(key(KeyCode::Esc));
        assert_eq!(picker.focus(), ConversationPickerFocus::List);
        picker.key(key(KeyCode::Up));
        assert_eq!(picker.selection(), Some(0));
        picker.key(key(KeyCode::Up));
        assert_eq!(picker.focus(), ConversationPickerFocus::Search);
        assert_eq!(
            picker.key(key(KeyCode::Esc)),
            ConversationPickerEffect::None
        );
        assert_eq!(
            picker.key(key(KeyCode::Esc)),
            ConversationPickerEffect::Cancel
        );
    }

    #[test]
    fn empty_results_cannot_open_and_clearing_returns_to_the_remembered_entry() {
        let mut picker = picker(vec![
            entry("first", &[]),
            entry("second", &[]),
            entry("third", &[]),
        ]);
        picker.key(key(KeyCode::Down));
        picker.key(key(KeyCode::Down));
        picker.key(key(KeyCode::Char('/')));
        type_text(&mut picker, "zzz");
        assert_eq!(picker.selection(), None);
        assert_eq!(
            picker.key(key(KeyCode::Enter)),
            ConversationPickerEffect::None
        );
        picker.key(key(KeyCode::Down));
        assert_eq!(
            picker.key(key(KeyCode::Enter)),
            ConversationPickerEffect::None
        );
        let lines = texts(&picker.frame(60, 8, DisplayProfile::default()));
        assert!(lines[0].ends_with("no matches"), "{lines:?}");
        assert!(
            lines
                .iter()
                .any(|line| line == "No conversations match “zzz”")
        );
        picker.key(key(KeyCode::Char('/')));
        picker.key(control('u'));
        assert_eq!(picker.selection(), Some(2));
        type_text(&mut picker, "c");
        assert_eq!(picker.selection(), Some(1));
        picker.key(key(KeyCode::Backspace));
        assert_eq!(picker.selection(), Some(1));
    }

    #[test]
    fn scrolling_keeps_the_selection_visible_within_constrained_heights() {
        let entries = (0..30)
            .map(|index| {
                entry(
                    &format!("conversation {index:02}"),
                    &[&format!("{index}m ago")],
                )
            })
            .collect();
        let mut picker = picker(entries);
        for _ in 0..17 {
            picker.key(key(KeyCode::Char('j')));
        }
        for height in 1..=16 {
            let frame = picker.frame(32, height, DisplayProfile::default());
            let lines = texts(&frame);
            assert_eq!(lines.len(), height);
            assert!(lines.iter().all(|line| display_width(line) <= 32));
            if height >= 2 {
                assert!(
                    lines.iter().any(|line| line == "❯ conversation 17"),
                    "height {height}: {lines:?}"
                );
            }
            if height >= TITLE_MIN_HEIGHT {
                assert!(lines[0].ends_with("18 of 30"), "{lines:?}");
            }
            if height >= HINT_MIN_HEIGHT {
                assert!(lines[height - 1].contains("esc cancel"), "{lines:?}");
            }
        }
        let tall = texts(&picker.frame(32, 16, DisplayProfile::default()));
        let selected = tall
            .iter()
            .position(|line| line == "❯ conversation 17")
            .unwrap();
        assert_eq!(tall[selected + 1], "  17m ago");
    }

    #[test]
    fn the_search_cursor_tracks_wide_text_and_the_hint_follows_focus() {
        let mut picker = picker(vec![entry("报告", &[])]);
        let list = picker.frame(80, 8, DisplayProfile::default());
        assert!(list.cursor.is_none());
        let list_lines = texts(&list);
        assert_eq!(
            list_lines[4], "",
            "one blank row separates a short list from its hint"
        );
        assert_eq!(
            hint_row(&list_lines),
            "enter open · type to search · esc cancel"
        );
        assert_eq!(
            hint_row(&texts(&picker.frame(32, 8, DisplayProfile::default()))),
            "enter open · esc cancel"
        );
        picker.paste("报\n告");
        let frame = picker.frame(40, 8, DisplayProfile::default());
        let lines = texts(&frame);
        assert_eq!(lines[1], "⌕ 报 告");
        assert_eq!(frame.cursor, Some((1, 7)));
        assert!(hint_row(&lines).contains("esc clear search"), "{lines:?}");
        assert_eq!(picker.selection(), None);
        picker.key(key(KeyCode::Left));
        picker.key(key(KeyCode::Left));
        picker.key(key(KeyCode::Delete));
        assert_eq!(picker.query(), "报告");
        assert_eq!(picker.selection(), Some(0));
        let narrow = picker.frame(4, 3, DisplayProfile::default());
        assert!(texts(&narrow).iter().all(|line| display_width(line) <= 4));
        assert!(narrow.cursor.is_some_and(|(_, column)| column < 4));
    }

    #[test]
    fn control_g_edits_the_search_query_from_either_focus() {
        let mut picker = picker(vec![entry("alpha", &[]), entry("beta", &[])]);
        assert_eq!(
            picker.key(control('g')),
            ConversationPickerEffect::EditQuery
        );
        assert_eq!(picker.focus(), ConversationPickerFocus::Search);
        picker.replace_query_text("be\nta\t");
        assert_eq!(picker.query(), "be ta");
        picker.replace_query_text("beta");
        assert_eq!(picker.selection(), Some(1));
        picker.set_notice("External editor failed: gone".into());
        let lines = texts(&picker.frame(80, 8, DisplayProfile::default()));
        assert_eq!(hint_row(&lines), "External editor failed: gone");
        picker.key(key(KeyCode::End));
        let lines = texts(&picker.frame(80, 8, DisplayProfile::default()));
        assert_eq!(
            hint_row(&lines),
            "enter open · ctrl+g editor · esc clear search"
        );
    }

    #[test]
    fn missing_metadata_is_omitted_rather_than_invented() {
        let entry = ConversationEntry::new("Title", &["just now".into(), " ".into()], &[]);
        assert_eq!(entry.plain_row(), "Title · just now");
        assert_eq!(ConversationEntry::new("Only", &[], &[]).plain_row(), "Only");
    }
}
