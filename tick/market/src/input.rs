use crossterm::event::KeyCode;

const MAX_INPUT_CHARACTERS: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditOutcome {
    Changed,
    Submit,
    None,
}

/// A one-line editor with the ordinary editing keys of the Agent input (ADR 0010): typed
/// characters insert at the cursor, arrows and Home/End move, Backspace and Delete remove, Enter
/// submits. It has no modes, and Ctrl+C never reaches it: the runtime cancels the edit through
/// the view that owns it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextInput {
    text: String,
    cursor: usize,
}

impl TextInput {
    pub fn new(text: String) -> Self {
        let text: String = text.chars().take(MAX_INPUT_CHARACTERS).collect();
        Self {
            cursor: text.chars().count(),
            text,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
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
        match key {
            KeyCode::Enter => return EditOutcome::Submit,
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(chars.len()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = chars.len(),
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                chars.remove(self.cursor);
            }
            KeyCode::Delete if self.cursor < chars.len() => {
                chars.remove(self.cursor);
            }
            KeyCode::Char(character) if !character.is_control() => {
                if chars.len() >= MAX_INPUT_CHARACTERS {
                    return EditOutcome::None;
                }
                chars.insert(self.cursor, character);
                self.cursor += 1;
            }
            _ => return EditOutcome::None,
        }
        self.text = chars.into_iter().collect();
        EditOutcome::Changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_inserts_at_the_cursor_and_the_viewport_keeps_a_unicode_cursor_visible() {
        let mut input = TextInput::new("甲乙".into());
        for character in "abc".chars() {
            input.handle(KeyCode::Char(character));
        }
        let (visible, cursor) = input.viewport(4, tickloop_ui::settings::AmbiguousWidth::Narrow);
        assert_eq!((visible.as_str(), cursor), ("abc", 3));
        input.handle(KeyCode::Home);
        input.handle(KeyCode::Right);
        input.handle(KeyCode::Char('新'));
        assert_eq!(input.text(), "甲新乙abc");
        input.handle(KeyCode::End);
        input.handle(KeyCode::Left);
        input.handle(KeyCode::Delete);
        assert_eq!(input.text(), "甲新乙ab");
    }

    #[test]
    fn backspace_deletes_before_the_cursor_and_never_leaves_the_editor() {
        let mut input = TextInput::new("财报".into());
        input.handle(KeyCode::Left);
        assert_eq!(input.handle(KeyCode::Backspace), EditOutcome::Changed);
        assert_eq!(input.text(), "报");
        assert_eq!(input.handle(KeyCode::Backspace), EditOutcome::None);
        assert_eq!(input.text(), "报");
        assert_eq!(input.handle(KeyCode::Enter), EditOutcome::Submit);
    }

    #[test]
    fn navigation_letters_are_text_and_esc_switches_no_mode() {
        let mut input = TextInput::new(String::new());
        for key in ['j', 'k', 'q', '?', 'b'] {
            input.handle(KeyCode::Char(key));
        }
        assert_eq!(input.handle(KeyCode::Esc), EditOutcome::None);
        for key in ['h', 'x', 'u', 'i'] {
            input.handle(KeyCode::Char(key));
        }
        assert_eq!(input.text(), "jkq?bhxui");
    }
}
