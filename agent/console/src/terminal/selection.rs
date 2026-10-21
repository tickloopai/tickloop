use std::ops::Range;
use std::time::Instant;

use tickloop_ui::hyperlink::web_url_ranges;
use unicode_width::UnicodeWidthChar;

use super::style::RoleLine;

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

/// What a drag extends a selection by: a press selects by character, a double-click by word and
/// a triple-click by logical line, as terminals and Claude Code do.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Granularity {
    Character,
    Word,
    Line,
}

/// A drag in progress or finished. `anchor` is where the button went down and
/// `head` follows the pointer, so dragging backwards is expressed by an anchor
/// that sorts after the head rather than by a separate case. A word or line
/// selection keeps `anchor_span`, the unit the clicks chose, whichever way it
/// is dragged.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Selection {
    surface: Surface,
    anchor: Cell,
    head: Cell,
    granularity: Granularity,
    anchor_span: (Cell, Cell),
}

impl Selection {
    pub(super) fn started_at(surface: Surface, anchor: Cell) -> Self {
        Self {
            surface,
            anchor,
            head: anchor,
            granularity: Granularity::Character,
            anchor_span: (anchor, anchor),
        }
    }

    /// A selection of the unit `[start, end)` that a double- or triple-click chose.
    pub(super) fn spanning(
        surface: Surface,
        granularity: Granularity,
        (start, end): (Cell, Cell),
    ) -> Self {
        Self {
            surface,
            anchor: start,
            head: end,
            granularity,
            anchor_span: (start, end),
        }
    }

    pub(super) fn surface(self) -> Surface {
        self.surface
    }

    pub(super) fn granularity(self) -> Granularity {
        self.granularity
    }

    pub(super) fn drag_to(&mut self, head: Cell) {
        self.head = head;
    }

