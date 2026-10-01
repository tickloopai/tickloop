use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    sync::atomic::{AtomicU64, Ordering},
};

use tickloop_ui::{
    markdown::{MarkdownLine, Marks, StreamingLayout},
    settings::AmbiguousWidth,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::{
    display::{DisplayProfile, Glyph},
    hyperlink::Hyperlink,
    running_shimmer::RunningShimmer,
    style::{Role, RoleLine},
};

const TAB_COLUMNS: usize = 4;
const DEFAULT_WIDTH: usize = 80;
const DEFAULT_HEIGHT: usize = 22;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ScrollAction {
    HalfPageUp,
    HalfPageDown,
    LinesUp(usize),
    LinesDown(usize),
    First,
    Last,
}

pub(super) struct ScrollFrame {
    pub(super) lines: Vec<String>,
    /// The same rows as `lines`, described by role.
    pub(super) styled: Vec<RoleLine>,
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) following: bool,
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) unread: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
struct Position {
    line: usize,
    byte: usize,
    tab_cell: usize,
}

struct Row {
    start: Position,
    text: String,
    styled: RoleLine,
}

/// Roles and links recorded for one logical line, parallel to its text. Marks
/// and links are byte ranges of that text; `hang` indents the rows a long line
/// wraps onto, which is how Claude Code aligns a message's continuation rows
/// under its content.
#[derive(Debug, Default)]
struct LineStyle {
    marks: Vec<(Range<usize>, Role)>,
    links: Vec<(Range<usize>, Hyperlink)>,
    fill: Option<Role>,
    hang: usize,
    markdown: Option<Box<MarkdownBlock>>,
    /// Lines that produce no rows, such as a superseded completion notice.
    hidden: bool,
}

/// An assistant reply held as Markdown source and laid out again for each
/// width. The whole reply is one logical line whose text stays empty; its rows
/// come from the layout, with `lead` in front of the first row and a hanging
/// indent of the same width under it.
#[derive(Debug)]
struct MarkdownBlock {
    lead: String,
    source: String,
    layout: StreamingLayout,
    /// Rows produced by each layout line, so rows of an unchanged prefix can
    /// be kept while the reply streams.
    row_counts: Vec<usize>,
}

impl MarkdownBlock {
    fn new(lead: &str) -> Self {
        Self {
            lead: lead.to_owned(),
            source: String::new(),
            layout: StreamingLayout::default(),
            row_counts: Vec::new(),
        }
    }

    fn hang(&self, width: usize) -> usize {
        let hang = self.lead.width();
        if hang < width { hang } else { 0 }
    }
}

/// A message whose later logical lines are indented to sit under its first
/// line's content rather than under its gutter glyph.
#[derive(Clone, Copy, Debug)]
struct Block {
    lead: &'static str,
    hang: usize,
}

/// Names one input echo in one scrollback. It means nothing to any other scrollback, including
/// the fresh one a reset or reload puts in its place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct InputEcho {
    scrollback: u64,
    input: u64,
}

static NEXT_SCROLLBACK: AtomicU64 = AtomicU64::new(1);

pub(super) struct Scrollback {
    id: u64,
    lines: Vec<String>,
    line_styles: Vec<LineStyle>,
    block: Option<Block>,
    rows: Vec<Row>,
    dirty_from: Option<usize>,
    width: usize,
    height: usize,
    anchor: Position,
    following: bool,
    auto_scroll: bool,
    seen_messages: HashSet<String>,
    unread: usize,
    /// A live row drawn below the conversation while a run works; it is not
    /// part of the logical lines, so it disappears once cleared.
    activity: Option<RoleLine>,
    /// Header lines of tool calls still executing, by call id.
    running_tool_calls: HashMap<String, usize>,
    running_shimmer: RunningShimmer,
    completion_notice: Option<Range<usize>>,
    /// Logical lines of each input echo, oldest first.
    inputs: Vec<(u64, Range<usize>)>,
    next_input: u64,
}

impl Default for Scrollback {
    fn default() -> Self {
        Self {
            id: NEXT_SCROLLBACK.fetch_add(1, Ordering::Relaxed),
            lines: vec![String::new()],
            line_styles: vec![LineStyle::default()],
            block: None,
            rows: Vec::new(),
            dirty_from: Some(0),
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
            anchor: Position::default(),
            following: true,
            auto_scroll: true,
            seen_messages: HashSet::new(),
            unread: 0,
            activity: None,
            running_tool_calls: HashMap::new(),
            running_shimmer: RunningShimmer::default(),
            completion_notice: None,
            inputs: Vec::new(),
            next_input: 0,
        }
    }
}

impl Scrollback {
    pub(super) fn is_following(&self) -> bool {
        self.following
    }

    pub(super) fn is_at_bottom(&self) -> bool {
        self.top_row() == self.row_count().saturating_sub(self.height)
    }

    pub(super) fn has_hidden_tail(&mut self, width: usize, height: usize) -> bool {
        self.prepare_frame(width, height);
        !self.is_at_bottom()
    }

    pub(super) fn activity_line(&self) -> Option<RoleLine> {
        self.activity.clone()
    }

    pub(super) fn reading_prompt(&self, width: usize, height: usize) -> Option<String> {
        if self.following || height < 7 || width < 24 {
            return None;
        }
        let first_line = self.rows.get(self.top_row())?.start.line;
        if self.line_styles.get(first_line)?.fill == Some(Role::UserMessageBg) {
            return None;
        }
        let end = (0..first_line)
            .rev()
            .find(|index| self.line_styles[*index].fill == Some(Role::UserMessageBg))?;
        let mut start = end;
        while start > 0 && self.line_styles[start - 1].fill == Some(Role::UserMessageBg) {
            start -= 1;
        }
        let summary = self.lines[start..=end]
            .join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let summary = summary.chars().take(512).collect::<String>();
        let mut line = RoleLine::plain(summary);
        line.truncate(width);
        Some(line.text())
    }

    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn append(&mut self, text: &str) {
        self.append_as(text, None, None);
    }

    /// Appends `text` carrying `role`. Every logical line that receives a
    /// character is also painted with `fill` across the row, when given.
    pub(super) fn append_as(&mut self, text: &str, role: Option<Role>, fill: Option<Role>) {
        self.append_span(text, role, fill, None);
    }

