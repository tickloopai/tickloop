use std::ops::Range;

use unicode_width::UnicodeWidthChar;

use super::style::{Role, RoleLine};

const CONTINUATION_PREFIX: &str = "  ";
const PREFIX_COLUMNS: usize = 2;
const TAB_COLUMNS: usize = 4;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InputFrame {
    pub(crate) lines: Vec<String>,
    pub(super) styled_lines: Vec<RoleLine>,
    pub(crate) cursor_row: usize,
    pub(crate) cursor_column: usize,
    pub(crate) first_visual_row: usize,
    pub(crate) total_visual_rows: usize,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct InputView;

impl InputView {
    #[cfg(test)]
    pub(crate) fn frame(
        draft: &str,
        cursor: usize,
        terminal_width: usize,
        max_rows: usize,
    ) -> InputFrame {
        Self::frame_with_prefix(draft, cursor, terminal_width, max_rows, "> ", &[])
    }

    pub(crate) fn frame_with_prompt(
        draft: &str,
        cursor: usize,
        terminal_width: usize,
        max_rows: usize,
        prompt: &str,
    ) -> InputFrame {
        Self::frame_with_prompt_and_marks(draft, cursor, terminal_width, max_rows, prompt, &[])
    }

    pub(super) fn frame_with_prompt_and_marks(
        draft: &str,
        cursor: usize,
        terminal_width: usize,
        max_rows: usize,
        prompt: &str,
        marks: &[(Range<usize>, Role)],
    ) -> InputFrame {
        let first_line_prefix = format!("{prompt}\u{a0}");
        Self::frame_with_prefix(
            draft,
            cursor,
            terminal_width,
            max_rows,
            &first_line_prefix,
            marks,
        )
    }

    fn frame_with_prefix(
        draft: &str,
        cursor: usize,
        terminal_width: usize,
        max_rows: usize,
        first_line_prefix: &str,
        marks: &[(Range<usize>, Role)],
    ) -> InputFrame {
        if terminal_width == 0 || max_rows == 0 {
            return InputFrame {
                lines: Vec::new(),
                styled_lines: Vec::new(),
                cursor_row: 0,
                cursor_column: 0,
                first_visual_row: 0,
                total_visual_rows: 0,
            };
        }

        let cursor = floor_char_boundary(draft, cursor.min(draft.len()));
        let prefix_columns = PREFIX_COLUMNS.min(terminal_width.saturating_sub(1));
        let content_width = terminal_width.saturating_sub(prefix_columns).max(1);
        let layout = layout(draft, cursor, content_width, marks);
        let first_visual_row = layout.cursor_row.saturating_add(1).saturating_sub(max_rows);
        let last_visual_row = first_visual_row
            .saturating_add(max_rows)
            .min(layout.rows.len());
        let styled_lines = layout.rows[first_visual_row..last_visual_row]
            .iter()
            .enumerate()
            .map(|(visible_row, content)| {
                let absolute_row = first_visual_row + visible_row;
                let prefix = if absolute_row == 0 {
                    first_line_prefix
                } else {
                    CONTINUATION_PREFIX
                };
                let mut line = RoleLine::plain(truncate(prefix, prefix_columns));
                for span in &content.spans {
                    line.push(span.text.clone(), span.role);
                }
                line
            })
            .collect::<Vec<_>>();

        InputFrame {
            lines: styled_lines.iter().map(RoleLine::text).collect(),
            styled_lines,
            cursor_row: layout.cursor_row.saturating_sub(first_visual_row),
            cursor_column: prefix_columns
                .saturating_add(layout.cursor_column)
                .min(terminal_width.saturating_sub(1)),
            first_visual_row,
            total_visual_rows: layout.rows.len(),
        }
    }
}

struct Layout {
    rows: Vec<RoleLine>,
    cursor_row: usize,
    cursor_column: usize,
}

fn layout(
    draft: &str,
    cursor: usize,
    content_width: usize,
    marks: &[(Range<usize>, Role)],
) -> Layout {
    let mut rows = vec![RoleLine::default()];
    let mut row = 0;
    let mut column = 0;
    let mut logical_column = 0;
    let mut cursor_position = None;

    for (byte, character) in draft.char_indices() {
        let role = marks
            .iter()
            .find(|(range, _)| range.contains(&byte))
            .map(|(_, role)| *role);
        if character == '\n' {
            if cursor == byte {
                cursor_position = Some(if column == content_width {
                    (row + 1, 0)
                } else {
                    (row, column)
                });
            }
            rows.push(RoleLine::default());
            row += 1;
            column = 0;
            logical_column = 0;
            continue;
        }

        if column == content_width {
            rows.push(RoleLine::default());
            row += 1;
            column = 0;
        }
        if cursor == byte {
            cursor_position = Some((row, column));
        }

        if character == '\t' {
            let spaces = TAB_COLUMNS - logical_column % TAB_COLUMNS;
            for _ in 0..spaces {
                if column == content_width {
                    rows.push(RoleLine::default());
                    row += 1;
                    column = 0;
                }
                push_character(&mut rows[row], ' ', role);
                column += 1;
                logical_column += 1;
            }
            continue;
        }

        let displayed = safe_character(character, content_width);
        let character_width = displayed.width().unwrap_or(0);
        if column > 0 && column + character_width > content_width {
            rows.push(RoleLine::default());
            row += 1;
            column = 0;
        }
        push_character(&mut rows[row], displayed, role);
        column += character_width;
        logical_column += character_width;
    }

    if cursor == draft.len() {
        if column == content_width {
            rows.push(RoleLine::default());
            row += 1;
            column = 0;
        }
        cursor_position = Some((row, column));
    }
    let (cursor_row, cursor_column) = cursor_position.unwrap_or((0, 0));

    Layout {
        rows,
        cursor_row,
        cursor_column,
    }
}

fn push_character(line: &mut RoleLine, character: char, role: Option<Role>) {
    if let Some(last) = line.spans.last_mut()
        && last.role == role
    {
        last.text.push(character);
    } else {
        line.push(character.to_string(), role);
    }
}

fn safe_character(character: char, content_width: usize) -> char {
    if character.is_control() || character.width().unwrap_or(0) > content_width {
        '�'
    } else {
        character
    }
}

fn truncate(text: &str, columns: usize) -> &str {
    let byte = text
        .char_indices()
        .take(columns)
        .last()
        .map_or(0, |(byte, character)| byte + character.len_utf8());
    &text[..byte]
}

fn floor_char_boundary(text: &str, mut cursor: usize) -> usize {
    while !text.is_char_boundary(cursor) {
        cursor -= 1;
    }
    cursor
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(draft: &str, cursor: usize, width: usize, rows: usize) -> InputFrame {
        InputView::frame(draft, cursor, width, rows)
    }

    #[test]
    fn chinese_characters_wrap_by_terminal_columns() {
        let result = frame("ab中文cd", "ab中".len(), 7, 4);

        assert_eq!(result.lines, ["> ab中", "  文cd"]);
        assert_eq!((result.cursor_row, result.cursor_column), (0, 6));
        assert_eq!(result.total_visual_rows, 2);
    }

    #[test]
    fn tabs_expand_to_four_column_stops_across_soft_wraps() {
        let result = frame("a\tb", "a\t".len(), 6, 4);

        assert_eq!(result.lines, ["> a   ", "  b"]);
        assert_eq!((result.cursor_row, result.cursor_column), (1, 2));
    }

    #[test]
    fn logical_newlines_create_real_continuation_rows() {
        let result = frame("first\n\n末行", "first\n".len(), 12, 5);

        assert_eq!(result.lines, ["> first", "  ", "  末行"]);
        assert_eq!((result.cursor_row, result.cursor_column), (1, 2));
    }

    #[test]
    fn long_unbroken_text_soft_wraps_without_losing_content() {
        let result = frame("abcdefghijkl", 12, 6, 8);

        assert_eq!(result.lines, ["> abcd", "  efgh", "  ijkl", "  "]);
        assert_eq!((result.cursor_row, result.cursor_column), (3, 2));
    }

    #[test]
    fn cursor_in_the_middle_maps_to_its_visual_row_and_column() {
        let draft = "abcd中文efgh";
        let cursor = "abcd".len();
        let result = frame(draft, cursor, 8, 5);

        assert_eq!(result.lines, ["> abcd中", "  文efgh"]);
        assert_eq!((result.cursor_row, result.cursor_column), (0, 6));
    }

    #[test]
    fn viewport_keeps_the_cursor_visible_and_scrolls_older_rows_up() {
        let result = frame(
            "one\ntwo\nthree\nfour\nfive",
            "one\ntwo\nthree\nfour".len(),
            12,
            2,
        );

        assert_eq!(result.lines, ["  three", "  four"]);
        assert_eq!(result.cursor_row, 1);
        assert_eq!(result.first_visual_row, 2);
        assert_eq!(result.total_visual_rows, 5);
    }

    #[test]
    fn resize_reflows_around_the_same_byte_cursor() {
        let draft = "abcd中文efghijkl";
        let cursor = "abcd中".len();
        let wide = frame(draft, cursor, 10, 3);
        let narrow = frame(draft, cursor, 7, 3);

        assert_eq!((wide.cursor_row, wide.cursor_column), (0, 8));
        assert_eq!((narrow.cursor_row, narrow.cursor_column), (1, 4));
        assert_eq!(wide.total_visual_rows, 2);
        assert_eq!(narrow.total_visual_rows, 4);
        assert!(wide.cursor_row < wide.lines.len());
        assert!(narrow.cursor_row < narrow.lines.len());
    }

    #[test]
    fn controls_are_rendered_as_safe_replacement_characters() {
        let result = frame("a\r\u{1b}\u{7f}b", 5, 20, 3);

        assert_eq!(result.lines, ["> a���b"]);
        assert!(
            result
                .lines
                .iter()
                .all(|line| !line.chars().any(char::is_control))
        );
    }

    #[test]
    fn invalid_utf8_cursor_is_floored_to_a_character_boundary() {
        let result = frame("中文", 1, 10, 2);

        assert_eq!((result.cursor_row, result.cursor_column), (0, 2));
    }

    #[test]
    fn zero_sized_view_has_no_rows_or_out_of_bounds_cursor() {
        let result = frame("draft", 5, 0, 3);
        assert!(result.lines.is_empty());
        assert_eq!((result.cursor_row, result.cursor_column), (0, 0));

        let result = frame("draft", 5, 20, 0);
        assert!(result.lines.is_empty());
        assert_eq!((result.cursor_row, result.cursor_column), (0, 0));
    }
}
