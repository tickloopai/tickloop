use crossterm::event::KeyCode;

const MAX_INPUT_CHARACTERS: usize = 4096;
const MAX_UNDO_STEPS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditOutcome {
    Changed,
    Submit,
    Cancel,
    None,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct VimInput {
    text: String,
    cursor: usize,
    insert: bool,
    undo: Vec<(String, usize)>,
    pending_undo: Option<(String, usize)>,
}

impl Default for VimInput {
    fn default() -> Self {
        Self::new(String::new())
    }
}

impl VimInput {
    pub fn new(text: String) -> Self {
        let text: String = text.chars().take(MAX_INPUT_CHARACTERS).collect();
        Self {
            cursor: text.chars().count(),
            text,
            insert: true,
            undo: Vec::new(),
            pending_undo: None,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn mode_label(&self) -> &'static str {
        if self.insert { "INSERT" } else { "NORMAL" }
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn viewport(
        &self,
        columns: usize,
        ambiguous_width: tickloop_ui::settings::AmbiguousWidth,
    ) -> (String, usize) {
        let characters: Vec<char> = self.text.chars().collect();
        let cursor = self.cursor.min(characters.len());
        let mut start = 0;
        let mut cursor_columns = tickloop_ui::format::width(
            &characters[..cursor].iter().collect::<String>(),
            ambiguous_width,
        );
        while start < cursor && cursor_columns >= columns.max(1) {
            cursor_columns = cursor_columns.saturating_sub(tickloop_ui::format::width(
                &characters[start].to_string(),
                ambiguous_width,
            ));
            start += 1;
        }
        (
            characters[start..].iter().collect(),
            cursor_columns.min(columns.saturating_sub(1)),
        )
    }

    pub fn handle(&mut self, key: KeyCode) -> EditOutcome {
        let mut chars: Vec<char> = self.text.chars().collect();
        self.cursor = self
            .cursor
            .min(chars.len().saturating_sub(usize::from(!self.insert)));
        let before = (self.text.clone(), self.cursor);
        let was_insert = self.insert;
        match key {
            KeyCode::Esc if self.insert => {
                self.insert = false;
                self.cursor = self
                    .cursor
                    .saturating_sub(1)
                    .min(chars.len().saturating_sub(1));
            }
            KeyCode::Esc => return EditOutcome::Cancel,
            KeyCode::Enter => return EditOutcome::Submit,
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => {
                self.cursor =
                    (self.cursor + 1).min(chars.len().saturating_sub(usize::from(!self.insert)))
            }
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = chars.len().saturating_sub(usize::from(!self.insert)),
            KeyCode::Backspace if self.insert && self.cursor > 0 => {
                self.cursor -= 1;
                chars.remove(self.cursor);
            }
            KeyCode::Delete if self.cursor < chars.len() => {
                chars.remove(self.cursor);
            }
            KeyCode::Char(character) if self.insert && !character.is_control() => {
                if chars.len() >= MAX_INPUT_CHARACTERS {
                    return EditOutcome::None;
                }
                chars.insert(self.cursor, character);
                self.cursor += 1;
            }
            KeyCode::Char('i') => self.insert = true,
            KeyCode::Char('a') => {
                self.cursor = (self.cursor + 1).min(chars.len());
                self.insert = true;
            }
            KeyCode::Char('I') => {
                self.cursor = 0;
                self.insert = true;
            }
            KeyCode::Char('A') => {
                self.cursor = chars.len();
                self.insert = true;
            }
            KeyCode::Char('h') => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Char('l') => {
                self.cursor = (self.cursor + 1).min(chars.len().saturating_sub(1))
            }
            KeyCode::Char('0') => self.cursor = 0,
            KeyCode::Char('$') => self.cursor = chars.len().saturating_sub(1),
            KeyCode::Char('w') => {
                while self.cursor < chars.len() && !chars[self.cursor].is_whitespace() {
                    self.cursor += 1;
                }
                while self.cursor < chars.len() && chars[self.cursor].is_whitespace() {
                    self.cursor += 1;
                }
                self.cursor = self.cursor.min(chars.len().saturating_sub(1));
            }
            KeyCode::Char('b') => {
                self.cursor = self.cursor.saturating_sub(1);
                while self.cursor > 0 && chars[self.cursor].is_whitespace() {
                    self.cursor -= 1;
                }
                while self.cursor > 0 && !chars[self.cursor - 1].is_whitespace() {
                    self.cursor -= 1;
                }
            }
            KeyCode::Char('x') if self.cursor < chars.len() => {
                chars.remove(self.cursor);
            }
            KeyCode::Char('X') if self.cursor > 0 => {
                self.cursor -= 1;
                chars.remove(self.cursor);
            }
            KeyCode::Char('u') => {
                if let Some((text, cursor)) = self.undo.pop() {
                    self.text = text;
                    self.cursor = cursor.min(self.text.chars().count().saturating_sub(1));
                }
                return EditOutcome::Changed;
            }
            _ => return EditOutcome::None,
        }
        self.text = chars.into_iter().collect();
        if self.text != before.0 {
            if was_insert {
                self.pending_undo.get_or_insert(before);
            } else {
                self.record_undo(before);
            }
        }
        if was_insert
            && !self.insert
            && let Some(checkpoint) = self.pending_undo.take()
        {
            self.record_undo(checkpoint);
        }
        self.cursor = self.cursor.min(
            self.text
                .chars()
                .count()
                .saturating_sub(usize::from(!self.insert)),
        );
        EditOutcome::Changed
    }

    fn record_undo(&mut self, checkpoint: (String, usize)) {
        if checkpoint.0 == self.text {
            return;
        }
        if self.undo.len() >= MAX_UNDO_STEPS {
            self.undo.drain(..self.undo.len() - MAX_UNDO_STEPS + 1);
        }
        self.undo.push(checkpoint);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undo_reverts_an_insert_session_and_viewport_keeps_unicode_cursor_visible() {
        let mut input = VimInput::new("甲乙".into());
        for character in "abc".chars() {
            input.handle(KeyCode::Char(character));
        }
        let (visible, cursor) = input.viewport(4, tickloop_ui::settings::AmbiguousWidth::Narrow);
        assert_eq!((visible.as_str(), cursor), ("abc", 3));
        input.handle(KeyCode::Esc);
        input.handle(KeyCode::Char('u'));
        assert_eq!(input.text(), "甲乙");
    }

    #[test]
    fn escape_leaves_insert_before_cancelling_and_unicode_edit_is_undoable() {
        let mut input = VimInput::new("财报".into());
        input.handle(KeyCode::Left);
        input.handle(KeyCode::Char('新'));
        assert_eq!(input.text(), "财新报");
        assert_eq!(input.handle(KeyCode::Esc), EditOutcome::Changed);
        assert_eq!(input.mode_label(), "NORMAL");
        input.handle(KeyCode::Char('u'));
        assert_eq!(input.text(), "财报");
        assert_eq!(input.handle(KeyCode::Esc), EditOutcome::Cancel);
    }

    #[test]
    fn insert_navigation_letters_remain_text_and_normal_movement_does_not_edit() {
        let mut input = VimInput::new(String::new());
        for key in ['j', 'k', 'q', '?'] {
            input.handle(KeyCode::Char(key));
        }
        input.handle(KeyCode::Esc);
        input.handle(KeyCode::Char('h'));
        input.handle(KeyCode::Char('x'));
        assert_eq!(input.text(), "jk?");
        input.handle(KeyCode::Char('u'));
        assert_eq!(input.text(), "jkq?");
    }
}
