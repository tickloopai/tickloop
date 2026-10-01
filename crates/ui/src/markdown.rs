//! Markdown layout shared by every host that shows Markdown.
//!
//! The rules reproduce Claude Code's `formatToken`, `MarkdownTable` and
//! `StreamingMarkdown`; `docs/design/agent-ui.md` records them together with the
//! deliberate deviations. Output lines carry semantic [`Marks`] instead of
//! styles, so each host maps them onto its own theme.

use std::ops::Range;

use pulldown_cmark::{Alignment, Event, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::Style,
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation;

use crate::{format::width, settings::AmbiguousWidth, styles::RendererStyles};

const QUOTE_BAR: &str = "▎ ";
const RULE: &str = "---";
const UNORDERED_MARKER: &str = "- ";
const TAB_COLUMNS: usize = 4;
const MIN_COLUMN_WIDTH: usize = 3;
/// A space on each side of the cell plus the border after it.
const COLUMN_BORDER_WIDTH: usize = 3;
/// `": "` after a vertical-table label.
const VERTICAL_LABEL_SEPARATOR_WIDTH: usize = 2;
const TABLE_SAFETY_MARGIN: usize = 4;
const MAX_TABLE_ROW_LINES: usize = 4;
const MAX_VERTICAL_SEPARATOR_WIDTH: usize = 40;
const MIN_VERTICAL_VALUE_WIDTH: usize = 10;
const VERTICAL_CONTINUATION_INDENT: &str = "  ";
const UNBOUNDED_COLUMNS: usize = usize::MAX / 8;

/// What a run of text is in the document. A run may carry several marks, such
/// as strong text inside a quote.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Marks {
    pub strong: bool,
    pub emphasis: bool,
    pub heading: Option<u8>,
    pub inline_code: bool,
    pub link: bool,
    pub quote: bool,
    pub quote_bar: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MarkdownSpan {
    pub text: String,
    pub marks: Marks,
}

/// One laid-out row. `source_offset` is a byte offset into the Markdown source
/// near where the row's content starts; offsets never decrease through a
/// document, so hosts can use them to keep a scroll position across reflows.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MarkdownLine {
    pub source_offset: usize,
    pub spans: Vec<MarkdownSpan>,
}

impl MarkdownLine {
    pub fn text(&self) -> String {
        self.spans.iter().map(|span| span.text.as_str()).collect()
    }

    fn push(&mut self, text: &str, marks: Marks) {
        if text.is_empty() {
            return;
        }
        if let Some(last) = self.spans.last_mut()
            && last.marks == marks
        {
            last.text.push_str(text);
        } else {
            self.spans.push(MarkdownSpan {
                text: text.to_owned(),
                marks,
            });
        }
    }

    fn trim_end(&mut self) {
        while let Some(last) = self.spans.last_mut() {
            let trimmed = last.text.trim_end_matches(' ').len();
            last.text.truncate(trimmed);
            if !last.text.is_empty() {
                break;
            }
            self.spans.pop();
        }
    }
}

/// Lays out `source` for `columns` display columns.
pub fn layout(source: &str, columns: usize, ambiguous: AmbiguousWidth) -> Vec<MarkdownLine> {
    let mut streaming = StreamingLayout::default();
    streaming.update(source, columns, ambiguous);
    streaming.lines
}