    /// [`Self::append_as`] with the whole of `text` opening `link`.
    pub(super) fn append_span(
        &mut self,
        text: &str,
        role: Option<Role>,
        fill: Option<Role>,
        link: Option<&Hyperlink>,
    ) {
        if text.is_empty() {
            return;
        }
        if self.last_line_is_markdown() && !text.starts_with('\n') {
            self.start_line();
        }
        self.mark_last_line_dirty();
        let mut segment_start = self.lines.last().unwrap().len();
        for character in text.chars() {
            if character == '\n' {
                self.close_segment(segment_start, role, link);
                self.start_line();
                segment_start = self.lines.last().unwrap().len();
                continue;
            }
            let line = self.lines.last_mut().unwrap();
            match character {
                '\t' => line.push(character),
                character if character.is_control() => line.push('�'),
                character => line.push(character),
            }
            if fill.is_some() {
                self.line_styles.last_mut().unwrap().fill = fill;
            }
        }
        self.close_segment(segment_start, role, link);
    }

    /// Appends Markdown reply text. The text extends the reply on the last
    /// line unless `starts_message` is set or something else was written since,
    /// in which case a new reply starts with `lead` in front of it.
    pub(super) fn append_markdown(&mut self, text: &str, lead: &str, starts_message: bool) {
        if text.is_empty() {
            return;
        }
        let continues_reply = !starts_message && self.last_line_is_markdown();
        if !continues_reply {
            if !self.lines.last().unwrap().is_empty() || self.last_line_is_markdown() {
                self.start_line();
            }
            self.line_styles.last_mut().unwrap().markdown =
                Some(Box::new(MarkdownBlock::new(lead)));
        }
        self.mark_last_line_dirty();
        let block = self
            .line_styles
            .last_mut()
            .unwrap()
            .markdown
            .as_mut()
            .unwrap();
        block.source.push_str(text);
    }

    /// Appends a run's completion notice, which must begin with a newline, and
    /// hides the previous one so only the latest completion stays visible. The
    /// empty line the notice closes is hidden with it, keeping the spacing a
    /// conversation without notices would have.
    pub(super) fn append_completion_notice(&mut self, spans: &[(String, Option<Role>)]) {
        self.hide_completion_notice();
        let last_line = self.lines.len() - 1;
        let first_line = if self.lines[last_line].is_empty() && !self.last_line_is_markdown() {
            last_line
        } else {
            last_line + 1
        };
        for (text, role) in spans {
            self.append_as(text, *role, None);
        }
        self.completion_notice = Some(first_line..self.lines.len() - 1);
    }

    /// Hides the latest completion notice, as when another run starts.
    pub(super) fn hide_completion_notice(&mut self) {
        if let Some(notice) = self.completion_notice.take() {
            self.hide_lines(notice);
        }
    }

    pub(super) fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub(super) fn mark_input(&mut self, lines: Range<usize>) -> InputEcho {
        self.next_input += 1;
        self.inputs.push((self.next_input, lines));
        InputEcho {
            scrollback: self.id,
            input: self.next_input,
        }
    }

    /// Hides `echo`, as when its input was not sent or is shown in another view. An echo of
    /// another scrollback names nothing here.
    pub(super) fn hide_echo(&mut self, echo: InputEcho) {
        if echo.scrollback != self.id {
            return;
        }
        if let Some(index) = self
            .inputs
            .iter()
            .position(|(input, _)| *input == echo.input)
        {
            let (_, lines) = self.inputs.remove(index);
            self.hide_lines(lines.start..lines.end.min(self.lines.len()));
        }
    }

    /// Draws `current` as the continuation of `previous`, one block in the body, when `current`
    /// directly follows `previous` here: the blank line between them is hidden and `current`'s
    /// `prompt` lead becomes spaces of the same width. Its text, links and fill stay as they are.
    /// Echoes with anything drawn between them, or from another scrollback, stay apart.
    pub(super) fn join_inputs(
        &mut self,
        previous: InputEcho,
        current: InputEcho,
        prompt: &str,
    ) -> bool {
        let lines_of = |echo: InputEcho| {
            (echo.scrollback == self.id)
                .then(|| self.inputs.iter().find(|(input, _)| *input == echo.input))
                .flatten()
                .map(|(_, lines)| lines.clone())
        };
        let (Some(previous), Some(current)) = (lines_of(previous), lines_of(current)) else {
            return false;
        };
        let separator = previous.end;
        let adjacent = current.start == separator + 1
            && current.end <= self.lines.len()
            && self.lines[separator].is_empty()
            && !self.line_styles[separator..current.end]
                .iter()
                .any(|style| style.hidden);
        if !adjacent {
            return false;
        }
        self.hide_lines(separator..separator + 1);
        let lead = " ".repeat(prompt.width());
        remap_prefix(
            &mut self.lines[current.start],
            &mut self.line_styles[current.start],
            0,
            prompt,
            &lead,
            Role::Subtle,
        );
        true
    }

    fn hide_lines(&mut self, lines: Range<usize>) {
        for style in &mut self.line_styles[lines.clone()] {
            style.hidden = true;
        }
        self.dirty_from = Some(
            self.dirty_from
                .map_or(lines.start, |existing| existing.min(lines.start)),
        );
    }

    /// Appends a tool call header and shows its glyph as running until
    /// [`Self::finish_tool_call`]. The header is the line the spans open, or
    /// the current line when they do not begin with a newline.
    pub(super) fn append_tool_call(&mut self, call_id: &str, spans: &[(String, Option<Role>)]) {
        let opens_line = spans
            .first()
            .is_some_and(|(text, _)| text.starts_with('\n'));
        let header_line = self.lines.len() - usize::from(!opens_line);
        for (text, role) in spans {
            self.append_as(text, *role, None);
        }
        self.running_tool_calls
            .insert(call_id.to_owned(), header_line);
        self.running_shimmer.reset();
    }

    pub(super) fn finish_tool_call(&mut self, call_id: &str) {
        self.running_tool_calls.remove(call_id);
    }

    pub(super) fn finish_tool_calls(&mut self) {
        self.running_tool_calls.clear();
    }

