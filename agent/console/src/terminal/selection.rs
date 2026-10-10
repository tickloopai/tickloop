use std::ops::Range;
use std::time::Instant;

use unicode_width::UnicodeWidthChar;

/// A cell of a [`Surface`]. Columns count terminal cells, so a wide character
/// occupies two of them.
#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub(super) struct Cell {
    pub row: usize,
    pub column: usize,
}

/// What a selection's rows count.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Surface {
    /// Rows of the screen as last drawn, such as the input below the conversation.
    Screen,
    /// Rows of the conversation's content, so scrolling keeps the same text selected.
    Conversation,
    /// Rows of the detailed transcript's content.
    Transcript,
}

/// A drag in progress or finished. `anchor` is where the button went down and
/// `head` follows the pointer, so dragging backwards is expressed by an anchor
/// that sorts after the head rather than by a separate case.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Selection {
    surface: Surface,
    anchor: Cell,
    head: Cell,
}

impl Selection {
    pub(super) fn started_at(surface: Surface, anchor: Cell) -> Self {
        Self {
            surface,
            anchor,
            head: anchor,
        }
    }

    pub(super) fn surface(self) -> Surface {
        self.surface
    }

    pub(super) fn drag_to(&mut self, head: Cell) {
        self.head = head;
    }

    pub(super) fn is_empty(self) -> bool {
        self.anchor == self.head
    }

    /// Ordered ends, so callers never have to care which way the drag went.
    pub(super) fn bounds(self) -> (Cell, Cell) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// Half-open cell range this selection covers on `row`, or `None` when the
    /// row is outside it. The end is exclusive so a zero-width range means
    /// nothing is selected on that row.
    pub(super) fn columns_on(self, row: usize, row_width: usize) -> Option<(usize, usize)> {
        let (start, end) = self.bounds();
        if row < start.row || row > end.row {
            return None;
        }
        let first = if row == start.row { start.column } else { 0 };
        let last = if row == end.row {
            end.column
        } else {
            row_width
        };
        (first < last).then_some((first, last.min(row_width)))
    }

    /// The rows the selection reaches, end exclusive.
    pub(super) fn rows(self) -> Range<usize> {
        let (start, end) = self.bounds();
        start.row..end.row + 1
    }