/// The document as text without width limits, for archives and copies.
pub fn plain_text(source: &str) -> String {
    layout(source, UNBOUNDED_COLUMNS, AmbiguousWidth::Narrow)
        .iter()
        .map(MarkdownLine::text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Lays out Markdown as ratatui lines styled with the renderer styles.
pub fn render(
    markdown: &str,
    columns: usize,
    ambiguous: AmbiguousWidth,
    renderer_styles: &RendererStyles,
) -> Vec<Line<'static>> {
    layout(markdown, columns, ambiguous)
        .into_iter()
        .map(|line| {
            Line::from(
                line.spans
                    .into_iter()
                    .map(|span| Span::styled(span.text, style_for(span.marks, renderer_styles)))
                    .collect::<Vec<_>>(),
            )
        })
        .collect()
}

fn style_for(marks: Marks, renderer_styles: &RendererStyles) -> Style {
    let mut style = Style::new();
    if marks.heading.is_some() {
        style = style.patch(renderer_styles.title);
    }
    if marks.strong {
        style = style.patch(renderer_styles.strong);
    }
    if marks.emphasis || marks.quote {
        style = style.patch(renderer_styles.emphasis);
    }
    if marks.link {
        style = style.patch(renderer_styles.link);
    }
    if marks.inline_code {
        style = style.patch(renderer_styles.inline_code);
    }
    if marks.quote_bar {
        style = style.patch(renderer_styles.muted_text);
    }
    style
}

/// Layout of a Markdown document that grows while it streams. Every top-level
/// block before the last one is complete, so its rows are kept and only the
/// last block is laid out again on each update, as Claude Code's
/// `StreamingMarkdown` does.
#[derive(Debug, Default)]
pub struct StreamingLayout {
    columns: usize,
    ambiguous: AmbiguousWidth,
    stable_source: String,
    stable_line_count: usize,
    last_stable_block: Option<BlockEnd>,
    lines: Vec<MarkdownLine>,
}

impl StreamingLayout {
    /// Lays out `source`, which normally extends the previous source, and
    /// returns how many leading lines are unchanged since the previous update.
    pub fn update(&mut self, source: &str, columns: usize, ambiguous: AmbiguousWidth) -> usize {
        if columns != self.columns
            || ambiguous != self.ambiguous
            || !source.starts_with(&self.stable_source)
        {
            *self = Self {
                columns,
                ambiguous,
                ..Self::default()
            };
        }
        let unchanged = self.stable_line_count;
        self.lines.truncate(unchanged);
        if columns == 0 {
            return 0;
        }
        let base = self.stable_source.len();
        let blocks = parse(&source[base..], base);
        let mut writer = Writer {
            source,
            columns,
            ambiguous,
            lines: std::mem::take(&mut self.lines),
        };
        // A block is complete only once a blank line follows it: without one,
        // a later line can still turn a paragraph into a table row or join it.
        let stable_blocks = (1..blocks.len())
            .rev()
            .find(|&index| {
                has_blank_line(
                    source,
                    blocks[index - 1].range.end,
                    blocks[index].range.start,
                )
            })
            .unwrap_or(0);
        let (completed, tail) = blocks.split_at(stable_blocks);
        if let Some(first_tail_block) = tail.first()
            && !completed.is_empty()
        {
            self.last_stable_block =
                writer.write_sequence(completed, &[], &[], 0, self.last_stable_block);
            self.stable_source = source[..first_tail_block.range.start].to_owned();
            self.stable_line_count = writer.lines.len();
        }
        writer.write_sequence(tail, &[], &[], 0, self.last_stable_block);
        self.lines = writer.lines;
        unchanged
    }

    pub fn lines(&self) -> &[MarkdownLine] {
        &self.lines
    }
}

#[derive(Clone, Debug)]
struct Piece {
    text: String,
    marks: Marks,
    offset: usize,
}

#[derive(Debug)]
struct Block {
    kind: BlockKind,
    range: Range<usize>,
}

#[derive(Debug)]
enum BlockKind {
    Paragraph(Vec<Piece>),
    Heading(Vec<Piece>),
    Code(String),
    Rule,
    Quote(Vec<Block>),
    List {
        start: Option<u64>,
        items: Vec<Block>,
    },
    Item(Vec<Block>),
    Table {
        alignments: Vec<Alignment>,
        header: Vec<Vec<Piece>>,
        rows: Vec<Vec<Vec<Piece>>>,
    },
}

#[derive(Clone, Copy, Debug)]
struct BlockEnd {
    end: usize,
    heading: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum ContainerKind {
    Root,
    Quote,
    List(Option<u64>),
    Item,
}

struct Container {
    kind: ContainerKind,
    start: usize,
    blocks: Vec<Block>,
    /// Inline content of a tight list item, which has no paragraph events.
    implicit: Vec<Piece>,
}

impl Container {
    fn new(kind: ContainerKind, start: usize) -> Self {
        Self {
            kind,
            start,
            blocks: Vec::new(),
            implicit: Vec::new(),
        }
    }

    fn flush_implicit(&mut self) {
        let pieces = std::mem::take(&mut self.implicit);
        if let (Some(first), Some(last)) = (pieces.first(), pieces.last()) {
            let range = first.offset..last.offset + last.text.len();
            self.blocks.push(Block {
                kind: BlockKind::Paragraph(pieces),
                range,
            });
        }
    }
}

#[derive(Default)]
struct TableBuilder {
    alignments: Vec<Alignment>,
    header: Vec<Vec<Piece>>,
    rows: Vec<Vec<Vec<Piece>>>,
    row: Vec<Vec<Piece>>,
    cell: Option<Vec<Piece>>,
}

enum Leaf {
    Inline {
        heading: Option<u8>,
        start: usize,
        pieces: Vec<Piece>,
    },
    Code {
        start: usize,
        text: String,
    },
    Table {
        start: usize,
        table: TableBuilder,
    },
}

struct LinkFrame {
    destination: String,
    first_piece: usize,
}

struct TreeBuilder {
    containers: Vec<Container>,
    leaf: Option<Leaf>,
    strong: usize,
    emphasis: usize,
    links: Vec<LinkFrame>,
    image_destinations: Vec<String>,
    ignored: usize,
}

fn parse(source: &str, base: usize) -> Vec<Block> {
    let mut builder = TreeBuilder {
        containers: vec![Container::new(ContainerKind::Root, base)],
        leaf: None,
        strong: 0,
        emphasis: 0,
        links: Vec::new(),
        image_destinations: Vec::new(),
        ignored: 0,
    };
    let options = Options::ENABLE_TABLES;
    for (event, range) in Parser::new_ext(source, options).into_offset_iter() {
        builder.event(event, range.start + base..range.end + base);
    }
    let mut root = builder.containers.pop().expect("the root container");
    root.flush_implicit();
    root.blocks
}

impl TreeBuilder {
    fn event(&mut self, event: Event<'_>, range: Range<usize>) {
        if self.ignored > 0 {
            match event {
                Event::Start(Tag::HtmlBlock) => self.ignored += 1,
                Event::End(TagEnd::HtmlBlock) => self.ignored -= 1,
                _ => {}
            }
            return;
        }
        match event {
            Event::Start(tag) => self.start(tag, range),
            Event::End(tag) => self.end(tag, range),
            Event::Text(text) => {
                if let Some(Leaf::Code { text: code, .. }) = &mut self.leaf {
                    code.push_str(&text);
                } else if self.image_destinations.is_empty() {
                    self.push_piece(&text, self.marks(), range.start);
                }
            }
            Event::Code(text) => {
                let marks = Marks {
                    inline_code: true,
                    ..self.marks()
                };
                self.push_piece(&text, marks, range.start);
            }
            Event::InlineMath(text) | Event::DisplayMath(text) => {
                self.push_piece(&text, self.marks(), range.start)
            }
            Event::SoftBreak | Event::HardBreak => self.push_piece("\n", self.marks(), range.start),
            Event::Rule => {
                self.push_block(BlockKind::Rule, range);
            }
            Event::Html(_)
            | Event::InlineHtml(_)
            | Event::FootnoteReference(_)
            | Event::TaskListMarker(_) => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>, range: Range<usize>) {
        match tag {
            Tag::Paragraph => self.open_leaf(Leaf::Inline {
                heading: None,
                start: range.start,
                pieces: Vec::new(),
            }),
            Tag::Heading { level, .. } => self.open_leaf(Leaf::Inline {
                heading: Some(level as u8),
                start: range.start,
                pieces: Vec::new(),
            }),
            Tag::CodeBlock(_) => self.open_leaf(Leaf::Code {
                start: range.start,
                text: String::new(),
            }),
            Tag::Table(alignments) => self.open_leaf(Leaf::Table {
                start: range.start,
                table: TableBuilder {
                    alignments,
                    ..TableBuilder::default()
                },
            }),
            Tag::TableHead | Tag::TableRow => {
                if let Some(Leaf::Table { table, .. }) = &mut self.leaf {
                    table.row.clear();
                }
            }
            Tag::TableCell => {
                if let Some(Leaf::Table { table, .. }) = &mut self.leaf {
                    table.cell = Some(Vec::new());
                }
            }
            Tag::BlockQuote(_) => self.open_container(ContainerKind::Quote, range.start),
            Tag::List(start) => self.open_container(ContainerKind::List(start), range.start),
            Tag::Item => self.open_container(ContainerKind::Item, range.start),
            Tag::Emphasis => self.emphasis += 1,
            Tag::Strong => self.strong += 1,
            Tag::Link { dest_url, .. } => {
                let first_piece = self.pieces_mut().map_or(0, |pieces| pieces.len());
                self.links.push(LinkFrame {
                    destination: dest_url.into_string(),
                    first_piece,
                });
            }
            Tag::Image { dest_url, .. } => self.image_destinations.push(dest_url.into_string()),
            Tag::HtmlBlock => self.ignored += 1,
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd, range: Range<usize>) {
        match tag {
            TagEnd::Paragraph | TagEnd::Heading(_) => {
                if let Some(Leaf::Inline {
                    heading,
                    start,
                    pieces,
                }) = self.leaf.take()
                {
                    let kind = if heading.is_some() {
                        BlockKind::Heading(pieces)
                    } else {
                        BlockKind::Paragraph(pieces)
                    };
                    self.push_block(kind, start..range.end);
                }
            }
            TagEnd::CodeBlock => {
                if let Some(Leaf::Code { start, mut text }) = self.leaf.take() {
                    if text.ends_with('\n') {
                        text.pop();
                    }
                    self.push_block(BlockKind::Code(text), start..range.end);
                }
            }
            TagEnd::TableCell => {
                if let Some(Leaf::Table { table, .. }) = &mut self.leaf
                    && let Some(cell) = table.cell.take()
                {
                    table.row.push(cell);
                }
            }
            TagEnd::TableHead => {
                if let Some(Leaf::Table { table, .. }) = &mut self.leaf {
                    table.header = std::mem::take(&mut table.row);
                }
            }
            TagEnd::TableRow => {
                if let Some(Leaf::Table { table, .. }) = &mut self.leaf {
                    let row = std::mem::take(&mut table.row);
                    table.rows.push(row);
                }
            }
            TagEnd::Table => {
                if let Some(Leaf::Table { start, table }) = self.leaf.take() {
                    self.push_block(
                        BlockKind::Table {
                            alignments: table.alignments,
                            header: table.header,
                            rows: table.rows,
                        },
                        start..range.end,
                    );
                }
            }
            TagEnd::BlockQuote(_) | TagEnd::List(_) | TagEnd::Item => {
                self.close_container(range.end)
            }
            TagEnd::Emphasis => self.emphasis = self.emphasis.saturating_sub(1),
            TagEnd::Strong => self.strong = self.strong.saturating_sub(1),
            TagEnd::Link => self.close_link(range.start),
            TagEnd::Image => {
                if let Some(destination) = self.image_destinations.pop() {
                    self.push_piece(&destination, self.marks(), range.start);
                }
            }
            _ => {}
        }
    }

    fn marks(&self) -> Marks {
        Marks {
            strong: self.strong > 0,
            emphasis: self.emphasis > 0,
            heading: match &self.leaf {
                Some(Leaf::Inline { heading, .. }) => *heading,
                _ => None,
            },
            link: !self.links.is_empty(),
            quote: self
                .containers
                .iter()
                .any(|container| container.kind == ContainerKind::Quote),
            ..Marks::default()
        }
    }

    fn open_leaf(&mut self, leaf: Leaf) {
        self.current_container().flush_implicit();
        self.leaf = Some(leaf);
    }

    fn open_container(&mut self, kind: ContainerKind, start: usize) {
        self.current_container().flush_implicit();
        self.containers.push(Container::new(kind, start));
    }

    fn close_container(&mut self, end: usize) {
        if self.containers.len() <= 1 {
            return;
        }
        let mut container = self.containers.pop().expect("a nested container");
        container.flush_implicit();
        let kind = match container.kind {
            ContainerKind::Quote => BlockKind::Quote(container.blocks),
            ContainerKind::List(start) => BlockKind::List {
                start,
                items: container.blocks,
            },
            ContainerKind::Item => BlockKind::Item(container.blocks),
            ContainerKind::Root => unreachable!("the root container is never closed"),
        };
        self.push_block(kind, container.start..end);
    }

    fn push_block(&mut self, kind: BlockKind, range: Range<usize>) {
        let container = self.current_container();
        container.flush_implicit();
        container.blocks.push(Block { kind, range });
    }

    fn current_container(&mut self) -> &mut Container {
        self.containers.last_mut().expect("the root container")
    }

    fn pieces_mut(&mut self) -> Option<&mut Vec<Piece>> {
        match &mut self.leaf {
            Some(Leaf::Inline { pieces, .. }) => Some(pieces),
            Some(Leaf::Table { table, .. }) => table.cell.as_mut(),
            Some(Leaf::Code { .. }) => None,
            None => self
                .containers
                .last_mut()
                .map(|container| &mut container.implicit),
        }
    }

    fn push_piece(&mut self, text: &str, marks: Marks, offset: usize) {
        if let Some(pieces) = self.pieces_mut() {
            pieces.push(Piece {
                text: text.to_owned(),
                marks,
                offset,
            });
        }
    }

    fn close_link(&mut self, offset: usize) {
        let Some(link) = self.links.pop() else {
            return;
        };
        let marks = self.marks();
        let Some(pieces) = self.pieces_mut() else {
            return;
        };
        let first_piece = link.first_piece.min(pieces.len());
        let label: String = pieces[first_piece..]
            .iter()
            .map(|piece| piece.text.as_str())
            .collect();
        if let Some(address) = link.destination.strip_prefix("mailto:") {
            pieces.truncate(first_piece);
            pieces.push(Piece {
                text: address.to_owned(),
                marks,
                offset,
            });
        } else if label.is_empty() || label == link.destination {
            pieces.truncate(first_piece);
            pieces.push(Piece {
                text: link.destination,
                marks: Marks {
                    link: true,
                    ..marks
                },
                offset,
            });
        } else {
            pieces.push(Piece {
                text: format!(" ({})", link.destination),
                marks,
                offset,
            });
        }
    }
}

#[derive(Clone, Debug)]
struct Cell {
    grapheme: String,
    marks: Marks,
    offset: usize,
    width: usize,
}

impl Cell {
    fn is_space(&self) -> bool {
        self.grapheme == " "
    }
}

type Prefix = [(String, Marks)];

struct Writer<'a> {
    source: &'a str,
    columns: usize,
    ambiguous: AmbiguousWidth,
    lines: Vec<MarkdownLine>,
}

impl Writer<'_> {
    /// Writes sibling blocks. `first` prefixes the first row written and
    /// `rest` every later row; the two always have the same width.
    fn write_sequence(
        &mut self,
        blocks: &[Block],
        first: &Prefix,
        rest: &Prefix,
        list_level: usize,
        mut previous: Option<BlockEnd>,
    ) -> Option<BlockEnd> {
        for (index, block) in blocks.iter().enumerate() {
            if let Some(previous) = previous
                && self.separated(previous, block.range.start)
            {
                self.emit(&[], Vec::new(), block.range.start);
            }
            let prefix = if index == 0 { first } else { rest };
            self.write_block(block, prefix, rest, list_level);
            previous = Some(BlockEnd {
                end: block.range.end,
                heading: matches!(block.kind, BlockKind::Heading(_)),
            });
        }
        previous
    }

    /// Blocks are separated by one blank row when the source separates them
    /// with a blank line, and always after a heading.
    fn separated(&self, previous: BlockEnd, next_start: usize) -> bool {
        previous.heading || has_blank_line(self.source, previous.end, next_start)
    }

    fn write_block(&mut self, block: &Block, first: &Prefix, rest: &Prefix, list_level: usize) {
        match &block.kind {
            BlockKind::Paragraph(pieces) | BlockKind::Heading(pieces) => {
                self.write_inline(pieces, first, rest, block.range.start)
            }
            BlockKind::Code(text) => self.write_code(text, first, rest, block.range.start),
            BlockKind::Rule => {
                let cells = self.cells(RULE, Marks::default(), block.range.start);
                self.write_rows(&cells, first, rest, block.range.start);
            }
            BlockKind::Quote(blocks) => {
                let bar = (
                    QUOTE_BAR.to_owned(),
                    Marks {
                        quote_bar: true,
                        ..Marks::default()
                    },
                );
                let first = [first, std::slice::from_ref(&bar)].concat();
                let rest = [rest, std::slice::from_ref(&bar)].concat();
                self.write_sequence(blocks, &first, &rest, list_level, None);
            }
            BlockKind::List { start, items } => {
                self.write_list(*start, items, first, rest, list_level)
            }
            BlockKind::Item(blocks) => {
                self.write_sequence(blocks, first, rest, list_level, None);
            }
            BlockKind::Table {
                alignments,
                header,
                rows,
            } => self.write_table(alignments, header, rows, first, rest, block.range.start),
        }
    }

    fn write_list(
        &mut self,
        start: Option<u64>,
        items: &[Block],
        first: &Prefix,
        rest: &Prefix,
        list_level: usize,
    ) {
        let mut previous = None;
        for (index, item) in items.iter().enumerate() {
            if let Some(previous) = previous
                && self.separated(previous, item.range.start)
            {
                self.emit(&[], Vec::new(), item.range.start);
            }
            let marker = match start {
                Some(start) => format!("{}. ", list_number(list_level, start + index as u64)),
                None => UNORDERED_MARKER.to_owned(),
            };
            let indent = " ".repeat(width(&marker, self.ambiguous));
            let lead = if index == 0 { first } else { rest };
            let marker = [(marker, Marks::default())];
            let indent = [(indent, Marks::default())];
            let item_first = [lead, &marker[..]].concat();
            let item_rest = [rest, &indent[..]].concat();
            match &item.kind {
                BlockKind::Item(blocks) if !blocks.is_empty() => {
                    self.write_sequence(blocks, &item_first, &item_rest, list_level + 1, None);
                }
                _ => self.emit(&item_first, Vec::new(), item.range.start),
            }
            previous = Some(BlockEnd {
                end: item.range.end,
                heading: false,
            });
        }
    }

    fn write_inline(&mut self, pieces: &[Piece], first: &Prefix, rest: &Prefix, offset: usize) {
        let mut logical_line = Vec::new();
        let mut is_first_line = true;
        for piece in pieces {
            for (index, segment) in piece.text.split('\n').enumerate() {
                if index > 0 {
                    let prefix = if is_first_line { first } else { rest };
                    self.write_rows(&logical_line, prefix, rest, offset);
                    logical_line.clear();
                    is_first_line = false;
                }
                let text = segment.replace('\t', " ");
                logical_line.extend(self.cells(&text, piece.marks, piece.offset));
            }
        }
        let prefix = if is_first_line { first } else { rest };
        self.write_rows(&logical_line, prefix, rest, offset);
    }

    fn write_code(&mut self, text: &str, first: &Prefix, rest: &Prefix, offset: usize) {
        let marks = Marks::default();
        let mut line_offset = offset;
        for (index, line) in text.split('\n').enumerate() {
            let cells = self.cells(&expand_tabs(line), marks, line_offset);
            let prefix = if index == 0 { first } else { rest };
            self.write_rows(&cells, prefix, rest, line_offset);
            line_offset += line.len() + 1;
        }
    }

    fn write_rows(&mut self, cells: &[Cell], first: &Prefix, rest: &Prefix, offset: usize) {
        let (first, rest, available) = self.fit_prefixes(first, rest);
        for (index, row) in wrap(cells, available, true).into_iter().enumerate() {
            let row_offset = row.first().map_or(offset, |cell| cell.offset);
            self.emit(if index == 0 { first } else { rest }, row, row_offset);
        }
    }

    /// Drops the prefixes when they would leave no room for content.
    fn fit_prefixes<'p>(
        &self,
        first: &'p Prefix,
        rest: &'p Prefix,
    ) -> (&'p Prefix, &'p Prefix, usize) {
        let prefix_width = prefix_width(rest, self.ambiguous);
        if prefix_width < self.columns {
            (first, rest, self.columns - prefix_width)
        } else {
            (&[], &[], self.columns)
        }
    }

    fn emit(&mut self, prefix: &Prefix, cells: Vec<Cell>, offset: usize) {
        let previous_offset = self.lines.last().map_or(0, |line| line.source_offset);
        let mut line = MarkdownLine {
            source_offset: offset.max(previous_offset),
            spans: Vec::new(),
        };
        for (text, marks) in prefix {
            line.push(text, *marks);
        }
        for cell in &cells {
            line.push(&cell.grapheme, cell.marks);
        }
        line.trim_end();
        self.lines.push(line);
    }

    fn cells(&self, text: &str, marks: Marks, offset: usize) -> Vec<Cell> {
        text.grapheme_indices(true)
            .filter(|(_, grapheme)| !grapheme.chars().any(char::is_control))
            .map(|(index, grapheme)| Cell {
                grapheme: grapheme.to_owned(),
                marks,
                offset: offset + index,
                width: width(grapheme, self.ambiguous),
            })
            .collect()
    }

    fn write_table(
        &mut self,
        alignments: &[Alignment],
        header: &[Vec<Piece>],
        rows: &[Vec<Vec<Piece>>],
        first: &Prefix,
        rest: &Prefix,
        offset: usize,
    ) {
        let (first, rest, columns) = self.fit_prefixes(first, rest);
        let header: Vec<Vec<Cell>> = header.iter().map(|cell| self.table_cell(cell)).collect();
        if header.is_empty() {
            return;
        }
        let rows: Vec<Vec<Vec<Cell>>> = rows
            .iter()
            .map(|row| {
                let mut cells: Vec<Vec<Cell>> =
                    row.iter().map(|cell| self.table_cell(cell)).collect();
                cells.resize(header.len(), Vec::new());
                cells
            })
            .collect();
        let table = TableLayout::new(&header, &rows, columns, self.ambiguous);
        let lines = table
            .horizontal(&header, &rows, alignments, columns)
            .unwrap_or_else(|| table_vertical(&header, &rows, columns, self.ambiguous));
        for (index, cells) in lines.into_iter().enumerate() {
            self.emit(if index == 0 { first } else { rest }, cells, offset);
        }
    }

    fn table_cell(&self, pieces: &[Piece]) -> Vec<Cell> {
        pieces
            .iter()
            .flat_map(|piece| {
                self.cells(
                    &piece.text.replace(['\n', '\t'], " "),
                    piece.marks,
                    piece.offset,
                )
            })
            .collect()
    }
}

/// Whether the source between a block ending at `end` and the next block has a
/// blank line, counting quote-only lines such as `>` as blank.
fn has_blank_line(source: &str, end: usize, next_start: usize) -> bool {
    let content_end = source[..end.min(next_start)].trim_end().len();
    let gap: Vec<&str> = source[content_end..next_start].split('\n').collect();
    gap.len() > 2
        && gap[1..gap.len() - 1].iter().any(|line| {
            line.trim_matches(|character: char| character.is_whitespace() || character == '>')
                .is_empty()
        })
}

fn prefix_width(prefix: &Prefix, ambiguous: AmbiguousWidth) -> usize {
    prefix.iter().map(|(text, _)| width(text, ambiguous)).sum()
}

fn list_number(list_level: usize, number: u64) -> String {
    match list_level {
        1 if number > 0 => number_to_letter(number),
        2 if number > 0 => number_to_roman(number),
        _ => number.to_string(),
    }
}

fn number_to_letter(mut number: u64) -> String {
    let mut letters = Vec::new();
    while number > 0 {
        number -= 1;
        letters.push((b'a' + (number % 26) as u8) as char);
        number /= 26;
    }
    letters.iter().rev().collect()
}

fn number_to_roman(mut number: u64) -> String {
    const NUMERALS: [(u64, &str); 13] = [
        (1000, "m"),
        (900, "cm"),
        (500, "d"),
        (400, "cd"),
        (100, "c"),
        (90, "xc"),
        (50, "l"),
        (40, "xl"),
        (10, "x"),
        (9, "ix"),
        (5, "v"),
        (4, "iv"),
        (1, "i"),
    ];
    let mut roman = String::new();
    for (value, numeral) in NUMERALS {
        while number >= value {
            roman.push_str(numeral);
            number -= value;
        }
    }
    roman
}

fn expand_tabs(line: &str) -> String {
    let mut expanded = String::with_capacity(line.len());
    let mut column = 0;
    for character in line.chars() {
        if character == '\t' {
            let spaces = TAB_COLUMNS - column % TAB_COLUMNS;
            expanded.extend(std::iter::repeat_n(' ', spaces));
            column += spaces;
        } else {
            expanded.push(character);
            column += 1;
        }
    }
    expanded
}

fn cells_width(cells: &[Cell]) -> usize {
    cells.iter().map(|cell| cell.width).sum()
}

/// East Asian text has no spaces, so a row may also break next to a wide
/// character, except before closing or after opening punctuation.
fn breaks_between(previous: &Cell, next: &Cell) -> bool {
    const CLOSING: &str = "，。、；：！？）」』】》〉…—％,.;:!?)]}”’";
    const OPENING: &str = "（「『【《〈([{“‘";
    (previous.width > 1 || next.width > 1)
        && !CLOSING.contains(next.grapheme.as_str())
        && !OPENING.contains(previous.grapheme.as_str())
}

/// Wraps like wrap-ansi with `{ hard, trim: false, wordWrap: true }`: breaks at
/// spaces, splits words longer than a row only when `hard`, and keeps leading
/// whitespace. Graphemes wider than a whole row are dropped.
fn wrap(cells: &[Cell], columns: usize, hard: bool) -> Vec<Vec<Cell>> {
    let columns = columns.max(1);
    let mut words = Vec::new();
    let mut separator = None;
    let mut word_start = 0;
    for (index, cell) in cells.iter().enumerate() {
        if cell.is_space() {
            words.push((separator, &cells[word_start..index]));
            separator = Some(cell);
            word_start = index + 1;
        } else if index > word_start && breaks_between(&cells[index - 1], cell) {
            words.push((separator, &cells[word_start..index]));
            separator = None;
            word_start = index;
        }
    }
    words.push((separator, &cells[word_start..]));

    let mut rows = vec![Vec::new()];
    let mut row_width = 0;
    for (separator, word) in words {
        // A space that falls exactly on a row break is dropped rather than
        // starting the next row, which wrap-ansi with `trim: false` would do.
        if let Some(separator) = separator
            && row_width < columns
        {
            rows.last_mut().unwrap().push(separator.clone());
            row_width += separator.width;
        }
        let word: Vec<&Cell> = word.iter().filter(|cell| cell.width <= columns).collect();
        let word_width: usize = word.iter().map(|cell| cell.width).sum();
        if hard && word_width > columns {
            let remaining = columns.saturating_sub(row_width);
            let breaks_starting_this_row = 1 + (word_width - remaining - 1) / columns;
            let breaks_starting_next_row = (word_width - 1) / columns;
            if breaks_starting_next_row < breaks_starting_this_row {
                rows.push(Vec::new());
                row_width = 0;
            }
            for cell in word {
                if row_width > 0 && row_width + cell.width > columns {
                    rows.push(Vec::new());
                    row_width = 0;
                }
                rows.last_mut().unwrap().push(cell.clone());
                row_width += cell.width;
            }
            continue;
        }
        if row_width > 0 && word_width > 0 && row_width + word_width > columns {
            rows.push(Vec::new());
            row_width = 0;
        }
        rows.last_mut().unwrap().extend(word.into_iter().cloned());
        row_width += word_width;
    }
    rows
}

fn trimmed_rows(cells: &[Cell], columns: usize, hard: bool) -> Vec<Vec<Cell>> {
    let rows: Vec<Vec<Cell>> = wrap(cells, columns, hard)
        .into_iter()
        .map(|mut row| {
            while row.last().is_some_and(Cell::is_space) {
                row.pop();
            }
            row
        })
        .filter(|row| !row.is_empty())
        .collect();
    if rows.is_empty() {
        vec![Vec::new()]
    } else {
        rows
    }
}

struct TableLayout {
    widths: Vec<usize>,
    hard: bool,
    ambiguous: AmbiguousWidth,
}

impl TableLayout {
    /// Column widths from `MarkdownTable`: ideal widths when they fit, else
    /// longest-word widths plus a proportional share of the spare room, else
    /// proportionally shrunk widths that need hard wrapping.
    fn new(
        header: &[Vec<Cell>],
        rows: &[Vec<Vec<Cell>>],
        columns: usize,
        ambiguous: AmbiguousWidth,
    ) -> Self {
        let column_count = header.len();
        let column_cells = |index: usize| {
            std::iter::once(&header[index]).chain(rows.iter().map(move |row| &row[index]))
        };
        let min_widths: Vec<usize> = (0..column_count)
            .map(|index| {
                column_cells(index)
                    .flat_map(|cells| cells.split(Cell::is_space).map(cells_width))
                    .max()
                    .unwrap_or(0)
                    .max(MIN_COLUMN_WIDTH)
            })
            .collect();
        let ideal_widths: Vec<usize> = (0..column_count)
            .map(|index| {
                column_cells(index)
                    .map(|cells| cells_width(cells))
                    .max()
                    .unwrap_or(0)
                    .max(MIN_COLUMN_WIDTH)
            })
            .collect();
        let border_overhead = 1 + column_count * COLUMN_BORDER_WIDTH;
        let available = columns
            .saturating_sub(border_overhead + TABLE_SAFETY_MARGIN)
            .max(column_count * MIN_COLUMN_WIDTH);
        let total_min: usize = min_widths.iter().sum();
        let total_ideal: usize = ideal_widths.iter().sum();
        if total_ideal <= available {
            return Self {
                widths: ideal_widths,
                hard: false,
                ambiguous,
            };
        }
        if total_min <= available {
            let extra_space = available - total_min;
            let overflows: Vec<usize> = ideal_widths
                .iter()
                .zip(&min_widths)
                .map(|(ideal, min)| ideal - min)
                .collect();
            let total_overflow: usize = overflows.iter().sum();
            let widths = min_widths
                .iter()
                .zip(&overflows)
                .map(|(min, overflow)| {
                    min + (overflow * extra_space)
                        .checked_div(total_overflow)
                        .unwrap_or(0)
                })
                .collect();
            return Self {
                widths,
                hard: false,
                ambiguous,
            };
        }
        let widths = min_widths
            .iter()
            .map(|min| (min * available / total_min).max(MIN_COLUMN_WIDTH))
            .collect();
        Self {
            widths,
            hard: true,
            ambiguous,
        }
    }

    fn horizontal(
        &self,
        header: &[Vec<Cell>],
        rows: &[Vec<Vec<Cell>>],
        alignments: &[Alignment],
        columns: usize,
    ) -> Option<Vec<Vec<Cell>>> {
        let wrap_row = |row: &[Vec<Cell>]| -> Vec<Vec<Vec<Cell>>> {
            row.iter()
                .zip(&self.widths)
                .map(|(cells, width)| trimmed_rows(cells, *width, self.hard))
                .collect()
        };
        let wrapped_header = wrap_row(header);
        let wrapped_rows: Vec<_> = rows.iter().map(|row| wrap_row(row)).collect();
        let max_row_lines = std::iter::once(&wrapped_header)
            .chain(&wrapped_rows)
            .flat_map(|row| row.iter().map(Vec::len))
            .max()
            .unwrap_or(1);
        if max_row_lines > MAX_TABLE_ROW_LINES {
            return None;
        }
        let mut lines = vec![self.border('┌', '┬', '┐')];
        lines.extend(self.row_lines(&wrapped_header, |_| Some(Alignment::Center)));
        lines.push(self.border('├', '┼', '┤'));
        for (index, row) in wrapped_rows.iter().enumerate() {
            lines.extend(self.row_lines(row, |column| alignments.get(column).copied()));
            if index + 1 < wrapped_rows.len() {
                lines.push(self.border('├', '┼', '┤'));
            }
        }
        lines.push(self.border('└', '┴', '┘'));
        let widest = lines
            .iter()
            .map(|line| cells_width(line))
            .max()
            .unwrap_or(0);
        (widest <= columns.saturating_sub(TABLE_SAFETY_MARGIN)).then_some(lines)
    }

    fn border(&self, left: char, cross: char, right: char) -> Vec<Cell> {
        let mut text = String::from(left);
        for (index, width) in self.widths.iter().enumerate() {
            text.extend(std::iter::repeat_n('─', width + COLUMN_BORDER_WIDTH - 1));
            text.push(if index + 1 < self.widths.len() {
                cross
            } else {
                right
            });
        }
        plain_cells(&text, self.ambiguous)
    }

    fn row_lines(
        &self,
        row: &[Vec<Vec<Cell>>],
        alignment: impl Fn(usize) -> Option<Alignment>,
    ) -> Vec<Vec<Cell>> {
        let height = row.iter().map(Vec::len).max().unwrap_or(1);
        (0..height)
            .map(|line_index| {
                let mut line = plain_cells("│", self.ambiguous);
                for (column, (cell_lines, width)) in row.iter().zip(&self.widths).enumerate() {
                    let top_padding = (height - cell_lines.len()) / 2;
                    let content = line_index
                        .checked_sub(top_padding)
                        .and_then(|index| cell_lines.get(index))
                        .cloned()
                        .unwrap_or_default();
                    line.extend(plain_cells(" ", self.ambiguous));
                    line.extend(pad_aligned(content, *width, alignment(column)));
                    line.extend(plain_cells(" │", self.ambiguous));
                }
                line
            })
            .collect()
    }
}

fn pad_aligned(content: Vec<Cell>, target: usize, alignment: Option<Alignment>) -> Vec<Cell> {
    let padding = target.saturating_sub(cells_width(&content));
    let (left, right) = match alignment {
        Some(Alignment::Center) => (padding / 2, padding - padding / 2),
        Some(Alignment::Right) => (padding, 0),
        _ => (0, padding),
    };
    let mut padded = plain_cells(&" ".repeat(left), AmbiguousWidth::Narrow);
    padded.extend(content);
    padded.extend(plain_cells(&" ".repeat(right), AmbiguousWidth::Narrow));
    padded
}

/// Unmarked cells for table borders, padding and separators.
fn plain_cells(text: &str, ambiguous: AmbiguousWidth) -> Vec<Cell> {
    text.graphemes(true)
        .map(|grapheme| Cell {
            grapheme: grapheme.to_owned(),
            marks: Marks::default(),
            offset: 0,
            width: width(grapheme, ambiguous),
        })
        .collect()
}

/// The key-value format `MarkdownTable` falls back to when a table does not
/// fit: each row lists `header: value` pairs, rows separated by a rule.
fn table_vertical(
    header: &[Vec<Cell>],
    rows: &[Vec<Vec<Cell>>],
    columns: usize,
    ambiguous: AmbiguousWidth,
) -> Vec<Vec<Cell>> {
    let separator_width = columns.saturating_sub(1).min(MAX_VERTICAL_SEPARATOR_WIDTH);
    let labels: Vec<String> = header
        .iter()
        .enumerate()
        .map(|(index, cells)| {
            let label: String = cells.iter().map(|cell| cell.grapheme.as_str()).collect();
            if label.trim().is_empty() {
                format!("Column {}", index + 1)
            } else {
                label
            }
        })
        .collect();
    let continuation_width = columns.saturating_sub(VERTICAL_CONTINUATION_INDENT.len() + 1);
    let mut lines = Vec::new();
    for (row_index, row) in rows.iter().enumerate() {
        if row_index > 0 {
            lines.push(plain_cells(&"─".repeat(separator_width), ambiguous));
        }
        for (label, value) in labels.iter().zip(row) {
            let value = collapse_whitespace(value);
            let first_width = columns
                .saturating_sub(width(label, ambiguous) + VERTICAL_LABEL_SEPARATOR_WIDTH + 1);
            let mut value_rows =
                trimmed_rows(&value, first_width.max(MIN_VERTICAL_VALUE_WIDTH), true);
            if value_rows.len() > 1 && continuation_width > first_width {
                let mut remaining = Vec::new();
                for (index, row) in value_rows.drain(1..).enumerate() {
                    if index > 0 {
                        remaining.extend(plain_cells(" ", ambiguous));
                    }
                    remaining.extend(row);
                }
                value_rows.extend(trimmed_rows(&remaining, continuation_width, true));
            }
            let mut first_line = plain_cells(&format!("{label}:"), ambiguous);
            for cell in &mut first_line {
                cell.marks.strong = true;
            }
            first_line.extend(plain_cells(" ", ambiguous));
            let mut value_rows = value_rows.into_iter();
            first_line.extend(value_rows.next().unwrap_or_default());
            lines.push(first_line);
            for row in value_rows.filter(|row| !row.is_empty()) {
                let mut line = plain_cells(VERTICAL_CONTINUATION_INDENT, ambiguous);
                line.extend(row);
                lines.push(line);
            }
        }
    }
    lines
}

fn collapse_whitespace(cells: &[Cell]) -> Vec<Cell> {
    let mut collapsed: Vec<Cell> = Vec::with_capacity(cells.len());
    for cell in cells {
        if cell.is_space() && collapsed.last().is_none_or(Cell::is_space) {
            continue;
        }
        collapsed.push(cell.clone());
    }
    while collapsed.last().is_some_and(Cell::is_space) {
        collapsed.pop();
    }
    collapsed
}