    /// Returns whether the activity row changed.
    pub(super) fn set_activity(&mut self, activity: Option<RoleLine>) -> bool {
        if self.activity == activity {
            return false;
        }
        self.activity = activity;
        true
    }

    pub(super) fn remap_display(&mut self, previous: DisplayProfile, next: DisplayProfile) {
        if previous.preferences().glyph_set == next.preferences().glyph_set {
            return;
        }
        let previous_lead = format!("{} ", previous.glyph(Glyph::Assistant));
        for block in self
            .line_styles
            .iter_mut()
            .filter_map(|style| style.markdown.as_mut())
        {
            if block.lead == previous_lead {
                block.lead = format!("{} ", next.glyph(Glyph::Assistant));
            }
            block.layout = StreamingLayout::default();
            block.row_counts.clear();
        }
        for (line, style) in self.lines.iter_mut().zip(&mut self.line_styles) {
            remap_prefix(
                line,
                style,
                0,
                previous.glyph(Glyph::Assistant),
                next.glyph(Glyph::Assistant),
                Role::Text,
            );
            remap_prefix(
                line,
                style,
                0,
                previous.glyph(Glyph::ToolCall),
                next.glyph(Glyph::ToolCall),
                Role::MutedText,
            );
            remap_prefix(
                line,
                style,
                2,
                previous.glyph(Glyph::ToolResult),
                next.glyph(Glyph::ToolResult),
                Role::MutedText,
            );
            remap_prefix(
                line,
                style,
                2,
                previous.glyph(Glyph::ToolResult),
                next.glyph(Glyph::ToolResult),
                Role::Error,
            );
            for (glyph, role) in [
                (Glyph::Success, Role::Success),
                (Glyph::Failure, Role::Error),
                (Glyph::Warning, Role::Warning),
                (Glyph::Prompt, Role::Subtle),
            ] {
                remap_prefix(
                    line,
                    style,
                    0,
                    previous.glyph(glyph),
                    next.glyph(glyph),
                    role,
                );
            }
            remap_prefix(
                line,
                style,
                0,
                previous.spinner_frame(0),
                next.spinner_frame(0),
                Role::Accent,
            );
        }
        self.rows.clear();
        self.dirty_from = Some(0);
    }

    #[cfg(test)]
    /// Starts a message block: the current line wraps with `hang` columns of
    /// indent, and each later logical line begins with `lead`.
    pub(super) fn begin_block(&mut self, lead: &'static str, hang: usize) {
        self.block = Some(Block { lead, hang });
        self.line_styles.last_mut().unwrap().hang = hang;
    }

    #[cfg(test)]
    pub(super) fn end_block(&mut self) {
        self.block = None;
    }

    fn last_line_is_markdown(&self) -> bool {
        self.line_styles.last().unwrap().markdown.is_some()
    }

    fn mark_last_line_dirty(&mut self) {
        let changed_line = self.lines.len() - 1;
        self.dirty_from = Some(
            self.dirty_from
                .map_or(changed_line, |existing| existing.min(changed_line)),
        );
    }

    fn start_line(&mut self) {
        self.lines.push(String::new());
        let mut style = LineStyle::default();
        if let Some(block) = self.block {
            self.lines.last_mut().unwrap().push_str(block.lead);
            style.hang = block.hang;
        }
        self.line_styles.push(style);
    }

    fn close_segment(&mut self, start: usize, role: Option<Role>, link: Option<&Hyperlink>) {
        let end = self.lines.last().unwrap().len();
        if end == start {
            return;
        }
        let style = self.line_styles.last_mut().unwrap();
        if let Some(role) = role {
            style.marks.push((start..end, role));
        }
        if let Some(link) = link {
            style.links.push((start..end, link.clone()));
        }
    }

    #[cfg(test)]
    pub(super) fn frame(&mut self, width: usize, height: usize) -> ScrollFrame {
        self.frame_with_motion(width, height, false)
    }