    /// Text the selection covers, joined with newlines between rows, read from
    /// `rows`, which holds the surface's rows from `first_row` on. Trailing
    /// blanks are dropped so selecting past the end of a line does not copy the
    /// padding the renderer wrote to fill the screen.
    pub(super) fn text(self, first_row: usize, rows: &[String]) -> String {
        self.rows()
            .filter_map(|row| {
                let line = rows.get(row.checked_sub(first_row)?)?;
                let (first, last) = self.columns_on(row, line_width(line))?;
                Some(slice_columns(line, first, last).trim_end().to_owned())
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ScrollDirection {
    Up,
    Down,
}

/// Where the last frame drew the content of a surface that scrolls: the screen
/// rows its content took, the columns it spans and the content row drawn first.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Viewport {
    pub surface: Surface,
    pub rows: Range<usize>,
    pub columns: usize,
    pub first_row: usize,
}

impl Viewport {
    pub(super) fn contains(&self, at: Cell) -> bool {
        self.rows.contains(&at.row) && at.column < self.columns
    }

    /// The content row drawn on `screen_row`, if the content reaches it.
    pub(super) fn content_row(&self, screen_row: usize) -> Option<usize> {
        self.rows
            .contains(&screen_row)
            .then(|| self.first_row + screen_row - self.rows.start)
    }

    /// The content cell under the screen cell `at`. Above the content is the
    /// start of its first row shown and below it the end of its last, so a drag
    /// past an edge takes whole rows.
    pub(super) fn content_cell(&self, at: Cell) -> Cell {
        if at.row < self.rows.start {
            Cell {
                row: self.first_row,
                column: 0,
            }
        } else if at.row >= self.rows.end {
            Cell {
                row: self.first_row + self.rows.len().saturating_sub(1),
                column: self.columns,
            }
        } else {
            Cell {
                row: self.first_row + at.row - self.rows.start,
                column: at.column.min(self.columns),
            }
        }
    }

    /// Which way a drag held at `at` scrolls the content: up at or above its
    /// first row shown and down at or below its last, as a terminal scrolls a
    /// selection dragged to the edge of its window.
    pub(super) fn drag_scroll(&self, at: Cell) -> Option<ScrollDirection> {
        if at.row <= self.rows.start {
            Some(ScrollDirection::Up)
        } else if at.row + 1 >= self.rows.end {
            Some(ScrollDirection::Down)
        } else {
            None
        }
    }
}

/// The button held after a press started a selection in scrollable content.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct SelectionDrag {
    /// Where the pointer last moved, once it has.
    pub pointer: Option<Cell>,
    /// When a pointer held at an edge next scrolls the content; `None` scrolls at once.
    pub next_scroll_at: Option<Instant>,
}

/// Splits `line` into the text before, inside and after the cells
/// `[first, last)`, by the same rule `text` copies with. What is highlighted is
/// therefore exactly what lands on the clipboard.
pub(super) fn split_columns(line: &str, first: usize, last: usize) -> (String, String, String) {
    let (mut before, mut inside, mut after) = (String::new(), String::new(), String::new());
    let mut column = 0;
    for character in line.chars() {
        let ends_at = column + char_width(character);
        if column < last && ends_at > first {
            inside.push(character);
        } else if ends_at <= first {
            before.push(character);
        } else {
            after.push(character);
        }
        column = ends_at;
    }
    (before, inside, after)
}

pub(super) fn line_width(line: &str) -> usize {
    line.chars().map(char_width).sum()
}

fn char_width(character: char) -> usize {
    UnicodeWidthChar::width(character).unwrap_or(0)
}

/// Takes the characters whose cells fall inside `[first, last)`. A wide
/// character straddling either edge is kept whole rather than split, because
/// half of one is not a character the user can paste.
fn slice_columns(line: &str, first: usize, last: usize) -> String {
    let mut column = 0;
    let mut taken = String::new();
    for character in line.chars() {
        let width = char_width(character);
        let ends_at = column + width;
        if column < last && ends_at > first {
            taken.push(character);
        }
        column = ends_at;
        if column >= last {
            break;
        }
    }
    taken
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(row: usize, column: usize) -> Cell {
        Cell { row, column }
    }

    fn screen() -> Vec<String> {
        vec![
            "hello world".into(),
            "中文混排 mixed".into(),
            "third line".into(),
        ]
    }

    #[test]
    fn a_fresh_selection_covers_nothing() {
        let selection = Selection::started_at(Surface::Screen, cell(0, 3));
        assert!(selection.is_empty());
        assert_eq!(selection.text(0, &screen()), "");
    }

    #[test]
    fn a_single_row_selection_takes_the_dragged_span() {
        let mut selection = Selection::started_at(Surface::Screen, cell(0, 0));
        selection.drag_to(cell(0, 5));
        assert_eq!(selection.text(0, &screen()), "hello");
    }

    #[test]
    fn dragging_backwards_selects_the_same_text() {
        let mut forwards = Selection::started_at(Surface::Screen, cell(0, 6));
        forwards.drag_to(cell(0, 11));
        let mut backwards = Selection::started_at(Surface::Screen, cell(0, 11));
        backwards.drag_to(cell(0, 6));
        assert_eq!(forwards.text(0, &screen()), "world");
        assert_eq!(backwards.text(0, &screen()), forwards.text(0, &screen()));
    }

    #[test]
    fn a_multi_row_selection_joins_rows_with_newlines() {
        let mut selection = Selection::started_at(Surface::Screen, cell(0, 6));
        selection.drag_to(cell(2, 5));
        assert_eq!(selection.text(0, &screen()), "world\n中文混排 mixed\nthird");
    }

    #[test]
    fn wide_characters_are_never_split_in_half() {
        // "中文混排" occupies columns 0..8, two cells each.
        let mut selection = Selection::started_at(Surface::Screen, cell(1, 0));
        selection.drag_to(cell(1, 4));
        assert_eq!(selection.text(0, &screen()), "中文");

        // Starting inside a wide character keeps that character whole.
        let mut straddling = Selection::started_at(Surface::Screen, cell(1, 1));
        straddling.drag_to(cell(1, 5));
        assert_eq!(straddling.text(0, &screen()), "中文混");
    }

    #[test]
    fn selecting_past_the_end_of_a_line_does_not_copy_padding() {
        let mut selection = Selection::started_at(Surface::Screen, cell(2, 0));
        selection.drag_to(cell(2, 60));
        assert_eq!(selection.text(0, &screen()), "third line");
    }

    #[test]
    fn rows_outside_the_selection_report_no_columns() {
        let mut selection = Selection::started_at(Surface::Screen, cell(1, 0));
        selection.drag_to(cell(1, 4));
        assert_eq!(selection.columns_on(0, 11), None);
        assert_eq!(selection.columns_on(2, 10), None);
        assert_eq!(selection.columns_on(1, 14), Some((0, 4)));
    }

    #[test]
    fn a_middle_row_of_a_multi_row_selection_spans_its_whole_width() {
        let mut selection = Selection::started_at(Surface::Screen, cell(0, 6));
        selection.drag_to(cell(2, 5));
        assert_eq!(selection.columns_on(1, 14), Some((0, 14)));
    }

    #[test]
    fn splitting_highlights_exactly_what_is_copied() {
        let mut selection = Selection::started_at(Surface::Screen, cell(1, 1));
        selection.drag_to(cell(1, 5));
        let line = &screen()[1];
        let (first, last) = selection.columns_on(1, line_width(line)).unwrap();
        let (before, inside, after) = split_columns(line, first, last);
        assert_eq!(inside, selection.text(0, &screen()));
        assert_eq!(format!("{before}{inside}{after}"), *line);
    }

    #[test]
    fn a_selection_reaching_beyond_the_screen_stops_at_the_last_row() {
        let mut selection = Selection::started_at(Surface::Screen, cell(1, 0));
        selection.drag_to(cell(9, 0));
        assert_eq!(selection.text(0, &screen()), "中文混排 mixed\nthird line");
    }

    #[test]
    fn content_rows_are_read_from_where_the_given_rows_start() {
        let mut selection = Selection::started_at(Surface::Conversation, cell(41, 4));
        selection.drag_to(cell(42, 4));
        assert_eq!(selection.rows(), 41..43);
        assert_eq!(selection.text(40, &screen()), "混排 mixed\nthir");
    }

    /// Content rows 50.. drawn on screen rows 2..12 of a 30-column surface.
    fn viewport() -> Viewport {
        Viewport {
            surface: Surface::Conversation,
            rows: 2..12,
            columns: 30,
            first_row: 50,
        }
    }

    #[test]
    fn a_viewport_maps_screen_rows_to_the_content_drawn_there() {
        let viewport = viewport();
        assert_eq!(viewport.content_row(1), None);
        assert_eq!(viewport.content_row(2), Some(50));
        assert_eq!(viewport.content_row(11), Some(59));
        assert_eq!(viewport.content_row(12), None);
        assert!(viewport.contains(cell(5, 29)));
        assert!(
            !viewport.contains(cell(5, 30)),
            "Monitor's column is not content"
        );
    }

    #[test]
    fn a_pointer_past_an_edge_takes_the_whole_row_at_that_edge() {
        let viewport = viewport();
        assert_eq!(viewport.content_cell(cell(4, 7)), cell(52, 7));
        assert_eq!(viewport.content_cell(cell(4, 70)), cell(52, 30));
        assert_eq!(viewport.content_cell(cell(0, 7)), cell(50, 0));
        assert_eq!(viewport.content_cell(cell(20, 7)), cell(59, 30));
    }

    #[test]
    fn a_drag_scrolls_at_or_past_the_first_and_last_rows_shown() {
        let viewport = viewport();
        assert_eq!(viewport.drag_scroll(cell(0, 3)), Some(ScrollDirection::Up));
        assert_eq!(viewport.drag_scroll(cell(2, 3)), Some(ScrollDirection::Up));
        assert_eq!(viewport.drag_scroll(cell(3, 3)), None);
        assert_eq!(viewport.drag_scroll(cell(10, 3)), None);
        assert_eq!(
            viewport.drag_scroll(cell(11, 3)),
            Some(ScrollDirection::Down)
        );
        assert_eq!(
            viewport.drag_scroll(cell(23, 3)),
            Some(ScrollDirection::Down)
        );
    }
}
