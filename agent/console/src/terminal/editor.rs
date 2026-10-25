use std::{
    ops::Range,
    time::{Duration, Instant},
};

use unicode_width::UnicodeWidthChar;

/// Mirrors Claude Code `useInputBuffer` `debounceMs`: typing within this window
/// shares one undo snapshot instead of one per character.
const TYPED_UNDO_WINDOW: Duration = Duration::from_millis(1000);
/// Mirrors Claude Code `useInputBuffer` `maxBufferSize`.
const UNDO_HISTORY_LIMIT: usize = 50;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum EditorCommand {
    Insert(String),
    /// Bracketed paste; always its own undo snapshot, never coalesced with typing.
    InsertPasted(String),
    Newline,
    MoveLeft,
    MoveRight,
    MoveLineStart,
    MoveLineEnd,
    MoveWordBackward,
    MoveWordForward,
    MoveUp {
        wrap_width: usize,
    },
    MoveDown {
        wrap_width: usize,
    },
    DeleteBackward,
    DeleteForward,
    KillToLineStart,
    KillToLineEnd,
    KillToPreviousWhitespace,
    KillWordBackward,
    KillWordForward,
    Yank,
    YankPrevious,
    Undo,
    Clear,
    Stash,
    Submit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum EditorOutcome {
    Unchanged,
    Changed,
    Submitted(String),
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub(super) struct Editor {
    draft: String,
    cursor: usize,
    history: Vec<String>,
    history_position: Option<usize>,
    saved_draft: Option<Snapshot>,
    kill_ring: Vec<String>,
    undo_stack: Vec<Snapshot>,
    stash: Option<Snapshot>,
    last_yank: Option<Yank>,
    preferred_column: Option<usize>,
    /// Time source for undo coalescing; `None` reads the wall clock, tests pin
    /// it so they never sleep.
    #[serde(skip)]
    pinned_now: Option<Instant>,
    #[serde(skip)]
    typed_snapshot_at: Option<Instant>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct Snapshot {
    draft: String,
    cursor: usize,
}

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
struct Yank {
    start: usize,
    end: usize,
    ring_index: usize,
}

#[derive(Clone, Copy, Debug)]
struct VisualRow {
    start: usize,
    end: usize,
    soft_end: bool,
}

impl Editor {
    pub(super) fn valid_upgrade_state(&self) -> bool {
        let valid_snapshot = |snapshot: &Snapshot| snapshot.draft.is_char_boundary(snapshot.cursor);
        self.draft.is_char_boundary(self.cursor)
            && self
                .history_position
                .is_none_or(|position| position < self.history.len())
            && self.saved_draft.as_ref().is_none_or(valid_snapshot)
            && self.stash.as_ref().is_none_or(valid_snapshot)
            && self.undo_stack.iter().all(valid_snapshot)
            && self.last_yank.is_none_or(|yank| {
                yank.start <= yank.end
                    && self.draft.is_char_boundary(yank.start)
                    && self.draft.is_char_boundary(yank.end)
                    && yank.ring_index < self.kill_ring.len()
            })
    }

    pub(super) fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    fn with_history(history: Vec<String>) -> Self {
        Self {
            history,
            ..Self::default()
        }
    }

    pub(super) fn draft(&self) -> &str {
        &self.draft
    }

    pub(super) fn cursor(&self) -> usize {
        self.cursor
    }

    #[cfg(test)]
    fn has_stash(&self) -> bool {
        self.stash.is_some()
    }

    #[cfg(test)]
    fn pin_clock(&mut self) {
        self.pinned_now = Some(Instant::now());
    }

    #[cfg(test)]
    fn advance_clock(&mut self, elapsed: Duration) {
        self.pinned_now = Some(self.pinned_now.unwrap_or_else(Instant::now) + elapsed);
    }

    #[cfg(test)]
    fn undo_depth(&self) -> usize {
        self.undo_stack.len()
    }

    pub(super) fn is_browsing_history(&self) -> bool {
        self.history_position.is_some()
    }

    /// Replaces the Up-arrow list with persisted history, `history` ordered oldest first; not
    /// replaced while browsing.
    pub(super) fn replace_history(&mut self, history: Vec<String>) {
        if !self.is_browsing_history() {
            self.history = history;
        }
    }

    pub(super) fn forget_latest_history(&mut self, submitted: &str) {
        if self.history.last().is_some_and(|entry| entry == submitted) {
            self.history.pop();
        }
    }

    pub(super) fn is_on_first_visual_row(&self, wrap_width: usize) -> bool {
        let rows = visual_rows(&self.draft, wrap_width.max(1));
        visual_position(&self.draft, self.cursor, &rows).0 == 0
    }

    pub(super) fn replace_draft(&mut self, draft: String, cursor: usize) {
        let cursor = floor_char_boundary(&draft, cursor.min(draft.len()));
        self.remember_edit();
        self.draft = draft;
        self.cursor = cursor;
        self.leave_history();
        self.finish_non_yank();
    }

    pub(super) fn apply(&mut self, command: EditorCommand) -> EditorOutcome {
        self.apply_keeping_whole(command, &[])
    }

    /// Applies `command`; a kill that reaches into any of `whole_ranges` (pasted blocks such as
    /// `[Image #N]`) removes that range entirely, as Claude Code's `snapOutOfImageRef` does.
    pub(super) fn apply_keeping_whole(
        &mut self,
        command: EditorCommand,
        whole_ranges: &[Range<usize>],
    ) -> EditorOutcome {
        // Anything but plain typing ends the coalescing group, so the next
        // typed character starts a fresh undo snapshot.
        if !matches!(command, EditorCommand::Insert(_)) {
            self.typed_snapshot_at = None;
        }
        match command {
            EditorCommand::Insert(text) => self.insert_typed(&text),
            EditorCommand::InsertPasted(text) => self.insert(&text),
            EditorCommand::Newline => self.insert("\n"),
            EditorCommand::MoveLeft => {
                self.move_cursor(previous_boundary(&self.draft, self.cursor))
            }
            EditorCommand::MoveRight => self.move_cursor(next_boundary(&self.draft, self.cursor)),
            EditorCommand::MoveLineStart => self.move_cursor(line_start(&self.draft, self.cursor)),
            EditorCommand::MoveLineEnd => self.move_cursor(line_end(&self.draft, self.cursor)),
            EditorCommand::MoveWordBackward => {
                self.move_cursor(backward_word_boundary(&self.draft, self.cursor))
            }
            EditorCommand::MoveWordForward => {
                self.move_cursor(forward_word_boundary(&self.draft, self.cursor))
            }
            EditorCommand::MoveUp { wrap_width } => self.move_vertical(false, wrap_width),
            EditorCommand::MoveDown { wrap_width } => self.move_vertical(true, wrap_width),
            EditorCommand::DeleteBackward => self.delete(
                previous_boundary(&self.draft, self.cursor),
                self.cursor,
                false,
            ),
            EditorCommand::DeleteForward => {
                self.delete(self.cursor, next_boundary(&self.draft, self.cursor), false)
            }
            EditorCommand::KillToLineStart => {
                let mut start = line_start(&self.draft, self.cursor);
                if start == self.cursor && self.cursor > 0 {
                    start = line_start(&self.draft, self.cursor - 1);
                }
                // A folded block shows on one line, so its inner newlines are not line breaks.
                while let Some(whole) = whole_ranges
                    .iter()
                    .find(|whole| whole.contains(&start) && whole.start < start)
                {
                    start = line_start(&self.draft, whole.start);
                }
                self.kill(start..self.cursor, whole_ranges)
            }
            EditorCommand::KillToLineEnd => {
                let mut end = line_end(&self.draft, self.cursor);
                while let Some(whole) = whole_ranges
                    .iter()
                    .find(|whole| whole.start < end && end < whole.end)
                {
                    end = line_end(&self.draft, whole.end);
                }
                self.kill(self.cursor..end, whole_ranges)
            }
            EditorCommand::KillToPreviousWhitespace => self.kill(
                previous_whitespace_boundary(&self.draft, self.cursor)..self.cursor,
                whole_ranges,
            ),
            EditorCommand::KillWordBackward => self.kill(
                backward_word_boundary(&self.draft, self.cursor)..self.cursor,
                whole_ranges,
            ),
            EditorCommand::KillWordForward => self.kill(
                self.cursor..forward_word_boundary(&self.draft, self.cursor),
                whole_ranges,
            ),
            EditorCommand::Yank => self.yank(),
            EditorCommand::YankPrevious => self.yank_previous(),
            EditorCommand::Undo => self.undo(),
            EditorCommand::Clear => self.clear(),
            EditorCommand::Stash => self.stash(),
            EditorCommand::Submit => self.submit(),
        }
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            draft: self.draft.clone(),
            cursor: self.cursor,
        }
    }

    fn remember_edit(&mut self) {
        let snapshot = self.snapshot();
        push_bounded(&mut self.undo_stack, snapshot);
        self.typed_snapshot_at = None;
    }

    /// Snapshot for a typed character, skipped while the previous typed
    /// character is still inside [`TYPED_UNDO_WINDOW`].
    fn remember_typed_edit(&mut self) {
        let now = self.pinned_now.unwrap_or_else(Instant::now);
        if self
            .typed_snapshot_at
            .is_some_and(|at| now.duration_since(at) < TYPED_UNDO_WINDOW)
        {
            return;
        }
        self.remember_edit();
        self.typed_snapshot_at = Some(now);
    }

    fn insert(&mut self, text: &str) -> EditorOutcome {
        self.insert_text(text, false)
    }

    fn insert_typed(&mut self, text: &str) -> EditorOutcome {
        self.insert_text(text, true)
    }

    fn insert_text(&mut self, text: &str, typed: bool) -> EditorOutcome {
        if text.is_empty() {
            return EditorOutcome::Unchanged;
        }
        if typed {
            self.remember_typed_edit();
        } else {
            self.remember_edit();
        }
        self.draft.insert_str(self.cursor, text);
        self.cursor += text.len();
        self.leave_history();
        self.finish_non_yank();
        EditorOutcome::Changed
    }

    fn move_cursor(&mut self, cursor: usize) -> EditorOutcome {
        self.preferred_column = None;
        self.last_yank = None;
        if cursor == self.cursor {
            EditorOutcome::Unchanged
        } else {
            self.cursor = cursor;
            EditorOutcome::Changed
        }
    }

    fn move_vertical(&mut self, down: bool, wrap_width: usize) -> EditorOutcome {
        let rows = visual_rows(&self.draft, wrap_width.max(1));
        let (row_index, column) = visual_position(&self.draft, self.cursor, &rows);
        let target_row = if down {
            row_index.checked_add(1).filter(|index| *index < rows.len())
        } else {
            row_index.checked_sub(1)
        };
        if let Some(target_row) = target_row {
            let preferred_column = self.preferred_column.unwrap_or(column);
            self.cursor = cursor_at_column(&self.draft, rows[target_row], preferred_column);
            self.preferred_column = Some(preferred_column);
            self.last_yank = None;
            EditorOutcome::Changed
        } else {
            self.preferred_column = None;
            self.last_yank = None;
            if down {
                self.next_history()
            } else {
                self.previous_history()
            }
        }
    }

    fn kill(&mut self, mut range: Range<usize>, whole_ranges: &[Range<usize>]) -> EditorOutcome {
        for whole in whole_ranges {
            if whole.start < range.end && range.start < whole.end {
                range.start = range.start.min(whole.start);
                range.end = range.end.max(whole.end);
            }
        }
        self.delete(range.start, range.end, true)
    }

    fn delete(&mut self, start: usize, end: usize, save_kill: bool) -> EditorOutcome {
        if start == end {
            self.finish_non_yank();
            return EditorOutcome::Unchanged;
        }
        self.remember_edit();
        let deleted = self.draft[start..end].to_owned();
        self.draft.replace_range(start..end, "");
        self.cursor = start;
        if save_kill {
            self.kill_ring.insert(0, deleted);
        }
        self.leave_history();
        self.finish_non_yank();
        EditorOutcome::Changed
    }

    fn yank(&mut self) -> EditorOutcome {
        let Some(text) = self.kill_ring.first().cloned() else {
            self.finish_non_yank();
            return EditorOutcome::Unchanged;
        };
        self.remember_edit();
        let start = self.cursor;
        self.draft.insert_str(start, &text);
        self.cursor += text.len();
        self.last_yank = Some(Yank {
            start,
            end: self.cursor,
            ring_index: 0,
        });
        self.preferred_column = None;
        self.leave_history();
        EditorOutcome::Changed
    }

    fn yank_previous(&mut self) -> EditorOutcome {
        let Some(yank) = self.last_yank else {
            return EditorOutcome::Unchanged;
        };
        if self.kill_ring.len() < 2 {
            return EditorOutcome::Unchanged;
        }
        let ring_index = (yank.ring_index + 1) % self.kill_ring.len();
        let replacement = self.kill_ring[ring_index].clone();
        self.remember_edit();
        self.draft.replace_range(yank.start..yank.end, &replacement);
        self.cursor = yank.start + replacement.len();
        self.last_yank = Some(Yank {
            start: yank.start,
            end: self.cursor,
            ring_index,
        });
        self.preferred_column = None;
        EditorOutcome::Changed
    }

    fn undo(&mut self) -> EditorOutcome {
        let Some(snapshot) = self.undo_stack.pop() else {
            self.finish_non_yank();
            return EditorOutcome::Unchanged;
        };
        self.draft = snapshot.draft;
        self.cursor = snapshot.cursor;
        self.leave_history();
        self.finish_non_yank();
        EditorOutcome::Changed
    }

    fn clear(&mut self) -> EditorOutcome {
        if self.draft.is_empty() {
            self.finish_non_yank();
            return EditorOutcome::Unchanged;
        }
        self.remember_edit();
        self.draft.clear();
        self.cursor = 0;
        self.leave_history();
        self.finish_non_yank();
        EditorOutcome::Changed
    }

    fn stash(&mut self) -> EditorOutcome {
        if self.draft.is_empty() {
            let Some(snapshot) = self.stash.take() else {
                self.finish_non_yank();
                return EditorOutcome::Unchanged;
            };
            self.draft = snapshot.draft;
            self.cursor = snapshot.cursor;
        } else {
            self.stash = Some(self.snapshot());
            self.draft.clear();
            self.cursor = 0;
        }
        self.undo_stack.clear();
        self.leave_history();
        self.finish_non_yank();
        EditorOutcome::Changed
    }

    fn submit(&mut self) -> EditorOutcome {
        let submitted = std::mem::take(&mut self.draft);
        if !submitted.is_empty() && self.history.last() != Some(&submitted) {
            self.history.push(submitted.clone());
        }
        self.cursor = 0;
        self.history_position = None;
        self.saved_draft = None;
        self.undo_stack.clear();
        self.finish_non_yank();
        EditorOutcome::Submitted(submitted)
    }

    fn previous_history(&mut self) -> EditorOutcome {
        if self.history.is_empty() {
            return EditorOutcome::Unchanged;
        }
        let index = match self.history_position {
            Some(index) => index.saturating_sub(1),
            None => {
                self.saved_draft = Some(self.snapshot());
                self.history.len() - 1
            }
        };
        if self.history_position == Some(index) {
            return EditorOutcome::Unchanged;
        }
        self.history_position = Some(index);
        self.draft.clone_from(&self.history[index]);
        self.cursor = self.draft.len();
        self.undo_stack.clear();
        EditorOutcome::Changed
    }

    fn next_history(&mut self) -> EditorOutcome {
        let Some(index) = self.history_position else {
            return EditorOutcome::Unchanged;
        };
        if index + 1 < self.history.len() {
            self.history_position = Some(index + 1);
            self.draft.clone_from(&self.history[index + 1]);
            self.cursor = self.draft.len();
        } else {
            self.history_position = None;
            let saved = self.saved_draft.take().unwrap_or(Snapshot {
                draft: String::new(),
                cursor: 0,
            });
            self.draft = saved.draft;
            self.cursor = saved.cursor;
        }
        self.undo_stack.clear();
        EditorOutcome::Changed
    }

    fn leave_history(&mut self) {
        self.history_position = None;
        self.saved_draft = None;
    }

    fn finish_non_yank(&mut self) {
        self.last_yank = None;
        self.preferred_column = None;
    }
}

fn push_bounded(stack: &mut Vec<Snapshot>, snapshot: Snapshot) {
    if stack.len() >= UNDO_HISTORY_LIMIT {
        stack.drain(..stack.len() - UNDO_HISTORY_LIMIT + 1);
    }
    stack.push(snapshot);
}

fn line_start(text: &str, cursor: usize) -> usize {
    text[..cursor].rfind('\n').map_or(0, |index| index + 1)
}

fn line_end(text: &str, cursor: usize) -> usize {
    text[cursor..]
        .find('\n')
        .map_or(text.len(), |index| cursor + index)
}

fn previous_boundary(text: &str, cursor: usize) -> usize {
    text[..cursor]
        .char_indices()
        .next_back()
        .map_or(0, |(index, _)| index)
}

fn next_boundary(text: &str, cursor: usize) -> usize {
    text[cursor..]
        .char_indices()
        .nth(1)
        .map_or(text.len(), |(index, _)| cursor + index)
}

fn floor_char_boundary(text: &str, mut cursor: usize) -> usize {
    while !text.is_char_boundary(cursor) {
        cursor -= 1;
    }
    cursor
}

fn backward_word_boundary(text: &str, mut cursor: usize) -> usize {
    while cursor > 0 {
        let previous = previous_boundary(text, cursor);
        if text[previous..cursor]
            .chars()
            .next()
            .is_some_and(char::is_alphanumeric)
        {
            break;
        }
        cursor = previous;
    }
    while cursor > 0 {
        let previous = previous_boundary(text, cursor);
        if !text[previous..cursor]
            .chars()
            .next()
            .is_some_and(char::is_alphanumeric)
        {
            break;
        }
        cursor = previous;
    }
    cursor
}

fn forward_word_boundary(text: &str, mut cursor: usize) -> usize {
    while cursor < text.len() {
        let next = next_boundary(text, cursor);
        if text[cursor..next]
            .chars()
            .next()
            .is_some_and(char::is_alphanumeric)
        {
            break;
        }
        cursor = next;
    }
    while cursor < text.len() {
        let next = next_boundary(text, cursor);
        if !text[cursor..next]
            .chars()
            .next()
            .is_some_and(char::is_alphanumeric)
        {
            break;
        }
        cursor = next;
    }
    cursor
}

fn previous_whitespace_boundary(text: &str, mut cursor: usize) -> usize {
    while cursor > 0 {
        let previous = previous_boundary(text, cursor);
        if !text[previous..cursor]
            .chars()
            .next()
            .is_some_and(char::is_whitespace)
        {
            break;
        }
        cursor = previous;
    }
    while cursor > 0 {
        let previous = previous_boundary(text, cursor);
        if text[previous..cursor]
            .chars()
            .next()
            .is_some_and(char::is_whitespace)
        {
            break;
        }
        cursor = previous;
    }
    cursor
}

fn visual_rows(text: &str, width: usize) -> Vec<VisualRow> {
    let mut rows = Vec::new();
    let mut start = 0;
    let mut column = 0;
    for (index, character) in text.char_indices() {
        if character == '\n' {
            rows.push(VisualRow {
                start,
                end: index,
                soft_end: false,
            });
            start = index + character.len_utf8();
            column = 0;
            continue;
        }
        let character_width = display_width(character);
        if column > 0 && column + character_width > width {
            rows.push(VisualRow {
                start,
                end: index,
                soft_end: true,
            });
            start = index;
            column = 0;
        }
        column += character_width;
    }
    rows.push(VisualRow {
        start,
        end: text.len(),
        soft_end: false,
    });
    rows
}

fn visual_position(text: &str, cursor: usize, rows: &[VisualRow]) -> (usize, usize) {
    let row_index = rows
        .iter()
        .enumerate()
        .rev()
        .find(|(_, row)| row.start <= cursor)
        .map_or(0, |(index, _)| index);
    let column = text[rows[row_index].start..cursor]
        .chars()
        .map(display_width)
        .sum();
    (row_index, column)
}

fn cursor_at_column(text: &str, row: VisualRow, desired_column: usize) -> usize {
    let mut cursor = row.start;
    let mut column = 0;
    for (offset, character) in text[row.start..row.end].char_indices() {
        let character_width = display_width(character);
        if column + character_width > desired_column {
            break;
        }
        column += character_width;
        cursor = row.start + offset + character.len_utf8();
    }
    if row.soft_end && cursor == row.end {
        previous_boundary(text, cursor)
    } else {
        cursor
    }
}

fn display_width(character: char) -> usize {
    match character {
        '\t' => 1,
        character => character.width().unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(editor: &mut Editor, command: EditorCommand) {
        let _ = editor.apply(command);
    }

    #[test]
    fn utf8_editing_and_logical_line_boundaries_are_safe() {
        let mut editor = Editor::new();
        apply(
            &mut editor,
            EditorCommand::Insert("第一行\n目录中文".into()),
        );
        apply(&mut editor, EditorCommand::MoveLeft);
        apply(&mut editor, EditorCommand::DeleteBackward);
        apply(&mut editor, EditorCommand::Insert("新".into()));
        assert_eq!(editor.draft(), "第一行\n目录新文");
        apply(&mut editor, EditorCommand::MoveLineStart);
        assert_eq!(editor.cursor(), "第一行\n".len());
        apply(&mut editor, EditorCommand::MoveLineEnd);
        assert_eq!(editor.cursor(), editor.draft().len());
    }

    #[test]
    fn insertion_preserves_bracketed_paste_text_exactly() {
        let mut editor = Editor::new();
        let paste = "fn main() {\r\n\tprintln!(\"你好\");\u{1b}\n}";
        apply(&mut editor, EditorCommand::Insert(paste.into()));
        assert_eq!(editor.draft(), paste);
    }

    #[test]
    fn up_and_down_move_by_visual_row_then_cross_the_history_boundary() {
        let mut editor = Editor::with_history(vec!["older".into()]);
        apply(&mut editor, EditorCommand::Insert("ab中文cd\nlast".into()));
        apply(&mut editor, EditorCommand::MoveLineStart);
        apply(&mut editor, EditorCommand::MoveUp { wrap_width: 5 });
        assert_eq!(editor.cursor(), "ab中".len());
        apply(&mut editor, EditorCommand::MoveUp { wrap_width: 5 });
        assert_eq!(editor.cursor(), 0);
        apply(&mut editor, EditorCommand::MoveUp { wrap_width: 5 });
        assert_eq!(editor.draft(), "older");
        apply(&mut editor, EditorCommand::MoveDown { wrap_width: 5 });
        assert_eq!(editor.draft(), "ab中文cd\nlast");
    }

    #[test]
    fn history_restores_the_original_draft_and_cursor() {
        let mut editor = Editor::new();
        apply(&mut editor, EditorCommand::Insert("first".into()));
        assert_eq!(
            editor.apply(EditorCommand::Submit),
            EditorOutcome::Submitted("first".into())
        );
        apply(&mut editor, EditorCommand::Insert("second".into()));
        assert_eq!(
            editor.apply(EditorCommand::Submit),
            EditorOutcome::Submitted("second".into())
        );
        apply(&mut editor, EditorCommand::Insert("未完成草稿".into()));
        apply(&mut editor, EditorCommand::MoveLeft);
        let saved_cursor = editor.cursor();
        apply(&mut editor, EditorCommand::MoveUp { wrap_width: 80 });
        assert_eq!(editor.draft(), "second");
        apply(&mut editor, EditorCommand::MoveUp { wrap_width: 80 });
        assert_eq!(editor.draft(), "first");
        apply(&mut editor, EditorCommand::MoveDown { wrap_width: 80 });
        apply(&mut editor, EditorCommand::MoveDown { wrap_width: 80 });
        assert_eq!(editor.draft(), "未完成草稿");
        assert_eq!(editor.cursor(), saved_cursor);
    }

    #[test]
    fn kill_yank_word_moves_and_yank_history_follow_documented_boundaries() {
        let mut editor = Editor::new();
        apply(
            &mut editor,
            EditorCommand::Insert("src/utils/foo.ts next".into()),
        );
        apply(&mut editor, EditorCommand::MoveWordBackward);
        assert_eq!(&editor.draft()[editor.cursor()..], "next");
        apply(&mut editor, EditorCommand::MoveWordBackward);
        assert_eq!(&editor.draft()[editor.cursor()..], "ts next");
        apply(&mut editor, EditorCommand::MoveWordForward);
        assert_eq!(&editor.draft()[..editor.cursor()], "src/utils/foo.ts");
        apply(&mut editor, EditorCommand::MoveLineEnd);
        apply(&mut editor, EditorCommand::KillToPreviousWhitespace);
        assert_eq!(editor.draft(), "src/utils/foo.ts ");
        apply(&mut editor, EditorCommand::MoveLineStart);
        apply(&mut editor, EditorCommand::KillWordForward);
        assert_eq!(editor.draft(), "/utils/foo.ts ");
        apply(&mut editor, EditorCommand::Yank);
        assert_eq!(editor.draft(), "src/utils/foo.ts ");
        apply(&mut editor, EditorCommand::YankPrevious);
        assert_eq!(editor.draft(), "next/utils/foo.ts ");
    }

    #[test]
    fn repeated_line_kill_crosses_newlines_and_undo_restores_text_and_cursor() {
        let mut editor = Editor::new();
        apply(&mut editor, EditorCommand::Insert("one\ntwo".into()));
        apply(&mut editor, EditorCommand::KillToLineStart);
        assert_eq!(editor.draft(), "one\n");
        apply(&mut editor, EditorCommand::KillToLineStart);
        assert_eq!(editor.draft(), "");
        apply(&mut editor, EditorCommand::Undo);
        assert_eq!(editor.draft(), "one\n");
        assert_eq!(editor.cursor(), "one\n".len());
        apply(&mut editor, EditorCommand::Undo);
        assert_eq!(editor.draft(), "one\ntwo");
        assert_eq!(editor.cursor(), editor.draft().len());
    }

    fn typed_editor() -> Editor {
        let mut editor = Editor::new();
        editor.pin_clock();
        editor
    }

    fn type_keys(editor: &mut Editor, keys: &str) {
        for character in keys.chars() {
            apply(editor, EditorCommand::Insert(character.to_string()));
        }
    }

    #[test]
    fn typing_inside_the_coalescing_window_undoes_as_one_group() {
        let mut editor = typed_editor();
        type_keys(&mut editor, "hello");
        editor.advance_clock(TYPED_UNDO_WINDOW / 2);
        type_keys(&mut editor, " world");
        assert_eq!(editor.draft(), "hello world");
        assert_eq!(editor.undo_depth(), 1);

        apply(&mut editor, EditorCommand::Undo);
        assert!(editor.draft().is_empty());
    }

    #[test]
    fn typing_after_the_coalescing_window_undoes_in_two_groups() {
        let mut editor = typed_editor();
        type_keys(&mut editor, "hello");
        editor.advance_clock(TYPED_UNDO_WINDOW);
        type_keys(&mut editor, " world");
        assert_eq!(editor.undo_depth(), 2);

        apply(&mut editor, EditorCommand::Undo);
        assert_eq!(editor.draft(), "hello");
        apply(&mut editor, EditorCommand::Undo);
        assert!(editor.draft().is_empty());
    }

    #[test]
    fn an_edit_between_keystrokes_starts_a_new_undo_group() {
        let mut editor = typed_editor();
        type_keys(&mut editor, "hello");
        apply(&mut editor, EditorCommand::DeleteBackward);
        type_keys(&mut editor, "p");
        assert_eq!(editor.draft(), "hellp");

        apply(&mut editor, EditorCommand::Undo);
        assert_eq!(editor.draft(), "hell");
        apply(&mut editor, EditorCommand::Undo);
        assert_eq!(editor.draft(), "hello");
        apply(&mut editor, EditorCommand::Undo);
        assert!(editor.draft().is_empty());
    }

    #[test]
    fn a_paste_between_keystrokes_is_its_own_undo_group() {
        let mut editor = typed_editor();
        type_keys(&mut editor, "ab");
        apply(&mut editor, EditorCommand::InsertPasted("<pasted>".into()));
        type_keys(&mut editor, "cd");
        assert_eq!(editor.draft(), "ab<pasted>cd");

        apply(&mut editor, EditorCommand::Undo);
        assert_eq!(editor.draft(), "ab<pasted>");
        apply(&mut editor, EditorCommand::Undo);
        assert_eq!(editor.draft(), "ab");
        apply(&mut editor, EditorCommand::Undo);
        assert!(editor.draft().is_empty());
    }

    #[test]
    fn the_undo_history_drops_the_oldest_snapshot_past_the_limit() {
        let mut editor = typed_editor();
        for index in 0..UNDO_HISTORY_LIMIT + 10 {
            apply(
                &mut editor,
                EditorCommand::InsertPasted(format!("{index} ")),
            );
        }
        assert_eq!(editor.undo_depth(), UNDO_HISTORY_LIMIT);

        for _ in 0..UNDO_HISTORY_LIMIT {
            apply(&mut editor, EditorCommand::Undo);
        }
        assert_eq!(editor.undo_depth(), 0);
        // The ten oldest snapshots were dropped, so the first ten insertions stay.
        assert_eq!(editor.draft(), "0 1 2 3 4 5 6 7 8 9 ");
        apply(&mut editor, EditorCommand::Undo);
        assert_eq!(editor.draft(), "0 1 2 3 4 5 6 7 8 9 ");
    }

    #[test]
    fn stash_round_trips_text_and_cursor() {
        let mut editor = Editor::new();
        apply(&mut editor, EditorCommand::Insert("暂存 draft".into()));
        apply(&mut editor, EditorCommand::MoveWordBackward);
        let cursor = editor.cursor();
        apply(&mut editor, EditorCommand::Stash);
        assert!(editor.draft().is_empty());
        assert!(editor.has_stash());
        apply(&mut editor, EditorCommand::Stash);
        assert_eq!(editor.draft(), "暂存 draft");
        assert_eq!(editor.cursor(), cursor);
        assert!(!editor.has_stash());
    }
}