    pub(super) fn frame_with_motion(
        &mut self,
        width: usize,
        height: usize,
        prefers_reduced_motion: bool,
    ) -> ScrollFrame {
        if width == 0 || height == 0 {
            return ScrollFrame {
                lines: Vec::new(),
                styled: Vec::new(),
                following: self.following,
                unread: self.unread,
            };
        }
        self.prepare_frame(width, height);
        let top = self.top_row();
        if top.saturating_add(height) >= self.row_count() {
            self.unread = 0;
        }
        let running_tool_rows = self.running_tool_rows();
        let styled: Vec<RoleLine> = self
            .rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let mut styled = row.styled.clone();
                if running_tool_rows.contains(&index) {
                    let spans = std::mem::take(&mut styled.spans);
                    for span in spans {
                        if span.role == Some(Role::ToolName) {
                            self.running_shimmer.append(
                                &mut styled,
                                &span.text,
                                prefers_reduced_motion,
                            );
                        } else {
                            styled.push(span.text, span.role);
                        }
                    }
                }
                styled
            })
            .chain(self.activity_rows())
            .skip(top)
            .take(height)
            .collect();
        ScrollFrame {
            lines: styled.iter().map(RoleLine::text).collect(),
            styled,
            following: self.following,
            unread: self.unread,
        }
    }

    fn activity_rows(&self) -> Vec<RoleLine> {
        let Some(activity) = &self.activity else {
            return Vec::new();
        };
        let separated = self.rows.last().is_none_or(|row| row.text.is_empty());
        let mut rows = Vec::with_capacity(2);
        if !separated {
            rows.push(RoleLine::default());
        }
        rows.push(activity.clone());
        rows
    }

    fn prepare_frame(&mut self, width: usize, height: usize) {
        if self.width != width {
            self.width = width;
            self.dirty_from = Some(0);
        }
        self.height = height;
        self.refresh_rows();
    }

    fn row_count(&self) -> usize {
        self.rows.len() + self.activity_rows().len()
    }

    fn running_tool_rows(&self) -> HashSet<usize> {
        self.running_tool_calls
            .values()
            .filter_map(|line| {
                let first_row = self.rows.partition_point(|row| row.start.line < *line);
                self.rows
                    .get(first_row)
                    .is_some_and(|row| row.start.line == *line)
                    .then_some(first_row)
            })
            .collect()
    }

    pub(super) fn scroll(&mut self, action: ScrollAction) {
        self.refresh_rows();
        let maximum = self.row_count().saturating_sub(self.height);
        let half_page = (self.height / 2).max(1);
        let target = match action {
            ScrollAction::HalfPageUp => self.top_row().saturating_sub(half_page),
            ScrollAction::HalfPageDown => self.top_row().saturating_add(half_page).min(maximum),
            ScrollAction::LinesUp(lines) => self.top_row().saturating_sub(lines),
            ScrollAction::LinesDown(lines) => self.top_row().saturating_add(lines).min(maximum),
            ScrollAction::First => 0,
            ScrollAction::Last => maximum,
        };
        self.anchor = self
            .rows
            .get(target.min(self.rows.len().saturating_sub(1)))
            .map_or(Position::default(), |row| row.start);
        let reached_bottom = target == maximum
            && matches!(
                action,
                ScrollAction::HalfPageDown | ScrollAction::LinesDown(_) | ScrollAction::Last
            );
        self.following = reached_bottom && self.auto_scroll;
        if reached_bottom {
            self.unread = 0;
        }
    }

    pub(super) fn note_message(&mut self, id: String) {
        if self.seen_messages.insert(id) && !self.following {
            self.unread = self.unread.saturating_add(1);
        }
    }

    pub(super) fn set_auto_scroll(&mut self, enabled: bool) {
        if enabled == self.auto_scroll {
            return;
        }
        self.refresh_rows();
        let top = self.top_row();
        if self.following {
            self.anchor = self
                .rows
                .get(top.min(self.rows.len().saturating_sub(1)))
                .map_or(Position::default(), |row| row.start);
        }
        self.auto_scroll = enabled;
        self.following = enabled && top == self.row_count().saturating_sub(self.height);
        if self.following {
            self.unread = 0;
        }
    }

    pub(super) fn auto_scroll(&self) -> bool {
        self.auto_scroll
    }

    fn top_row(&self) -> usize {
        let maximum = self.row_count().saturating_sub(self.height);
        if self.following {
            maximum
        } else {
            self.rows
                .partition_point(|row| row.start <= self.anchor)
                .saturating_sub(1)
                .min(maximum)
        }
    }

    fn refresh_rows(&mut self) {
        let Some(first_line) = self.dirty_from.take() else {
            return;
        };
        let first_row = self.rows.partition_point(|row| row.start.line < first_line);
        let first_line_rows = self.rows.len() - first_row;
        for (line, text) in self.lines.iter().enumerate().skip(first_line) {
            let style = &mut self.line_styles[line];
            if style.hidden {
                if line == first_line {
                    self.rows.truncate(first_row);
                }
                continue;
            }
            if let Some(block) = style.markdown.as_mut() {
                let existing_rows = if line == first_line {
                    first_line_rows
                } else {
                    0
                };
                let kept_rows = layout_markdown(block, self.width, existing_rows);
                if line == first_line {
                    self.rows.truncate(first_row + kept_rows);
                }
                push_markdown_rows(&mut self.rows, block, line, self.width);
            } else {
                if line == first_line {
                    self.rows.truncate(first_row);
                }
                wrap_line(&mut self.rows, text, style, line, self.width);
            }
        }
    }
}

fn remap_prefix(
    text: &mut String,
    style: &mut LineStyle,
    start: usize,
    previous: &str,
    next: &str,
    role: Role,
) {
    let end = start.saturating_add(previous.len());
    if text.get(start..end) != Some(previous)
        || !style
            .marks
            .iter()
            .any(|(range, marked_role)| *marked_role == role && range.contains(&start))
    {
        return;
    }
    text.replace_range(start..end, next);
    let difference = next.len() as isize - previous.len() as isize;
    if difference == 0 {
        return;
    }
    let marked_ranges = style.marks.iter_mut().map(|(range, _)| range);
    let linked_ranges = style.links.iter_mut().map(|(range, _)| range);
    for range in marked_ranges.chain(linked_ranges) {
        if range.end <= start {
            continue;
        }
        if range.start >= end {
            range.start = range.start.saturating_add_signed(difference);
        }
        range.end = range.end.saturating_add_signed(difference);
    }
}

/// Lays the block out for `width` and returns how many of its `existing_rows`
/// still match the layout. `row_counts` is truncated to the matching lines.
fn layout_markdown(block: &mut MarkdownBlock, width: usize, existing_rows: usize) -> usize {
    let content_width = width - block.hang(width);
    let unchanged = block
        .layout
        .update(&block.source, content_width, AmbiguousWidth::Narrow);
    let recorded_rows: usize = block.row_counts.iter().sum();
    if recorded_rows != existing_rows || unchanged > block.row_counts.len() {
        block.row_counts.clear();
        return 0;
    }
    block.row_counts.truncate(unchanged);
    block.row_counts.iter().sum()
}

fn push_markdown_rows(rows: &mut Vec<Row>, block: &mut MarkdownBlock, line: usize, width: usize) {
    let hang = block.hang(width);
    let content_width = width - hang;
    let first_new_line = block.row_counts.len();
    for (index, markdown_line) in block.layout.lines().iter().enumerate().skip(first_new_line) {
        let lead = if index == 0 {
            Lead::glyph(&block.lead)
        } else if markdown_line.spans.is_empty() {
            Lead::indent(0)
        } else {
            Lead::indent(hang)
        };
        let before = rows.len();
        for (row_index, content) in split_overflow(markdown_line, content_width)
            .into_iter()
            .enumerate()
        {
            let lead = if row_index == 0 {
                lead.clone()
            } else {
                Lead::indent(hang)
            };
            let mut styled = RoleLine::default();
            styled.push(lead.text, lead.role);
            for (text, role) in content {
                styled.push(text, role);
            }
            let byte = markdown_line.source_offset;
            let tab_cell = rows
                .last()
                .filter(|row| row.start.line == line && row.start.byte == byte)
                .map_or(0, |row| row.start.tab_cell + 1);
            rows.push(Row {
                start: Position {
                    line,
                    byte,
                    tab_cell,
                },
                text: styled.text(),
                styled,
            });
        }
        block.row_counts.push(rows.len() - before);
    }
}

