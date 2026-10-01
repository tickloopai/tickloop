use unicode_width::UnicodeWidthChar;

/// A cell on the rendered screen. Columns count terminal cells, so a wide
/// character occupies two of them.
#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub(super) struct Cell {
    pub row: usize,
    pub column: usize,
}

/// A drag in progress or finished. `anchor` is where the button went down and
/// `head` follows the pointer, so dragging backwards is expressed by an anchor
/// that sorts after the head rather than by a separate case.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Selection {
    anchor: Cell,
    head: Cell,
}

impl Selection {
    pub(super) fn started_at(anchor: Cell) -> Self {
        Self {
            anchor,
            head: anchor,
        }
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

    /// Text the selection covers, joined with newlines between rows. Trailing
    /// blanks are dropped so selecting past the end of a line does not copy the
    /// padding the renderer wrote to fill the screen.
    pub(super) fn text(self, screen: &[String]) -> String {
        let (start, end) = self.bounds();
        (start.row..=end.row)
            .filter_map(|row| {
                let line = screen.get(row)?;
                let (first, last) = self.columns_on(row, line_width(line))?;
                Some(slice_columns(line, first, last).trim_end().to_owned())
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
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
        let selection = Selection::started_at(cell(0, 3));
        assert!(selection.is_empty());
        assert_eq!(selection.text(&screen()), "");
    }

    #[test]
    fn a_single_row_selection_takes_the_dragged_span() {
        let mut selection = Selection::started_at(cell(0, 0));
        selection.drag_to(cell(0, 5));
        assert_eq!(selection.text(&screen()), "hello");
    }

    #[test]
    fn dragging_backwards_selects_the_same_text() {
        let mut forwards = Selection::started_at(cell(0, 6));
        forwards.drag_to(cell(0, 11));
        let mut backwards = Selection::started_at(cell(0, 11));
        backwards.drag_to(cell(0, 6));
        assert_eq!(forwards.text(&screen()), "world");
        assert_eq!(backwards.text(&screen()), forwards.text(&screen()));
    }

    #[test]
    fn a_multi_row_selection_joins_rows_with_newlines() {
        let mut selection = Selection::started_at(cell(0, 6));
        selection.drag_to(cell(2, 5));
        assert_eq!(selection.text(&screen()), "world\n中文混排 mixed\nthird");
    }

    #[test]
    fn wide_characters_are_never_split_in_half() {
        // "中文混排" occupies columns 0..8, two cells each.
        let mut selection = Selection::started_at(cell(1, 0));
        selection.drag_to(cell(1, 4));
        assert_eq!(selection.text(&screen()), "中文");

        // Starting inside a wide character keeps that character whole.
        let mut straddling = Selection::started_at(cell(1, 1));
        straddling.drag_to(cell(1, 5));
        assert_eq!(straddling.text(&screen()), "中文混");
    }

    #[test]
    fn selecting_past_the_end_of_a_line_does_not_copy_padding() {
        let mut selection = Selection::started_at(cell(2, 0));
        selection.drag_to(cell(2, 60));
        assert_eq!(selection.text(&screen()), "third line");
    }

    #[test]
    fn rows_outside_the_selection_report_no_columns() {
        let mut selection = Selection::started_at(cell(1, 0));
        selection.drag_to(cell(1, 4));
        assert_eq!(selection.columns_on(0, 11), None);
        assert_eq!(selection.columns_on(2, 10), None);
        assert_eq!(selection.columns_on(1, 14), Some((0, 4)));
    }

    #[test]
    fn a_middle_row_of_a_multi_row_selection_spans_its_whole_width() {
        let mut selection = Selection::started_at(cell(0, 6));
        selection.drag_to(cell(2, 5));
        assert_eq!(selection.columns_on(1, 14), Some((0, 14)));
    }

    #[test]
    fn splitting_highlights_exactly_what_is_copied() {
        let mut selection = Selection::started_at(cell(1, 1));
        selection.drag_to(cell(1, 5));
        let line = &screen()[1];
        let (first, last) = selection.columns_on(1, line_width(line)).unwrap();
        let (before, inside, after) = split_columns(line, first, last);
        assert_eq!(inside, selection.text(&screen()));
        assert_eq!(format!("{before}{inside}{after}"), *line);
    }

    #[test]
    fn a_selection_reaching_beyond_the_screen_stops_at_the_last_row() {
        let mut selection = Selection::started_at(cell(1, 0));
        selection.drag_to(cell(9, 0));
        assert_eq!(selection.text(&screen()), "中文混排 mixed\nthird line");
    }
}