    /// Extends a word or line selection over the unit `[start, end)` under the pointer, keeping
    /// the unit the clicks chose.
    pub(super) fn drag_over(&mut self, (start, end): (Cell, Cell)) {
        let (chosen_start, chosen_end) = self.anchor_span;
        if start < chosen_start {
            self.anchor = chosen_end;
            self.head = start;
        } else {
            self.anchor = chosen_start;
            self.head = end.max(chosen_end);
        }
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

    /// Text the selection covers, read from `rows`, which holds the surface's
    /// rows from `first_row` on. A row that continues the logical line above it
    /// joins that line without its hanging indent, with the space the wrap took
    /// put back; other rows are joined with newlines. Rows strictly inside the
    /// selection count even when blank, so a copied paragraph break survives.
    /// Trailing blanks are dropped so selecting past the end of a line does not
    /// copy the padding the renderer wrote to fill the screen.
    pub(super) fn text(self, first_row: usize, rows: &[RoleLine]) -> String {
        let (start, end) = self.bounds();
        let mut text = String::new();
        let mut copied_any = false;
        for row in self.rows() {
            let Some(line) = row.checked_sub(first_row).and_then(|index| rows.get(index)) else {
                continue;
            };
            let plain = line.text();
            let width = line_width(&plain);
            let inside = start.row < row && row < end.row;
            let Some((mut first, last)) = self
                .columns_on(row, width)
                .or_else(|| inside.then_some((0, 0)))
            else {
                continue;
            };
            if copied_any {
                match line.continuation {
                    Some(continuation) => {
                        first = first.max(continuation.indent);
                        let piece = slice_columns(&plain, first, last);
                        if piece.trim_end().is_empty() {
                            continue;
                        }
                        if continuation.after_space {
                            text.push(' ');
                        }
                    }
                    None => text.push('\n'),
                }
            }
            text.push_str(slice_columns(&plain, first, last).trim_end());
            copied_any = true;
        }
        text
    }
}

/// One character of a logical line put together from its rows, with the cells it was drawn in.
/// A space that wrapping replaced has no cells, and a zero-width character such as a combining
/// mark shares the cells of the character it follows.
struct LogicalCharacter {
    character: char,
    cells: Option<(Cell, Cell)>,
}

/// The logical line drawn on `row`, which `rows` holds from `first_row` on: the rows a long
/// line wrapped onto, joined without their hanging indents.
fn logical_line(rows: &[RoleLine], first_row: usize, row: usize) -> Option<Vec<LogicalCharacter>> {
    let index = row.checked_sub(first_row)?;
    rows.get(index)?;
    let mut first = index;
    while first > 0 && rows[first].continuation.is_some() {
        first -= 1;
    }
    let mut last = index;
    while rows
        .get(last + 1)
        .is_some_and(|row| row.continuation.is_some())
    {
        last += 1;
    }
    let mut characters = Vec::new();
    for (offset, line) in rows[first..=last].iter().enumerate() {
        let skip = match line.continuation {
            Some(continuation) if offset > 0 => {
                if continuation.after_space {
                    characters.push(LogicalCharacter {
                        character: ' ',
                        cells: None,
                    });
                }
                continuation.indent
            }
            _ => 0,
        };
        let row = first_row + first + offset;
        let mut column = 0;
        for character in line.text().chars() {
            let width = char_width(character);
            let base_cells = characters
                .last()
                .and_then(|base: &LogicalCharacter| base.cells);
            if width == 0
                && let Some(cells) = base_cells.filter(|(start, _)| start.row == row)
            {
                characters.push(LogicalCharacter {
                    character,
                    cells: Some(cells),
                });
            } else if column >= skip {
                characters.push(LogicalCharacter {
                    character,
                    cells: Some((
                        Cell { row, column },
                        Cell {
                            row,
                            column: column + width,
                        },
                    )),
                });
            }
            column += width;
        }
    }
    Some(characters)
}

/// The index of the character of `line` drawn over the cell `at`.
fn character_at(line: &[LogicalCharacter], at: Cell) -> Option<usize> {
    line.iter().position(|character| {
        character.cells.is_some_and(|(start, end)| {
            start.row == at.row && (start.column..end.column).contains(&at.column)
        })
    })
}

/// The cells `[start, end)` that the characters `range` of `line` were drawn over.
fn span_cells(line: &[LogicalCharacter], range: Range<usize>) -> Option<(Cell, Cell)> {
    let start = line[range.clone()]
        .iter()
        .find_map(|character| character.cells)?
        .0;
    let end = line[range]
        .iter()
        .rev()
        .find_map(|character| character.cells)?
        .1;
    Some((start, end))
}

/// The word a double-click on `at` selects, as the cells it covers. A web URL is taken whole,
/// then a quoted path, which may hold spaces; otherwise the run of characters of the clicked
/// character's kind: a word or path such as `src/main.rs:12`, a run of CJK text, or a run of
/// punctuation. Whitespace selects nothing.
pub(super) fn word_at(rows: &[RoleLine], first_row: usize, at: Cell) -> Option<(Cell, Cell)> {
    let line = logical_line(rows, first_row, at.row)?;
    let clicked = character_at(&line, at)?;
    let text: String = line.iter().map(|character| character.character).collect();
    let starts: Vec<usize> = text.char_indices().map(|(byte, _)| byte).collect();
    let characters_of = |bytes: Range<usize>| {
        starts.partition_point(|start| *start < bytes.start)
            ..starts.partition_point(|start| *start < bytes.end)
    };
    let url = web_url_ranges(&text)
        .into_iter()
        .map(characters_of)
        .find(|range| range.contains(&clicked));
    let range = url
        .or_else(|| quoted_path_at(&line, clicked))
        .or_else(|| same_kind_run_at(&line, clicked))?;
    span_cells(&line, range)
}

/// The web URL drawn over `at`, read from the logical line so a URL that wrapped opens whole.
pub(super) fn url_at(rows: &[RoleLine], first_row: usize, at: Cell) -> Option<String> {
    let line = logical_line(rows, first_row, at.row)?;
    let clicked = character_at(&line, at)?;
    let text: String = line.iter().map(|character| character.character).collect();
    let clicked_byte = text.char_indices().nth(clicked)?.0;
    web_url_ranges(&text)
        .into_iter()
        .find(|range| range.contains(&clicked_byte))
        .map(|range| text[range].to_owned())
}

/// The cells of the whole logical line drawn on `row`, the unit a triple-click selects.
pub(super) fn line_at(rows: &[RoleLine], first_row: usize, row: usize) -> Option<(Cell, Cell)> {
    let line = logical_line(rows, first_row, row)?;
    let (_, end) = span_cells(&line, 0..line.len())
        .unwrap_or((Cell { row, column: 0 }, Cell { row, column: 0 }));
    let first_row_of_line = line
        .iter()
        .find_map(|character| character.cells)
        .map_or(row, |(start, _)| start.row);
    Some((
        Cell {
            row: first_row_of_line,
            column: 0,
        },
        end,
    ))
}

const QUOTES: [char; 3] = ['"', '\'', '`'];

/// The text between a pair of matching quotes around `clicked`, when it names a path.
/// Quotes pair up from the start of the line, so the closing quote of one string never opens
/// the next; an apostrophe between two letters, as in "it's", quotes nothing.
fn quoted_path_at(line: &[LogicalCharacter], clicked: usize) -> Option<Range<usize>> {
    if QUOTES.contains(&line[clicked].character) {
        return None;
    }
    let is_apostrophe = |index: usize| {
        let around = |index: Option<usize>| {
            index
                .and_then(|index| line.get(index))
                .is_some_and(|character| character.character.is_alphanumeric())
        };
        line[index].character == '\'' && around(index.checked_sub(1)) && around(Some(index + 1))
    };
    QUOTES.into_iter().find_map(|quote| {
        let mut open = None;
        for index in (0..line.len()).filter(|index| line[*index].character == quote) {
            if is_apostrophe(index) {
                continue;
            }
            let Some(start) = open.take() else {
                open = Some(index);
                continue;
            };
            if start < clicked && clicked < index {
                let inner = &line[start + 1..index];
                return inner
                    .iter()
                    .any(|character| character.character == '/')
                    .then_some(start + 1..index);
            }
            if index > clicked {
                return None;
            }
        }
        None
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CharacterKind {
    Space,
    /// CJK ideographs, kana and Hangul, which are written without spaces between words.
    Ideograph,
    /// Letters, digits and the characters paths and identifiers are made of.
    Word,
    Punctuation,
}

/// Characters that belong to a word, extending iTerm2's default `/-+\~_.` with what a path
/// with a line number, an address, an anchor or a query such as `?ref=main&page=2` holds.
const WORD_PUNCTUATION: &str = "_/.-+~\\:@#%=?&";
/// Characters that end a sentence rather than a path when they end a word.
const WORD_TRAILING_PUNCTUATION: &str = ".:?";

fn kind(character: char) -> CharacterKind {
    if character.is_whitespace() {
        CharacterKind::Space
    } else if is_ideograph(character) {
        CharacterKind::Ideograph
    } else if character.is_alphanumeric() || WORD_PUNCTUATION.contains(character) {
        CharacterKind::Word
    } else {
        CharacterKind::Punctuation
    }
}

fn is_ideograph(character: char) -> bool {
    matches!(
        character,
        '\u{3005}'
            | '\u{3040}'..='\u{30ff}'
            | '\u{3400}'..='\u{4dbf}'
            | '\u{4e00}'..='\u{9fff}'
            | '\u{ac00}'..='\u{d7af}'
            | '\u{f900}'..='\u{faff}'
            | '\u{20000}'..='\u{3134f}'
    )
}

/// The kind of each character of `line`; a zero-width character takes the kind of the
/// character it marks, so a combining accent never splits a word.
fn kinds(line: &[LogicalCharacter]) -> Vec<CharacterKind> {
    let mut kinds: Vec<CharacterKind> = Vec::with_capacity(line.len());
    for character in line {
        let own = kind(character.character);
        let marks_previous = char_width(character.character) == 0 && own != CharacterKind::Space;
        kinds.push(match kinds.last() {
            Some(previous) if marks_previous => *previous,
            _ => own,
        });
    }
    kinds
}

fn same_kind_run_at(line: &[LogicalCharacter], clicked: usize) -> Option<Range<usize>> {
    let kinds = kinds(line);
    let clicked_kind = kinds[clicked];
    if clicked_kind == CharacterKind::Space {
        return None;
    }
    let start = kinds[..clicked]
        .iter()
        .rposition(|kind| *kind != clicked_kind)
        .map_or(0, |before| before + 1);
    let mut end = clicked
        + kinds[clicked..]
            .iter()
            .position(|kind| *kind != clicked_kind)
            .unwrap_or(line.len() - clicked);
    if clicked_kind == CharacterKind::Word {
        while end > clicked + 1 && WORD_TRAILING_PUNCTUATION.contains(line[end - 1].character) {
            end -= 1;
        }
    }
    Some(start..end)
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
    let mut parts = [String::new(), String::new(), String::new()];
    for (character, part) in column_parts(line, first, last) {
        parts[part as usize].push(character);
    }
    let [before, inside, after] = parts;
    (before, inside, after)
}

#[derive(Clone, Copy, PartialEq)]
enum ColumnPart {
    Before,
    Inside,
    After,
}

/// Where each character of `line` falls against the cells `[first, last)`. A wide character
/// straddling either edge is kept whole rather than split, because half of one is not a
/// character the user can paste, and a zero-width character stays with the character it marks.
fn column_parts(line: &str, first: usize, last: usize) -> Vec<(char, ColumnPart)> {
    let mut column = 0;
    let mut parts: Vec<(char, ColumnPart)> = Vec::with_capacity(line.len());
    for character in line.chars() {
        let width = char_width(character);
        let ends_at = column + width;
        let part = match parts.last() {
            Some((_, previous)) if width == 0 => *previous,
            _ if column < last && ends_at > first => ColumnPart::Inside,
            _ if ends_at <= first => ColumnPart::Before,
            _ => ColumnPart::After,
        };
        parts.push((character, part));
        column = ends_at;
    }
    parts
}

pub(super) fn line_width(line: &str) -> usize {
    line.chars().map(char_width).sum()
}

fn char_width(character: char) -> usize {
    UnicodeWidthChar::width(character).unwrap_or(0)
}

/// Takes the characters whose cells fall inside `[first, last)`, by the rule of
/// [`column_parts`].
fn slice_columns(line: &str, first: usize, last: usize) -> String {
    column_parts(line, first, last)
        .into_iter()
        .filter(|(_, part)| *part == ColumnPart::Inside)
        .map(|(character, _)| character)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(row: usize, column: usize) -> Cell {
        Cell { row, column }
    }

    fn screen() -> Vec<RoleLine> {
        ["hello world", "中文混排 mixed", "third line"]
            .into_iter()
            .map(RoleLine::plain)
            .collect()
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
        let line = screen()[1].text();
        let (first, last) = selection.columns_on(1, line_width(&line)).unwrap();
        let (before, inside, after) = split_columns(&line, first, last);
        assert_eq!(inside, selection.text(0, &screen()));
        assert_eq!(format!("{before}{inside}{after}"), line);
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

    fn wrapped(text: &str, indent: usize, after_space: bool) -> RoleLine {
        RoleLine {
            continuation: Some(tickloop_ui::markdown::Continuation {
                indent,
                after_space,
            }),
            ..RoleLine::plain(text)
        }
    }

    /// A paragraph wrapped under a two-column hang, a URL split mid-word, and a blank row.
    fn conversation() -> Vec<RoleLine> {
        vec![
            RoleLine::plain("● copy this"),
            wrapped("  paragraph", 2, true),
            RoleLine::plain(""),
            RoleLine::plain("  open https://example.com/a"),
            wrapped("  /b/c.html now", 2, false),
        ]
    }

    #[test]
    fn a_copy_joins_wrapped_rows_and_keeps_blank_rows_between_lines() {
        let mut selection = Selection::started_at(Surface::Conversation, cell(10, 2));
        selection.drag_to(cell(14, 17));
        assert_eq!(
            selection.text(10, &conversation()),
            "copy this paragraph\n\n  open https://example.com/a/b/c.html now"
        );
    }

    #[test]
    fn a_copy_starting_on_a_continuation_row_keeps_its_own_columns() {
        let mut selection = Selection::started_at(Surface::Conversation, cell(1, 4));
        selection.drag_to(cell(1, 11));
        assert_eq!(selection.text(0, &conversation()), "ragraph");
    }

    fn word(rows: &[RoleLine], at: Cell) -> Option<String> {
        let (start, end) = word_at(rows, 0, at)?;
        Some(Selection::spanning(Surface::Screen, Granularity::Word, (start, end)).text(0, rows))
    }

    fn row(text: &str) -> Vec<RoleLine> {
        vec![RoleLine::plain(text)]
    }

    #[test]
    fn a_double_click_takes_the_word_or_path_under_it() {
        let line = row("see src/main.rs:12:5, foo_bar and ~/.claude/x.json.");
        assert_eq!(word(&line, cell(0, 6)).as_deref(), Some("src/main.rs:12:5"));
        assert_eq!(word(&line, cell(0, 24)).as_deref(), Some("foo_bar"));
        assert_eq!(
            word(&line, cell(0, 40)).as_deref(),
            Some("~/.claude/x.json")
        );
        assert_eq!(word(&line, cell(0, 3)), None, "whitespace selects nothing");
        assert_eq!(
            word(&line, cell(0, 80)),
            None,
            "past the end selects nothing"
        );
    }

    #[test]
    fn a_double_click_takes_a_whole_run_of_cjk_text() {
        // "用Rust写中文句子，然后" — 用 0..2, Rust 2..6, 写中文句子 6..16, ， 16..18.
        let line = row("用Rust写中文句子，然后");
        assert_eq!(word(&line, cell(0, 9)).as_deref(), Some("写中文句子"));
        assert_eq!(word(&line, cell(0, 3)).as_deref(), Some("Rust"));
        assert_eq!(word(&line, cell(0, 17)).as_deref(), Some("，"));
        assert_eq!(word(&line, cell(0, 1)).as_deref(), Some("用"));
    }

    #[test]
    fn a_double_click_takes_a_url_or_a_quoted_path_whole() {
        let line = row("(https://en.wikipedia.org/wiki/Rust_(language)), \"/tmp/my file.txt\"");
        assert_eq!(
            word(&line, cell(0, 12)).as_deref(),
            Some("https://en.wikipedia.org/wiki/Rust_(language)")
        );
        assert_eq!(
            word(&line, cell(0, 58)).as_deref(),
            Some("/tmp/my file.txt")
        );
    }

    #[test]
    fn a_url_that_wrapped_is_found_from_either_of_its_rows() {
        let rows = conversation();
        let full = "https://example.com/a/b/c.html";
        assert_eq!(url_at(&rows, 0, cell(3, 10)).as_deref(), Some(full));
        assert_eq!(url_at(&rows, 0, cell(4, 5)).as_deref(), Some(full));
        assert_eq!(url_at(&rows, 0, cell(4, 13)), None);
        assert_eq!(word(&rows, cell(4, 4)).as_deref(), Some(full));
    }

    #[test]
    fn a_triple_click_takes_the_whole_logical_line() {
        let rows = conversation();
        let line = |row| {
            let span = line_at(&rows, 0, row).unwrap();
            Selection::spanning(Surface::Screen, Granularity::Line, span).text(0, &rows)
        };
        assert_eq!(line(1), "● copy this paragraph");
        assert_eq!(line(0), line(1));
        assert_eq!(line(3), "  open https://example.com/a/b/c.html now");
    }

    #[test]
    fn dragging_a_word_selection_keeps_the_word_the_clicks_chose() {
        let rows = row("alpha beta gamma");
        let chosen = word_at(&rows, 0, cell(0, 7)).unwrap();
        let mut selection = Selection::spanning(Surface::Screen, Granularity::Word, chosen);
        selection.drag_over(word_at(&rows, 0, cell(0, 13)).unwrap());
        assert_eq!(selection.text(0, &rows), "beta gamma");
        selection.drag_over(word_at(&rows, 0, cell(0, 1)).unwrap());
        assert_eq!(selection.text(0, &rows), "alpha beta");
    }

    #[test]
    fn apostrophes_and_neighbouring_strings_do_not_quote_a_double_click() {
        let prose = row("it's in /tmp and we don't care");
        assert_eq!(word(&prose, cell(0, 5)).as_deref(), Some("in"));
        assert_eq!(word(&prose, cell(0, 15)).as_deref(), Some("and"));
        let strings = row("\"a\" src/lib.rs \"b\"");
        assert_eq!(word(&strings, cell(0, 6)).as_deref(), Some("src/lib.rs"));
    }

    #[test]
    fn a_combining_mark_stays_with_the_word_it_marks() {
        let line = row("say cafe\u{301} now");
        assert_eq!(word(&line, cell(0, 5)).as_deref(), Some("cafe\u{301}"));
        let mut drag = Selection::started_at(Surface::Screen, cell(0, 4));
        drag.drag_to(cell(0, 8));
        assert_eq!(drag.text(0, &line), "cafe\u{301}");
    }

    #[test]
    fn a_drag_ending_inside_a_hang_indent_copies_no_trailing_space() {
        let mut selection = Selection::started_at(Surface::Conversation, cell(0, 2));
        selection.drag_to(cell(1, 1));
        assert_eq!(selection.text(0, &conversation()), "copy this");
    }

    #[test]
    fn a_double_click_keeps_a_path_query_but_not_a_question_mark_ending_a_sentence() {
        let path = "repos/anthropics/sandbox-runtime/contents/package.json?ref=c40cf14a&page=2";
        let text = format!("gh api {path} then is it foo.rs?");
        let line = row(&text);
        assert_eq!(word(&line, cell(0, 12)).as_deref(), Some(path));
        let question = text.find("foo").unwrap();
        assert_eq!(word(&line, cell(0, question)).as_deref(), Some("foo.rs"));
    }
}