#[derive(Clone)]
struct Lead {
    text: String,
    role: Option<Role>,
}

impl Lead {
    fn glyph(text: &str) -> Self {
        Self {
            text: text.to_owned(),
            role: Some(Role::Text),
        }
    }

    fn indent(width: usize) -> Self {
        Self {
            text: " ".repeat(width),
            role: None,
        }
    }
}

/// The layout already fits its width; only a key-value table cell with a
/// minimum value width can overflow, and it continues on further rows.
fn split_overflow(line: &MarkdownLine, width: usize) -> Vec<Vec<(String, Option<Role>)>> {
    let mut rows = vec![Vec::new()];
    let mut columns = 0;
    for span in &line.spans {
        let role = markdown_role(span.marks);
        for character in span.text.chars() {
            let character_width = character.width().unwrap_or(0);
            if columns > 0 && columns + character_width > width {
                rows.push(Vec::new());
                columns = 0;
            }
            let row: &mut Vec<(String, Option<Role>)> = rows.last_mut().unwrap();
            match row.last_mut() {
                Some((text, last_role)) if *last_role == role => text.push(character),
                _ => row.push((character.to_string(), role)),
            }
            columns += character_width;
        }
    }
    rows
}

/// Maps Markdown semantics onto roles; `docs/design/agent-ui.md` records the
/// precedence when a run carries several marks.
fn markdown_role(marks: Marks) -> Option<Role> {
    if marks.quote_bar {
        Some(Role::QuoteBar)
    } else if marks.inline_code || marks.link {
        Some(Role::Permission)
    } else if marks.heading == Some(1) {
        Some(Role::Heading)
    } else if marks.heading.is_some() || marks.strong {
        Some(if marks.emphasis {
            Role::StrongEmphasis
        } else {
            Role::Strong
        })
    } else if marks.emphasis || marks.quote {
        Some(Role::Emphasis)
    } else {
        None
    }
}

fn wrap_line(rows: &mut Vec<Row>, text: &str, style: &LineStyle, line: usize, width: usize) {
    let hang = if style.hang < width { style.hang } else { 0 };
    let mut start = Position {
        line,
        byte: 0,
        tab_cell: 0,
    };
    let mut output = String::new();
    let mut styled = RoleLine {
        fill: style.fill,
        ..Default::default()
    };
    let mut columns = 0;
    let mut logical_column = 0;
    let mut marks = style.marks.iter().peekable();
    let mut links = style.links.iter().peekable();
    for (byte, character) in text.char_indices() {
        while marks.peek().is_some_and(|(range, _)| range.end <= byte) {
            marks.next();
        }
        while links.peek().is_some_and(|(range, _)| range.end <= byte) {
            links.next();
        }
        let role = marks
            .peek()
            .filter(|(range, _)| range.contains(&byte))
            .map(|(_, role)| *role);
        let link = links
            .peek()
            .filter(|(range, _)| range.contains(&byte))
            .map(|(_, link)| link);
        let cells = if character == '\t' {
            TAB_COLUMNS - logical_column % TAB_COLUMNS
        } else {
            1
        };
        for tab_cell in 0..cells {
            let displayed = if character == '\t' { ' ' } else { character };
            let displayed = if displayed.width().unwrap_or(0) > width {
                '�'
            } else {
                displayed
            };
            let character_width = displayed.width().unwrap_or(0);
            if columns > 0 && columns + character_width > width {
                rows.push(Row {
                    start,
                    text: std::mem::take(&mut output),
                    styled: std::mem::take(&mut styled),
                });
                styled.fill = style.fill;
                start = Position {
                    line,
                    byte,
                    tab_cell,
                };
                columns = 0;
                if hang > 0 && character_width + hang <= width {
                    let indent = " ".repeat(hang);
                    output.push_str(&indent);
                    styled.push(indent, None);
                    columns = hang;
                }
            }
            output.push(displayed);
            styled.push_linked(displayed.to_string(), role, link.cloned());
            columns += character_width;
            logical_column += if character == '\t' {
                1
            } else {
                character.width().unwrap_or(0)
            };
        }
    }
    rows.push(Row {
        start,
        text: output,
        styled,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roles_of(line: &RoleLine) -> Vec<(String, Option<Role>)> {
        line.spans
            .iter()
            .map(|span| (span.text.clone(), span.role))
            .collect()
    }

    #[test]
    fn appended_roles_follow_their_text_onto_rows() {
        let mut scrollback = Scrollback::new();
        scrollback.append("plain ");
        scrollback.append_as("tool", Some(Role::ToolName), None);
        scrollback.append("(x)\nnext");
        let frame = scrollback.frame(40, 5);
        assert_eq!(frame.lines, ["plain tool(x)", "next"]);
        assert_eq!(
            roles_of(&frame.styled[0]),
            [
                ("plain ".to_owned(), None),
                ("tool".to_owned(), Some(Role::ToolName)),
                ("(x)".to_owned(), None),
            ]
        );
    }

    #[test]
    fn display_switch_remaps_only_semantic_glyph_runs() {
        let mut scrollback = Scrollback::new();
        scrollback.append("literal ↑↓ and ⏺ stay text\n");
        scrollback.append_as("⏺ assistant\n", Some(Role::Text), None);
        scrollback.append_as("⏺ tool\n", Some(Role::MutedText), None);
        scrollback.append_as("  ⎿ result\n", Some(Role::MutedText), None);
        scrollback.append_as("✓ Done", Some(Role::Success), None);
        let unicode = DisplayProfile::default();
        let nerd_font = DisplayProfile::new(tickloop_agent::settings::DisplayPreferences {
            glyph_set: tickloop_agent::settings::GlyphSet::NerdFont,
            prefers_reduced_motion: false,
        });

        scrollback.remap_display(unicode, nerd_font);

        assert_eq!(
            scrollback.frame(80, 8).lines,
            [
                "literal ↑↓ and ⏺ stay text",
                "\u{f06a9} assistant",
                "\u{f1064} tool",
                "  \u{f060c} result",
                "\u{f05e0} Done",
            ]
        );
        let before_reduced_motion = scrollback.frame(80, 8).lines;
        let reduced_motion = DisplayProfile::new(tickloop_agent::settings::DisplayPreferences {
            glyph_set: tickloop_agent::settings::GlyphSet::NerdFont,
            prefers_reduced_motion: true,
        });
        scrollback.remap_display(nerd_font, reduced_motion);
        assert_eq!(scrollback.frame(80, 8).lines, before_reduced_motion);
    }

    #[test]
    fn a_link_follows_its_text_across_wrapped_rows_and_glyph_remaps() {
        let link = Hyperlink::file(std::path::Path::new("/cache/1.png")).unwrap();
        let mut scrollback = Scrollback::new();
        scrollback.append_as("❯ ", Some(Role::Subtle), None);
        scrollback.append("see ");
        scrollback.append_span("[Image #1]", None, None, Some(&link));
        scrollback.append(" now");
        let linked_text = |scrollback: &mut Scrollback, width| {
            scrollback
                .frame(width, 5)
                .styled
                .iter()
                .flat_map(|row| row.spans.clone())
                .filter(|span| span.link.as_ref() == Some(&link))
                .map(|span| span.text)
                .collect::<Vec<_>>()
        };

        assert_eq!(scrollback.frame(40, 5).lines, ["❯ see [Image #1] now"]);
        assert_eq!(linked_text(&mut scrollback, 40), ["[Image #1]"]);
        assert_eq!(linked_text(&mut scrollback, 12), ["[Image", " #1]"]);

        let nerd_font = DisplayProfile::new(tickloop_agent::settings::DisplayPreferences {
            glyph_set: tickloop_agent::settings::GlyphSet::NerdFont,
            prefers_reduced_motion: false,
        });
        scrollback.remap_display(DisplayProfile::default(), nerd_font);
        assert_eq!(linked_text(&mut scrollback, 40), ["[Image #1]"]);
    }

    #[test]
    fn a_fill_marks_only_the_lines_that_received_text() {
        let mut scrollback = Scrollback::new();
        scrollback.append("before\n");
        scrollback.append_as("\n❯ hi\n", None, Some(Role::UserMessageBg));
        scrollback.append("after");
        let frame = scrollback.frame(40, 6);
        let fills: Vec<_> = frame.styled.iter().map(|line| line.fill).collect();
        assert_eq!(frame.lines, ["before", "", "❯ hi", "after"]);
        assert_eq!(fills, [None, None, Some(Role::UserMessageBg), None]);
    }

    #[test]
    fn a_block_hangs_wrapped_rows_and_leads_later_lines() {
        let mut scrollback = Scrollback::new();
        scrollback.begin_block("  ", 2);
        scrollback.append_as("⏺ ", Some(Role::Text), None);
        scrollback.append("abcdefgh\nij");
        scrollback.end_block();
        scrollback.append("\nout");
        let frame = scrollback.frame(6, 6);
        assert_eq!(frame.lines, ["⏺ abcd", "  efgh", "  ij", "out"]);
    }

    #[test]
    fn streamed_markdown_hangs_under_its_lead_and_drops_markup() {
        let mut scrollback = Scrollback::new();
        scrollback.append("\n");
        for chunk in ["## 标", "题\n\n**粗体** 与 `code` ", "wraps onto more rows"] {
            scrollback.append_markdown(chunk, "⏺ ", false);
            scrollback.frame(16, 10);
        }
        scrollback.append("\n✓ Done");
        let frame = scrollback.frame(16, 10);
        assert_eq!(
            frame.lines,
            [
                "",
                "⏺ 标题",
                "",
                "  粗体 与 code",
                "  wraps onto",
                "  more rows",
                "✓ Done"
            ]
        );
        assert_eq!(
            roles_of(&frame.styled[1]),
            [
                ("⏺ ".to_owned(), Some(Role::Text)),
                ("标题".to_owned(), Some(Role::Strong))
            ]
        );
        assert!(roles_of(&frame.styled[3]).contains(&("code".to_owned(), Some(Role::Permission))));
    }

    #[test]
    fn markdown_reflows_on_resize_and_a_new_message_starts_a_new_lead() {
        let mut scrollback = Scrollback::new();
        scrollback.append_markdown("| a | b |\n|---|---|\n| 1 | 2 |", "⏺ ", true);
        assert_eq!(
            scrollback.frame(40, 10).lines,
            [
                "⏺ ┌─────┬─────┐",
                "  │  a  │  b  │",
                "  ├─────┼─────┤",
                "  │ 1   │ 2   │",
                "  └─────┴─────┘",
            ]
        );
        assert_eq!(scrollback.frame(12, 10).lines, ["⏺ a: 1", "  b: 2"]);
        scrollback.append_markdown("second", "⏺ ", true);
        scrollback.append_markdown(" reply", "⏺ ", false);
        assert_eq!(scrollback.frame(40, 10).lines[5..], ["⏺ second reply"]);
    }

    #[test]
    fn streaming_markdown_matches_a_fresh_layout_after_every_chunk() {
        let source =
            "Intro paragraph.\n\n- one\n- two\n\n```\ncode line\n```\n\nTail text that wraps.";
        let mut streamed = Scrollback::new();
        for (index, character) in source.char_indices() {
            streamed.append_markdown(&source[index..index + character.len_utf8()], "⏺ ", false);
            let mut fresh = Scrollback::new();
            fresh.append_markdown(&source[..index + character.len_utf8()], "⏺ ", false);
            assert_eq!(streamed.frame(12, 40).lines, fresh.frame(12, 40).lines);
        }
    }

    #[test]
    fn a_glyph_switch_remaps_markdown_leads() {
        let mut scrollback = Scrollback::new();
        scrollback.append_markdown("reply", "⏺ ", true);
        scrollback.frame(20, 4);
        let nerd_font = DisplayProfile::new(tickloop_agent::settings::DisplayPreferences {
            glyph_set: tickloop_agent::settings::GlyphSet::NerdFont,
            prefers_reduced_motion: false,
        });
        scrollback.remap_display(DisplayProfile::default(), nerd_font);
        assert_eq!(scrollback.frame(20, 4).lines, ["\u{f06a9} reply"]);
    }

    fn done_notice(usage: &str) -> Vec<(String, Option<Role>)> {
        vec![
            ("\n".into(), None),
            ("✓ Done".into(), Some(Role::Success)),
            (format!(" · {usage}"), Some(Role::MutedText)),
            ("\n".into(), None),
        ]
    }

    #[test]
    fn only_the_latest_completion_notice_stays_visible() {
        let mut scrollback = Scrollback::new();
        scrollback.append("\n❯ first\n\n");
        scrollback.append_markdown("reply", "⏺ ", true);
        scrollback.append_completion_notice(&done_notice("1"));
        scrollback.append("\n❯ second\n");
        scrollback.append_as("  ⎿ result\n", Some(Role::MutedText), None);
        scrollback.append_completion_notice(&done_notice("2"));
        assert_eq!(
            scrollback.frame(40, 20).lines,
            [
                "",
                "❯ first",
                "",
                "⏺ reply",
                "",
                "❯ second",
                "  ⎿ result",
                "",
                "✓ Done · 2",
                ""
            ]
        );

        scrollback.append("\n❯ third\n");
        scrollback.hide_completion_notice();
        assert_eq!(
            scrollback.frame(40, 20).lines[5..],
            ["❯ second", "  ⎿ result", "", "❯ third", ""]
        );
        scrollback.append_completion_notice(&done_notice("3"));
        assert_eq!(
            scrollback.frame(40, 20).lines[5..],
            [
                "❯ second",
                "  ⎿ result",
                "",
                "❯ third",
                "",
                "✓ Done · 3",
                ""
            ]
        );
    }

    #[test]
    fn completion_notice_is_separated_from_a_markdown_list() {
        let mut scrollback = Scrollback::new();
        scrollback.append_markdown("1. first\n2. second\n", "⏺ ", true);
        scrollback.append_completion_notice(&[
            ("\n\n".into(), None),
            ("✓ Done".into(), Some(Role::Success)),
            ("\n".into(), None),
        ]);
        let lines = scrollback.frame(40, 20).lines;
        let done = lines.iter().position(|line| line == "✓ Done").unwrap();
        assert_eq!(lines[done - 1], "");
        assert_eq!(lines[done - 2], "  2. second");
    }

    #[test]
    fn the_activity_row_follows_the_conversation_until_cleared() {
        let mut scrollback = Scrollback::new();
        scrollback.append_markdown("reply", "⏺ ", true);
        assert!(scrollback.set_activity(Some(RoleLine::role("✢ Working", Role::Accent))));
        assert!(!scrollback.set_activity(Some(RoleLine::role("✢ Working", Role::Accent))));
        let working = scrollback.frame(20, 5);
        assert_eq!(working.lines, ["⏺ reply", "", "✢ Working"]);
        assert_eq!(working.styled[0].spans[0].role, Some(Role::Text));

        scrollback.set_activity(None);
        let finished = scrollback.frame(20, 5);
        assert_eq!(finished.lines, ["⏺ reply"]);
        assert_eq!(finished.styled[0].spans[0].role, Some(Role::Text));
    }

    #[test]
    fn a_running_tool_name_shimmers_until_finished() {
        let mut scrollback = Scrollback::new();
        scrollback.append("before");
        scrollback.append_tool_call(
            "call",
            &[
                ("\n".into(), None),
                ("⏺ ".into(), Some(Role::MutedText)),
                ("Shell".into(), Some(Role::ToolName)),
                ("\n".into(), None),
            ],
        );
        scrollback.append_as("  ⎿ output\n", Some(Role::MutedText), None);
        let running = scrollback.frame_with_motion(20, 5, true);
        assert_eq!(running.lines, ["before", "⏺ Shell", "  ⎿ output", ""]);
        assert!(
            running.styled[1]
                .spans
                .iter()
                .any(|span| span.role == Some(Role::RunningShade0))
        );
        assert_eq!(running.styled[2].spans[0].role, Some(Role::MutedText));

        scrollback.finish_tool_call("call");
        assert_eq!(
            scrollback.frame(20, 5).styled[1]
                .spans
                .iter()
                .find(|span| span.text == "Shell")
                .map(|span| span.role),
            Some(Some(Role::ToolName))
        );
    }

    fn numbered_lines(count: usize) -> String {
        (0..count)
            .map(|index| format!("row {index}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn all_logical_lines_remain_available_beyond_one_thousand_rows() {
        let mut scrollback = Scrollback::new();
        scrollback.append(&numbered_lines(1501));
        assert_eq!(scrollback.frame(80, 4).lines.last().unwrap(), "row 1500");
        scrollback.scroll(ScrollAction::First);
        assert_eq!(
            scrollback.frame(80, 4).lines,
            ["row 0", "row 1", "row 2", "row 3"]
        );
        scrollback.scroll(ScrollAction::Last);
        assert!(scrollback.frame(80, 4).following);
    }

    #[test]
    fn streamed_fragments_keep_newlines_and_reflow_the_unfinished_line() {
        let mut scrollback = Scrollback::new();
        for text in ["ab", "cdef", "ghi", "j\n", "中", "文", "\n", "tail"] {
            scrollback.append(text);
            scrollback.frame(5, 2);
        }
        assert_eq!(scrollback.lines, ["abcdefghij", "中文", "tail"]);
        scrollback.scroll(ScrollAction::First);
        assert_eq!(
            scrollback.frame(5, 10).lines,
            ["abcde", "fghij", "中文", "tail"]
        );
        assert_eq!(
            scrollback.frame(12, 10).lines,
            ["abcdefghij", "中文", "tail"]
        );
    }

    #[test]
    fn paused_streaming_and_resize_preserve_the_logical_byte_anchor() {
        let mut scrollback = Scrollback::new();
        scrollback.append("abcdefghijklmnopqrstuvwx\n尾部");
        scrollback.frame(6, 2);
        scrollback.scroll(ScrollAction::First);
        scrollback.scroll(ScrollAction::HalfPageDown);
        let before = scrollback.frame(6, 2);
        assert_eq!(before.lines[0], "ghijkl");
        assert!(!before.following);
        scrollback.append("继续输出\nnew output");
        assert_eq!(scrollback.frame(4, 2).lines[0], "efgh");
        assert_eq!(scrollback.frame(6, 2).lines, before.lines);
        assert!(!scrollback.frame(6, 2).following);
    }

    #[test]
    fn chinese_and_tab_anchors_survive_a_round_trip_resize() {
        let mut scrollback = Scrollback::new();
        scrollback.append("甲乙丙丁戊己庚辛壬癸\n\t尾部");
        scrollback.frame(6, 2);
        scrollback.scroll(ScrollAction::First);
        scrollback.scroll(ScrollAction::HalfPageDown);
        let before = scrollback.frame(6, 2);
        assert_eq!(before.lines[0], "丁戊己");
        assert_eq!(scrollback.frame(4, 2).lines[0], "丙丁");
        assert_eq!(scrollback.frame(6, 2).lines, before.lines);

        let mut tabs = Scrollback::new();
        tabs.append("\t中文\n后续内容");
        tabs.frame(1, 2);
        tabs.scroll(ScrollAction::First);
        tabs.scroll(ScrollAction::HalfPageDown);
        let tab_anchor = tabs.anchor;
        assert_eq!(tab_anchor.tab_cell, 1);
        tabs.frame(3, 2);
        tabs.frame(1, 2);
        assert_eq!(tabs.anchor, tab_anchor);
        assert_eq!(tabs.rows[tabs.top_row()].start, tab_anchor);
    }

    #[test]
    fn chinese_tabs_and_control_characters_respect_terminal_columns() {
        let mut scrollback = Scrollback::new();
        scrollback.append("中\t文\u{1b}[31m\n\t尾");
        for width in [1, 2, 3, 6, 20] {
            let frame = scrollback.frame(width, 100);
            for line in frame.lines {
                assert!(!line.chars().any(char::is_control));
                assert!(
                    line.chars()
                        .map(|character| character.width().unwrap_or(0))
                        .sum::<usize>()
                        <= width
                );
            }
        }
        assert_eq!(scrollback.frame(20, 100).lines, ["中  文�[31m", "    尾"]);
        assert_eq!(scrollback.lines[0], "中\t文�[31m");
    }

    #[test]
    fn half_page_scroll_pauses_and_reaching_the_bottom_resumes() {
        let mut scrollback = Scrollback::new();
        scrollback.append(&numbered_lines(20));
        assert_eq!(scrollback.frame(40, 6).lines[0], "row 14");
        scrollback.scroll(ScrollAction::HalfPageUp);
        let paused = scrollback.frame(40, 6);
        assert_eq!(paused.lines[0], "row 11");
        assert!(!paused.following);
        scrollback.append("\nrow 20");
        assert_eq!(scrollback.frame(40, 6).lines[0], "row 11");
        scrollback.scroll(ScrollAction::HalfPageDown);
        assert!(!scrollback.frame(40, 6).following);
        scrollback.scroll(ScrollAction::HalfPageDown);
        assert!(scrollback.frame(40, 6).following);
        scrollback.append("\nrow 21");
        assert_eq!(scrollback.frame(40, 6).lines.last().unwrap(), "row 21");
    }

    #[test]
    fn message_ids_count_once_while_paused_and_clear_at_the_bottom() {
        let mut scrollback = Scrollback::new();
        scrollback.append(&numbered_lines(30));
        scrollback.note_message("existing".into());
        scrollback.frame(40, 4);
        scrollback.scroll(ScrollAction::First);
        scrollback.append("\nfirst fragment");
        scrollback.note_message("reply-1".into());
        scrollback.append(" continued fragment");
        scrollback.note_message("reply-1".into());
        scrollback.note_message("existing".into());
        assert_eq!(scrollback.frame(40, 4).unread, 1);
        scrollback.append("\nTool result");
        scrollback.note_message("tool-1".into());
        assert_eq!(scrollback.frame(40, 4).unread, 2);
        scrollback.scroll(ScrollAction::Last);
        assert_eq!(scrollback.frame(40, 4).unread, 0);
        scrollback.scroll(ScrollAction::First);
        scrollback.note_message("tool-1".into());
        assert_eq!(scrollback.frame(40, 4).unread, 0);
    }

    #[test]
    fn disabled_auto_scroll_does_not_follow_after_a_manual_jump() {
        let mut scrollback = Scrollback::new();
        scrollback.append(&numbered_lines(20));
        scrollback.frame(40, 4);
        scrollback.set_auto_scroll(false);
        assert!(!scrollback.auto_scroll());
        scrollback.append("\nrow 20");
        assert_eq!(scrollback.frame(40, 4).lines[0], "row 16");
        scrollback.scroll(ScrollAction::Last);
        let manual_bottom = scrollback.frame(40, 4);
        assert_eq!(manual_bottom.lines.last().unwrap(), "row 20");
        assert!(!manual_bottom.following);
        scrollback.append("\nrow 21");
        scrollback.note_message("new reply".into());
        let paused = scrollback.frame(40, 4);
        assert_eq!(paused.lines, manual_bottom.lines);
        assert_eq!(paused.unread, 1);
        scrollback.set_auto_scroll(true);
        assert!(!scrollback.frame(40, 4).following);
        scrollback.scroll(ScrollAction::Last);
        assert!(scrollback.frame(40, 4).following);
    }

    #[test]
    fn visible_tail_clears_unread_without_enabling_auto_scroll() {
        let mut scrollback = Scrollback::new();
        scrollback.append("first");
        scrollback.frame(40, 6);
        scrollback.set_auto_scroll(false);
        scrollback.append("\nsecond");
        scrollback.note_message("second".into());
        let visible = scrollback.frame(40, 6);
        assert_eq!(visible.unread, 0);
        assert_eq!(visible.lines, ["first", "second"]);
        assert!(!visible.following);
        assert!(!scrollback.auto_scroll());
        scrollback.set_auto_scroll(true);
        assert!(scrollback.frame(40, 6).following);
    }

    #[test]
    fn resizing_a_visible_tail_does_not_resolve_a_manual_pause() {
        let mut scrollback = Scrollback::new();
        scrollback.append(&numbered_lines(10));
        scrollback.frame(20, 3);
        scrollback.scroll(ScrollAction::First);
        assert!(!scrollback.frame(40, 20).following);
        scrollback.append("\nrow 10");
        assert!(!scrollback.frame(20, 3).following);
        assert_eq!(scrollback.frame(20, 3).lines[0], "row 0");
    }
}
