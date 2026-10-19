use std::{
    collections::{BTreeMap, HashMap, HashSet},
    ops::Range,
    sync::atomic::{AtomicU64, Ordering},
};

use tickloop_agent_protocol::ModelRequestKind;
use tickloop_ui::{
    markdown::{MarkdownLine, Marks, StreamingLayout},
    settings::AmbiguousWidth,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use uuid::Uuid;

use super::{
    hyperlink::Hyperlink,
    running_shimmer::RunningShimmer,
    style::{Role, RoleLine},
};

const TAB_COLUMNS: usize = 4;
const DEFAULT_WIDTH: usize = 80;
const DEFAULT_HEIGHT: usize = 22;
/// Rows of a `⎿` response wrap under its content: two spaces, `⎿` and two spaces.
const RESPONSE_HANG: usize = 5;
/// Rows of a block's body wrap under its two-space indent.
const BODY_HANG: usize = 2;
const EXPAND_HINT: &str = " (ctrl+o to expand)";

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
    /// The row of the conversation and its tail drawn first.
    pub(super) top: usize,
    pub(super) lines: Vec<String>,
    /// The same rows as `lines`, described by role.
    pub(super) styled: Vec<RoleLine>,
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) following: bool,
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) unread: usize,
}

#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    Eq,
    Ord,
    PartialEq,
    PartialOrd,
    serde::Serialize,
    serde::Deserialize,
)]
struct Position {
    line: usize,
    byte: usize,
    tab_cell: usize,
}

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub(super) struct ScrollCheckpoint {
    anchor: Position,
    following: bool,
    auto_scroll: bool,
    unread: usize,
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
    flash: Option<(super::list_row::RowFlash, u64)>,
    pending_take_in: Option<tick_protocol::InstanceRef>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ToolCallState {
    Queued,
    Running,
    Finished,
    /// Finished with an error result, including a call the user declined.
    Failed,
    Stopped,
}

impl ToolCallState {
    pub(super) fn after_run(self) -> Self {
        match self {
            Self::Queued => Self::Stopped,
            Self::Running => Self::Finished,
            Self::Finished | Self::Failed | Self::Stopped => self,
        }
    }
    pub(super) fn suffix(self) -> &'static str {
        match self {
            Self::Queued => " · Queued",
            Self::Stopped => " · Stopped",
            Self::Running | Self::Finished | Self::Failed => "",
        }
    }
    /// The colour of the call's `⏺`, as Claude Code's `ToolUseLoader` paints it: muted until a
    /// result arrives, then the success or the error colour.
    fn dot_role(self) -> Role {
        match self {
            Self::Queued | Self::Running | Self::Stopped => Role::MutedText,
            Self::Finished => Role::Success,
            Self::Failed => Role::Error,
        }
    }
}

struct ToolCallHeader {
    run_id: Uuid,
    line: usize,
    /// Where the call's next result row goes: under its header and the result rows it has.
    results_end: usize,
    spans: Vec<(String, Option<Role>)>,
    state: ToolCallState,
    /// What a read or search call adds to the collapsed group it joined.
    collapse: Option<(CollapsedCall, usize)>,
}

/// A call that reads or searches. Consecutive ones share one collapsed row, as Claude Code's
/// `CollapsedReadSearchContent` sums them up; Ctrl+O shows each of them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum CollapsedCall {
    Search,
    Read { path: String },
}

/// One collapsed row standing for consecutive read and search calls.
struct CollapsedGroup {
    line: usize,
    /// The `⏺ ` shown while one of its calls still works.
    lead: String,
}

struct ModelRetryNotice {
    line: usize,
    attempt: usize,
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
/// What moves the model's running Bash calls to the background, and the calls it sits under; a
/// command whose call is unknown has `None`.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct ForegroundCommandHint {
    pub(super) line: RoleLine,
    pub(super) calls: Vec<Option<String>>,
}

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
    pending_take_in_lines: Vec<usize>,
    block: Option<Block>,
    rows: Vec<Row>,
    /// Rows of the conversation itself, without the blank rows that end it; kept with `rows`.
    conversation_rows: usize,
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
    activity_details: Vec<RoleLine>,
    /// The latest output of a running `!` command, drawn under its echo until it ends.
    shell_preview: Vec<RoleLine>,
    /// What moves the model's running Bash calls to the background.
    foreground_command_hint: Option<ForegroundCommandHint>,
    /// The hint's logical lines under the calls' rows; they are hidden once it goes.
    foreground_command_hint_lines: Vec<usize>,
    /// Header lines of tool calls still executing, by call id.
    running_tool_calls: HashMap<String, usize>,
    tool_call_headers: HashMap<String, ToolCallHeader>,
    running_shimmer: RunningShimmer,
    completion_notice: Option<Range<usize>>,
    model_retries: BTreeMap<(Uuid, ModelRequestKind), ModelRetryNotice>,
    /// Logical lines of each input echo, oldest first.
    inputs: Vec<(u64, Range<usize>)>,
    next_input: u64,
    flashes_since_ms: u64,
    groups: Vec<CollapsedGroup>,
    /// The collapsed group that ends the conversation, which the next read or search joins.
    open_group: Option<usize>,
}

impl Default for Scrollback {
    fn default() -> Self {
        Self {
            id: NEXT_SCROLLBACK.fetch_add(1, Ordering::Relaxed),
            lines: vec![String::new()],
            line_styles: vec![LineStyle::default()],
            pending_take_in_lines: Vec::new(),
            block: None,
            rows: Vec::new(),
            conversation_rows: 0,
            dirty_from: Some(0),
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
            anchor: Position::default(),
            following: true,
            auto_scroll: true,
            seen_messages: HashSet::new(),
            unread: 0,
            activity: None,
            activity_details: Vec::new(),
            shell_preview: Vec::new(),
            foreground_command_hint: None,
            foreground_command_hint_lines: Vec::new(),
            running_tool_calls: HashMap::new(),
            tool_call_headers: HashMap::new(),
            running_shimmer: RunningShimmer::default(),
            completion_notice: None,
            model_retries: BTreeMap::new(),
            inputs: Vec::new(),
            next_input: 0,
            flashes_since_ms: 0,
            groups: Vec::new(),
            open_group: None,
        }
    }
}

impl Scrollback {
    pub(super) fn upgrade_checkpoint(&self) -> ScrollCheckpoint {
        ScrollCheckpoint {
            anchor: self.anchor,
            following: self.following,
            auto_scroll: self.auto_scroll,
            unread: self.unread,
        }
    }

    pub(super) fn restore_upgrade_checkpoint(&mut self, checkpoint: ScrollCheckpoint) {
        self.anchor = checkpoint.anchor;
        self.following = checkpoint.following;
        self.auto_scroll = checkpoint.auto_scroll;
        self.unread = checkpoint.unread;
    }

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

    pub(super) fn sticky_input_text(
        &mut self,
        prompt: &str,
        width: usize,
        height: usize,
    ) -> Option<String> {
        self.prepare_frame(width, height);
        let (range, text) = self.inputs.iter().rev().find_map(|(_, range)| {
            let text = self
                .lines
                .get(range.clone())?
                .iter()
                .zip(&self.line_styles[range.clone()])
                .filter(|(_, style)| !style.hidden)
                .map(|(line, _)| line.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            let text = text.strip_prefix(prompt).unwrap_or(&text).trim();
            (!text.is_empty() && !text.starts_with('/')).then(|| (range, text.to_owned()))
        })?;
        let first_row = self
            .rows
            .partition_point(|row| row.start.line < range.start);
        self.rows
            .get(first_row)
            .filter(|row| range.contains(&row.start.line))?;
        (first_row < self.top_row()).then_some(text)
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
        self.open_group = None;
        if self.last_line_is_markdown() && !text.starts_with('\n') {
            self.start_line();
        }
        self.mark_last_line_dirty();
        let mut segment_start = self.lines.last().unwrap().len();
        let mut line_started_here = false;
        for character in text.chars() {
            if character == '\n' {
                // A blank line inside filled text keeps the fill, as Claude Code's message box
                // paints its background behind every line it holds; a line this text did not
                // start keeps its own.
                if fill.is_some() && line_started_here {
                    self.line_styles.last_mut().unwrap().fill = fill;
                }
                self.close_segment(segment_start, role, link);
                self.start_line();
                line_started_here = true;
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

    /// [`Self::append_span`] with every line that receives text wrapping its continuation rows
    /// under column `hang`, as Claude Code's message boxes indent wrapped content.
    pub(super) fn append_hanging(
        &mut self,
        text: &str,
        role: Option<Role>,
        fill: Option<Role>,
        link: Option<&Hyperlink>,
        hang: usize,
    ) {
        let starts_line = text.starts_with('\n') || self.last_line_is_markdown();
        let first_line = self.lines.len() - 1 + usize::from(starts_line);
        self.append_span(text, role, fill, link);
        if hang == 0 {
            return;
        }
        let last_line = self.lines.len() - usize::from(text.ends_with('\n'));
        for style in &mut self.line_styles[first_line.min(last_line)..last_line] {
            style.hang = hang;
        }
    }

    /// Appends Markdown reply text. The text extends the reply on the last
    /// line unless `starts_message` is set or something else was written since,
    /// in which case a new reply block starts with `lead` in front of it.
    pub(super) fn append_markdown(&mut self, text: &str, lead: &str, starts_message: bool) {
        if text.is_empty() {
            return;
        }
        if starts_message || !self.last_line_is_markdown() {
            self.start_block();
            self.begin_markdown(lead);
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

    /// Starts a reply laid out as Markdown behind `lead` on the line below, with no blank row
    /// above it; [`Self::append_markdown`] extends it.
    pub(super) fn begin_markdown(&mut self, lead: &str) {
        if !self.lines.last().unwrap().is_empty() || self.last_line_is_markdown() {
            self.start_line();
        }
        self.mark_last_line_dirty();
        self.line_styles.last_mut().unwrap().markdown = Some(Box::new(MarkdownBlock::new(lead)));
    }

    /// Appends a run's completion notice as a block of its own and hides the previous one, so
    /// only the latest completion stays visible.
    pub(super) fn append_completion_notice(&mut self, spans: &[(String, Option<Role>)]) {
        self.hide_completion_notice();
        self.start_block();
        let first_line = self.lines.len() - 1;
        for (text, role) in spans {
            self.append_as(text, *role, None);
        }
        let end = self.lines.len() - usize::from(self.lines.last().unwrap().is_empty());
        self.completion_notice = Some(first_line..end.max(first_line));
    }

    pub(super) fn set_model_retry(
        &mut self,
        run_id: Uuid,
        kind: ModelRequestKind,
        attempt: usize,
        spans: &[(String, Option<Role>)],
    ) {
        let key = (run_id, kind);
        let line = match self.model_retries.get(&key) {
            Some(previous) if attempt > previous.attempt => previous.line,
            _ => {
                self.open_group = None;
                if !self.lines.last().unwrap().is_empty() {
                    self.start_line();
                }
                let line = self.lines.len() - 1;
                self.start_line();
                line
            }
        };
        let spans = spans
            .iter()
            .map(|(text, role)| (text.as_str(), *role))
            .collect::<Vec<_>>();
        self.set_line(line, &spans);
        self.model_retries
            .insert(key, ModelRetryNotice { line, attempt });
    }

    /// Starts a block: later text begins on a line of its own with exactly one blank row above
    /// it, as Claude Code gives every message a top margin, whatever hidden lines lie between it
    /// and the visible content above.
    pub(super) fn start_block(&mut self) {
        self.open_group = None;
        self.separate_block();
    }

    /// Appends `text` as a block that leaves an open read or search group open, as Claude Code
    /// lets a thinking block pass a collapsed group without ending it.
    pub(super) fn append_aside(&mut self, text: &str, role: Option<Role>) {
        let open_group = self.open_group;
        self.separate_block();
        self.append_as(text, role, None);
        self.open_group = open_group;
    }

    fn separate_block(&mut self) {
        if !self.lines.last().unwrap().is_empty() || self.last_line_is_markdown() {
            self.start_line();
        }
        let open_line = self.lines.len() - 1;
        let follows_blank = self.lines[..open_line]
            .iter()
            .zip(&self.line_styles[..open_line])
            .rev()
            .find(|(_, style)| !style.hidden)
            .is_some_and(|(text, style)| is_blank_line(text, style));
        if !follows_blank {
            self.start_line();
        }
    }

    /// Appends `⎿` response rows at the end of the conversation, each wrapping under its
    /// content.
    pub(super) fn append_response(&mut self, runs: &[(String, Option<Role>)]) {
        self.append_rows(runs, RESPONSE_HANG);
    }

    /// Appends a block's body rows, indented under its heading, each wrapping under its indent.
    pub(super) fn append_body(&mut self, runs: &[(String, Option<Role>)]) {
        self.append_rows(runs, BODY_HANG);
    }

    fn append_rows(&mut self, runs: &[(String, Option<Role>)], hang: usize) {
        let first_line = self.lines.len() - 1 + usize::from(self.last_line_is_markdown());
        for (text, role) in runs {
            self.append_linked_runs(text, *role);
        }
        let last_line = self.lines.len() - usize::from(self.lines.last().unwrap().is_empty());
        for style in &mut self.line_styles[first_line.min(last_line)..last_line] {
            style.hang = hang;
        }
    }

    /// Text whose error runs open the web addresses they name.
    fn append_linked_runs(&mut self, text: &str, role: Option<Role>) {
        if role == Some(Role::Error) {
            let links = super::hyperlink::web_links(text);
            for (run, link) in super::hyperlink::runs(text, &links) {
                self.append_span(run, role, None, link);
            }
        } else {
            self.append_as(text, role, None);
        }
    }

    /// Draws a call's result under the call's own row and the result rows it already has, so
    /// parallel calls each keep their results. A result of a collapsed read or search shows only
    /// in the detailed transcript; one whose call is not shown goes to the end.
    pub(super) fn append_tool_result(&mut self, call_id: &str, runs: &[(String, Option<Role>)]) {
        let Some(header) = self.tool_call_headers.get(call_id) else {
            self.append_response(runs);
            return;
        };
        if header.collapse.is_some() {
            return;
        }
        let at = header.results_end;
        let mut result = Scrollback::new();
        result.append_response(runs);
        if result.lines.last().is_some_and(String::is_empty) && result.lines.len() > 1 {
            result.lines.pop();
            result.line_styles.pop();
        }
        self.insert_lines(at, result.lines, result.line_styles);
    }

    /// Inserts logical lines before line `at`, moving every recorded line position behind it.
    fn insert_lines(&mut self, at: usize, lines: Vec<String>, styles: Vec<LineStyle>) {
        let count = lines.len();
        if count == 0 {
            return;
        }
        let shift = |line: &mut usize| {
            if *line >= at {
                *line += count;
            }
        };
        for header in self.tool_call_headers.values_mut() {
            shift(&mut header.line);
            shift(&mut header.results_end);
        }
        for line in self.running_tool_calls.values_mut() {
            shift(line);
        }
        for group in &mut self.groups {
            shift(&mut group.line);
        }
        for line in &mut self.foreground_command_hint_lines {
            shift(line);
        }
        for notice in self.model_retries.values_mut() {
            shift(&mut notice.line);
        }
        let shift_range = |range: &mut Range<usize>| {
            if range.start >= at {
                range.start += count;
                range.end += count;
            }
        };
        if let Some(notice) = &mut self.completion_notice {
            shift_range(notice);
        }
        for (_, range) in &mut self.inputs {
            shift_range(range);
        }
        shift(&mut self.anchor.line);
        self.lines.splice(at..at, lines);
        self.line_styles.splice(at..at, styles);
        self.dirty_from = Some(self.dirty_from.map_or(at, |existing| existing.min(at)));
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

    pub(super) fn append_flashing_label(
        &mut self,
        runs: &[(String, Option<Role>)],
        flash: Option<(super::list_row::RowFlash, u64)>,
        pending_take_in: Option<tick_protocol::InstanceRef>,
    ) {
        self.start_block();
        let start = self.lines.len() - 1;
        for (text, role) in runs {
            self.append_as(text.trim_start_matches('\n'), *role, None);
        }
        for index in start..self.lines.len() {
            if !self.lines[index].is_empty() {
                self.line_styles[index].flash = flash;
                self.line_styles[index].pending_take_in = pending_take_in.clone();
                if pending_take_in.is_some() {
                    self.pending_take_in_lines.push(index);
                }
            }
        }
    }

    pub(super) fn sync_native_take_in(
        &mut self,
        membership: &tickloop_agent_protocol::MonitorMembership,
    ) {
        self.pending_take_in_lines.retain(|index| {
            let Some(style) = self.line_styles.get_mut(*index) else {
                return false;
            };
            let Some(instance) = &style.pending_take_in else {
                return false;
            };
            let Some(since) = membership.taken_in_at(instance) else {
                return true;
            };
            if since >= self.flashes_since_ms {
                style.flash = Some((super::list_row::RowFlash::TakeIn, since));
            }
            style.pending_take_in = None;
            false
        });
    }

    pub(super) fn flash_tool_call(&mut self, key: &str, since_ms: u64) {
        if let Some(header) = self.tool_call_headers.get(key) {
            self.line_styles[header.line].flash = Some((super::list_row::RowFlash::Use, since_ms));
        }
    }

    pub(super) fn settle_flashes(&mut self, now_ms: u64) {
        self.flashes_since_ms = now_ms;
    }

    pub(super) fn next_flash_frame_ms(
        &self,
        now_ms: u64,
        display: super::display::DisplayProfile,
    ) -> Option<u64> {
        let top = self.top_row();
        self.rows
            .iter()
            .skip(top)
            .take(self.height)
            .filter_map(|row| self.line_styles[row.start.line].flash)
            .filter(|(_, since)| *since >= self.flashes_since_ms)
            .filter_map(|(_, since)| display.next_flash_change_ms(since, now_ms))
            .min()
    }

    /// Shows a call's row in `state`, adding it as a block when the call is new. A read or
    /// search (`collapse`) joins the collapsed group that ends the conversation, or starts one.
    pub(super) fn set_tool_call(
        &mut self,
        run_id: Uuid,
        call_id: &str,
        spans: &[(String, Option<Role>)],
        state: ToolCallState,
        collapse: Option<CollapsedCall>,
    ) {
        let existing = self
            .tool_call_headers
            .get(call_id)
            .map(|header| (header.line, header.results_end, header.collapse.clone()));
        let (header_line, results_end, collapse) = match existing {
            Some(existing) => existing,
            None => match collapse {
                Some(call) => {
                    let group = self.open_group.unwrap_or_else(|| self.start_group(spans));
                    let line = self.groups[group].line;
                    (line, line + 1, Some((call, group)))
                }
                None => {
                    let line = self.start_block_line();
                    (line, line + 1, None)
                }
            },
        };
        self.tool_call_headers.insert(
            call_id.to_owned(),
            ToolCallHeader {
                run_id,
                line: header_line,
                results_end,
                spans: spans.to_vec(),
                state,
                collapse: collapse.clone(),
            },
        );
        if let Some((_, group)) = collapse {
            self.running_tool_calls.remove(call_id);
            self.draw_group(group);
            return;
        }
        // The first styled span is the call's `⏺`; its colour follows the state.
        let dot = spans.iter().position(|(_, role)| role.is_some());
        let header_spans: Vec<_> = spans
            .iter()
            .map(|(text, role)| (text.trim_matches('\n'), *role))
            .chain(std::iter::once((state.suffix(), Some(Role::MutedText))))
            .enumerate()
            .map(|(index, (text, role))| {
                let role = if Some(index) == dot {
                    Some(state.dot_role())
                } else {
                    role
                };
                (text, role)
            })
            .collect();
        self.set_line(header_line, &header_spans);
        if state == ToolCallState::Running {
            self.running_tool_calls
                .insert(call_id.to_owned(), header_line);
            self.running_shimmer.reset();
        } else {
            self.running_tool_calls.remove(call_id);
        }
    }

    /// Starts a block of one line, set later, and returns the line.
    fn start_block_line(&mut self) -> usize {
        self.start_block();
        let line = self.lines.len() - 1;
        self.start_line();
        line
    }

    /// Adds a collapsed group as a new block led by the call's `⏺`.
    fn start_group(&mut self, spans: &[(String, Option<Role>)]) -> usize {
        let line = self.start_block_line();
        let lead = spans
            .iter()
            .find(|(_, role)| role.is_some())
            .map_or_else(String::new, |(text, _)| text.trim_matches('\n').to_owned());
        self.groups.push(CollapsedGroup { line, lead });
        self.open_group = Some(self.groups.len() - 1);
        self.groups.len() - 1
    }

    /// Redraws a collapsed group's row from its calls: present tense behind a `⏺` while one of
    /// them works, past tense and muted once all are done.
    fn draw_group(&mut self, group: usize) {
        let members: Vec<_> = self
            .tool_call_headers
            .values()
            .filter_map(|header| {
                let (call, member_group) = header.collapse.as_ref()?;
                (*member_group == group).then_some((call, header.state))
            })
            .collect();
        let active = members
            .iter()
            .any(|(_, state)| matches!(state, ToolCallState::Queued | ToolCallState::Running));
        let failed = members
            .iter()
            .any(|(_, state)| *state == ToolCallState::Failed);
        let searches = members
            .iter()
            .filter(|(call, _)| **call == CollapsedCall::Search)
            .count();
        let reads = members
            .iter()
            .filter_map(|(call, _)| match call {
                CollapsedCall::Read { path } => Some(path.as_str()),
                CollapsedCall::Search => None,
            })
            .collect::<HashSet<_>>()
            .len();
        let (text_role, count_role) = if active {
            (None, Some(Role::Strong))
        } else {
            (Some(Role::MutedText), Some(Role::MutedStrong))
        };
        let mut spans: Vec<(String, Option<Role>)> = if active {
            let dot = if failed { Role::Error } else { Role::MutedText };
            vec![(self.groups[group].lead.clone(), Some(dot))]
        } else {
            vec![("  ".into(), None)]
        };
        for (count, verbs, noun) in [
            (searches, ["Searching for", "Searched for"], "pattern"),
            (reads, ["Reading", "Read"], "file"),
        ] {
            if count == 0 {
                continue;
            }
            let verb = verbs[usize::from(!active)];
            let verb = if spans.len() == 1 {
                format!("{verb} ")
            } else {
                format!(", {} ", verb.to_lowercase())
            };
            let plural = if count == 1 { "" } else { "s" };
            spans.push((verb, text_role));
            spans.push((count.to_string(), count_role));
            spans.push((format!(" {noun}{plural}"), text_role));
        }
        if active {
            spans.push(("…".into(), text_role));
        }
        spans.push((EXPAND_HINT.into(), Some(Role::MutedText)));
        let line = self.groups[group].line;
        let spans: Vec<_> = spans
            .iter()
            .map(|(text, role)| (text.as_str(), *role))
            .collect();
        self.set_line(line, &spans);
    }

    /// Replaces line `line` with `spans`.
    fn set_line(&mut self, line: usize, spans: &[(&str, Option<Role>)]) {
        let text = &mut self.lines[line];
        text.clear();
        let style = &mut self.line_styles[line];
        *style = LineStyle::default();
        for (span, role) in spans {
            let start = text.len();
            text.push_str(span);
            if let Some(role) = role {
                style.marks.push((start..text.len(), *role));
            }
        }
        self.dirty_from = Some(self.dirty_from.map_or(line, |existing| existing.min(line)));
    }

    pub(super) fn finish_tool_call(&mut self, run_id: Uuid, call_id: &str, failed: bool) {
        if let Some(header) = self
            .tool_call_headers
            .get(call_id)
            .filter(|header| header.run_id == run_id)
        {
            let spans = header.spans.clone();
            let state = if failed {
                ToolCallState::Failed
            } else {
                ToolCallState::Finished
            };
            self.set_tool_call(run_id, call_id, &spans, state, None);
        }
    }

    pub(super) fn finish_tool_calls(&mut self, run_id: Uuid) {
        let unfinished: Vec<_> = self
            .tool_call_headers
            .iter()
            .filter(|(_, header)| header.run_id == run_id)
            .filter_map(|(id, header)| {
                let state = header.state.after_run();
                (state != header.state).then(|| (id.clone(), header.spans.clone(), state))
            })
            .collect();
        for (id, spans, state) in unfinished {
            self.set_tool_call(run_id, &id, &spans, state, None);
        }
    }

    /// Returns whether the activity row changed.
    pub(super) fn set_activity(&mut self, activity: Option<RoleLine>) -> bool {
        if self.activity == activity {
            return false;
        }
        self.activity = activity;
        true
    }

    pub(super) fn set_activity_details(&mut self, details: Vec<RoleLine>) {
        self.activity_details = details;
    }

    /// Returns whether the running command's preview changed.
    pub(super) fn set_shell_preview(&mut self, rows: Vec<RoleLine>) -> bool {
        if self.shell_preview == rows {
            return false;
        }
        self.shell_preview = rows;
        true
    }

    /// Draws the hint under the row of each call it names, after the results the call already
    /// has, so later messages and calls stay below it; a call without a row here has it below the
    /// conversation. Returns whether the hint changed.
    pub(super) fn set_foreground_command_hint(
        &mut self,
        hint: Option<ForegroundCommandHint>,
    ) -> bool {
        if self.foreground_command_hint == hint {
            return false;
        }
        for line in std::mem::take(&mut self.foreground_command_hint_lines) {
            self.hide_lines(line..line + 1);
        }
        if let Some(hint) = &hint {
            let runs: Vec<_> = hint
                .line
                .spans
                .iter()
                .map(|span| (span.text.clone(), span.role))
                .collect();
            for call in hint.calls.iter().flatten() {
                let Some(at) = self.call_row_end(call) else {
                    continue;
                };
                let mut row = Scrollback::new();
                row.append_response(&runs);
                row.lines.truncate(1);
                row.line_styles.truncate(1);
                self.insert_lines(at, row.lines, row.line_styles);
                if let Some(header) = self.tool_call_headers.get_mut(call) {
                    header.results_end = at;
                }
                self.foreground_command_hint_lines.push(at);
            }
        }
        self.foreground_command_hint = hint;
        true
    }

    /// Where the next result of `call` goes, when the call has a row of its own here.
    fn call_row_end(&self, call: &str) -> Option<usize> {
        self.tool_call_headers
            .get(call)
            .filter(|header| header.collapse.is_none())
            .map(|header| header.results_end)
    }

    pub(super) fn shows_foreground_command_hint(&self) -> bool {
        self.foreground_command_hint.is_some()
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
        self.mark_last_line_dirty();
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
        self.frame_at(
            width,
            height,
            prefers_reduced_motion,
            tickloop_agent_base::private_fs::unix_time_milliseconds(),
        )
    }

    pub(super) fn frame_at(
        &mut self,
        width: usize,
        height: usize,
        prefers_reduced_motion: bool,
        now_ms: u64,
    ) -> ScrollFrame {
        if width == 0 || height == 0 {
            return ScrollFrame {
                top: 0,
                lines: Vec::new(),
                styled: Vec::new(),
                following: self.following,
                unread: self.unread,
            };
        }
        self.prepare_frame(width, height);
        // Only the rows the window shows are copied, so a frame costs its height whatever the
        // length of the conversation above it.
        let tail = self.tail_rows();
        let row_count = self.conversation_rows + tail.len();
        let top = self.top_row_within(row_count);
        let end = top.saturating_add(height).min(row_count);
        let conversation_end = end.min(self.conversation_rows);
        let running_tool_rows = self.running_tool_rows();
        let styled: Vec<RoleLine> = (top.min(conversation_end)..conversation_end)
            .map(|index| {
                let mut styled = self.rows[index].styled.clone();
                if !prefers_reduced_motion
                    && let Some(flash) = self.line_styles[self.rows[index].start.line]
                        .flash
                        .filter(|(_, since)| *since >= self.flashes_since_ms)
                    && let Some(fill) = super::list_row::flash_fill(
                        Some(flash),
                        super::display::DisplayProfile::default(),
                        now_ms,
                    )
                {
                    let columns = unicode_width::UnicodeWidthStr::width(styled.text().as_str());
                    let first_line = &self.lines[self.rows[index].start.line];
                    let start = first_line
                        .find(' ')
                        .map(|byte| unicode_width::UnicodeWidthStr::width(&first_line[..byte + 1]))
                        .unwrap_or(0);
                    let start = if self.rows[index].start.byte == 0 {
                        start
                    } else {
                        0
                    };
                    styled = styled.banded(start.min(columns)..columns, fill);
                }
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
            .chain(
                tail[top.saturating_sub(self.conversation_rows)
                    ..end.saturating_sub(self.conversation_rows)]
                    .iter()
                    .map(|row| row.styled(self.width)),
            )
            .collect();
        if end == row_count {
            self.unread = 0;
        }
        ScrollFrame {
            top,
            lines: styled.iter().map(RoleLine::text).collect(),
            styled,
            following: self.following,
            unread: self.unread,
        }
    }

    /// Text of the rows in `range`, numbered as a frame numbers them: the conversation, then its
    /// tail. Rows past the end are left out.
    pub(super) fn row_texts(&mut self, range: Range<usize>) -> Vec<String> {
        self.refresh_rows();
        let tail = self.tail_rows();
        range
            .map_while(|index| match index.checked_sub(self.conversation_rows) {
                None => Some(self.rows[index].styled.text()),
                Some(tail_index) => Some(tail.get(tail_index)?.styled(self.width).text()),
            })
            .collect()
    }

    /// The rows below the conversation: a running command's output under its echo and the hint
    /// for a running Bash call, the live activity and its details as a block of their own, then
    /// the blank row that keeps everything above clear of the input.
    fn tail_rows(&self) -> Vec<TailRow<'_>> {
        let unplaced_hint = self.foreground_command_hint.as_ref().filter(|hint| {
            hint.calls.iter().any(|call| {
                call.as_deref()
                    .and_then(|call| self.call_row_end(call))
                    .is_none()
            })
        });
        let mut rows: Vec<TailRow> = self
            .shell_preview
            .iter()
            .chain(unplaced_hint.map(|hint| &hint.line))
            .map(TailRow::Clipped)
            .collect();
        let has_conversation = self.conversation_rows > 0 || !rows.is_empty();
        if self.activity.is_some() || !self.activity_details.is_empty() {
            if has_conversation {
                rows.push(TailRow::Blank);
            }
            rows.extend(self.activity.as_ref().map(TailRow::Clipped));
            rows.extend(self.activity_details.iter().map(TailRow::Whole));
        }
        if has_conversation || !rows.is_empty() {
            rows.push(TailRow::Blank);
        }
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
        self.conversation_rows + self.tail_rows().len()
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

    /// The row the next frame draws first.
    pub(super) fn top_row(&self) -> usize {
        self.top_row_within(self.row_count())
    }

    /// The first row shown when the conversation and its tail take `row_count` rows.
    fn top_row_within(&self, row_count: usize) -> usize {
        let maximum = row_count.saturating_sub(self.height);
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
                // One blank row separates blocks, however many blank lines lie between them.
                let repeats_blank =
                    is_blank_line(text, style) && self.rows.last().is_some_and(Row::is_blank);
                if !repeats_blank {
                    wrap_line(&mut self.rows, text, style, line, self.width);
                }
            }
        }
        self.conversation_rows = self
            .rows
            .iter()
            .rposition(|row| !row.is_blank())
            .map_or(0, |last| last + 1);
    }
}

impl Row {
    fn is_blank(&self) -> bool {
        self.text.is_empty() && self.styled.fill.is_none()
    }
}

/// One row below the conversation, borrowed until a frame shows it.
enum TailRow<'a> {
    /// Cut to the frame's width.
    Clipped(&'a RoleLine),
    Whole(&'a RoleLine),
    Blank,
}

impl TailRow<'_> {
    fn styled(&self, width: usize) -> RoleLine {
        match self {
            Self::Clipped(row) => {
                let mut row = (*row).clone();
                row.truncate(width);
                row
            }
            Self::Whole(row) => (*row).clone(),
            Self::Blank => RoleLine::default(),
        }
    }
}

fn is_blank_line(text: &str, style: &LineStyle) -> bool {
    text.is_empty() && style.markdown.is_none() && style.fill.is_none()
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
    if let Some(token) = marks.code {
        Some(Role::code(token))
    } else if marks.quote_bar {
        Some(Role::QuoteBar)
    } else if marks.link {
        Some(Role::Link)
    } else if marks.inline_code && (marks.strong || marks.heading.is_some()) {
        Some(Role::StrongCode)
    } else if marks.inline_code {
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

/// One display cell of a logical line: a character, or one column of an expanded tab.
struct Cell<'a> {
    byte: usize,
    tab_cell: usize,
    character: char,
    width: usize,
    role: Option<Role>,
    link: Option<&'a Hyperlink>,
}

impl Cell<'_> {
    fn is_space(&self) -> bool {
        self.character == ' '
    }
}

fn line_cells<'a>(text: &str, style: &'a LineStyle, width: usize) -> Vec<Cell<'a>> {
    let mut cells = Vec::with_capacity(text.len());
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
        let cells_of_character = if character == '\t' {
            TAB_COLUMNS - logical_column % TAB_COLUMNS
        } else {
            1
        };
        for tab_cell in 0..cells_of_character {
            let displayed = if character == '\t' { ' ' } else { character };
            let displayed = if displayed.width().unwrap_or(0) > width {
                '�'
            } else {
                displayed
            };
            cells.push(Cell {
                byte,
                tab_cell,
                character: displayed,
                width: displayed.width().unwrap_or(0),
                role,
                link,
            });
            logical_column += if character == '\t' {
                1
            } else {
                character.width().unwrap_or(0)
            };
        }
    }
    cells
}

/// Lays one logical line out in rows of `width` columns. Rows break between words where they
/// can, and wrapped rows start `hang` columns in, under the line's content; a break never falls
/// inside that hanging lead. A word longer than a row breaks between characters.
fn wrap_line(rows: &mut Vec<Row>, text: &str, style: &LineStyle, line: usize, width: usize) {
    let hang = if style.hang < width { style.hang } else { 0 };
    let cells = line_cells(text, style, width);
    let mut row_start = 0;
    let mut indent = 0;
    let mut columns = 0;
    // The first cell of the latest word in this row that follows a space past the hang.
    let mut word_start: Option<usize> = None;
    let mut after_word = false;
    let mut after_space = false;
    let mut index = 0;
    let mut pushed = false;
    while index < cells.len() {
        let cell = &cells[index];
        if columns > indent && columns + cell.width > width {
            let (row_end, next) = if cell.is_space() {
                let next = cells[index..]
                    .iter()
                    .position(|cell| !cell.is_space())
                    .map_or(cells.len(), |offset| index + offset);
                (index, next)
            } else if let Some(word) = word_start.filter(|word| *word > row_start) {
                (word, word)
            } else {
                (index, index)
            };
            let row_end = trim_trailing_spaces(&cells, row_start, row_end);
            push_row(
                rows,
                &cells[row_start..row_end],
                indent,
                style,
                line,
                row_start == 0,
            );
            pushed = true;
            if next >= cells.len() {
                return;
            }
            row_start = next;
            indent = if hang > 0 && cells[next].width + hang <= width {
                hang
            } else {
                0
            };
            columns = indent
                + cells[next..index.max(next)]
                    .iter()
                    .map(|cell| cell.width)
                    .sum::<usize>();
            word_start = None;
            after_word = cells[next..index.max(next)]
                .iter()
                .any(|cell| !cell.is_space());
            after_space = false;
            index = index.max(next);
            continue;
        }
        if cell.is_space() {
            after_space = after_word;
        } else {
            if after_space && columns > hang {
                word_start = Some(index);
            }
            after_space = false;
            after_word = true;
        }
        columns += cell.width;
        index += 1;
    }
    if row_start < cells.len() || !pushed {
        push_row(
            rows,
            &cells[row_start..],
            indent,
            style,
            line,
            row_start == 0,
        );
    }
}

fn trim_trailing_spaces(cells: &[Cell], start: usize, end: usize) -> usize {
    cells[start..end]
        .iter()
        .rposition(|cell| !cell.is_space())
        .map_or(start, |last| start + last + 1)
}

fn push_row(
    rows: &mut Vec<Row>,
    cells: &[Cell],
    indent: usize,
    style: &LineStyle,
    line: usize,
    starts_line: bool,
) {
    let start = match cells.first() {
        Some(cell) if !starts_line => Position {
            line,
            byte: cell.byte,
            tab_cell: cell.tab_cell,
        },
        _ => Position {
            line,
            byte: 0,
            tab_cell: 0,
        },
    };
    let mut styled = RoleLine {
        fill: style.fill,
        ..Default::default()
    };
    styled.push(" ".repeat(indent), None);
    for cell in cells {
        styled.push_linked(cell.character.to_string(), cell.role, cell.link.cloned());
    }
    rows.push(Row {
        start,
        text: styled.text(),
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
    fn native_completion_waits_for_take_in_and_never_replays_after_a_view_switch() {
        let instance = tick_protocol::InstanceRef::shell(Uuid::new_v4());
        let mut scroll = Scrollback::new();
        scroll.append_flashing_label(
            &[("⏺ Background command completed\n".into(), None)],
            None,
            Some(instance.clone()),
        );
        let mut membership = tickloop_agent_protocol::MonitorMembership::default();
        scroll.sync_native_take_in(&membership);
        assert!(scroll.line_styles.iter().all(|style| style.flash.is_none()));
        membership.mark_taken_in(instance.clone(), 1_000);
        scroll.sync_native_take_in(&membership);
        let frame = scroll.frame_at(40, 10, false, 1_100);
        let row = frame
            .styled
            .iter()
            .find(|row| row.text().contains("Background"))
            .unwrap();
        assert_eq!(row.band.as_ref().unwrap().columns.start, 2);
        assert_eq!(row.band.as_ref().unwrap().role, Role::TakeInFlashStrong);
        membership.mark_taken_in(instance, 2_000);
        scroll.sync_native_take_in(&membership);
        assert!(
            scroll
                .line_styles
                .iter()
                .filter_map(|style| style.flash)
                .all(|(_, since)| since == 1_000)
        );
        scroll.settle_flashes(2_000);
        assert!(
            scroll
                .frame_at(40, 10, false, 2_000)
                .styled
                .iter()
                .all(|line| line.band.is_none())
        );
    }

    #[test]
    fn tick_labels_share_the_monitor_flash_phases_and_reduce_motion() {
        use super::super::{
            display::DisplayProfile,
            list_row::{RowFlash, flash_fill},
        };
        let mut scroll = Scrollback::new();
        scroll.append_flashing_label(
            &[("Tick update\n".into(), Some(Role::MutedText))],
            Some((RowFlash::TakeIn, 1_000)),
            None,
        );
        scroll.set_tool_call(
            Uuid::from_u128(1),
            "read",
            &[("TickRead".into(), Some(Role::ToolName))],
            ToolCallState::Finished,
            None,
        );
        scroll.flash_tool_call("read", 1_000);
        for now in [1_000, 1_400, 1_800, 2_200] {
            let frame = scroll.frame_at(40, 10, false, now);
            for (text, kind) in [
                ("Tick update", RowFlash::TakeIn),
                ("TickRead", RowFlash::Use),
            ] {
                let row = frame
                    .styled
                    .iter()
                    .find(|row| row.text().contains(text))
                    .unwrap();
                assert_eq!(
                    row.band.as_ref().map(|band| band.role),
                    flash_fill(Some((kind, 1_000)), DisplayProfile::default(), now)
                );
            }
        }
        assert!(
            scroll
                .frame_at(40, 10, true, 1_000)
                .styled
                .iter()
                .all(|line| line.band.is_none())
        );
        scroll.settle_flashes(1_001);
        assert!(
            scroll
                .frame_at(40, 10, false, 1_100)
                .styled
                .iter()
                .all(|line| line.band.is_none())
        );
        assert_eq!(
            scroll.next_flash_frame_ms(1_100, DisplayProfile::default()),
            None
        );
    }

    #[test]
    fn appended_roles_follow_their_text_onto_rows() {
        let mut scrollback = Scrollback::new();
        scrollback.append("plain ");
        scrollback.append_as("tool", Some(Role::ToolName), None);
        scrollback.append("(x)\nnext");
        let frame = scrollback.frame(40, 5);
        assert_eq!(frame.lines, ["plain tool(x)", "next", ""]);
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
    fn a_link_follows_its_text_across_wrapped_rows() {
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

        assert_eq!(scrollback.frame(40, 5).lines, ["❯ see [Image #1] now", ""]);
        assert_eq!(linked_text(&mut scrollback, 40), ["[Image #1]"]);
        assert_eq!(linked_text(&mut scrollback, 12), ["[Image", "#1]"]);
    }

    #[test]
    fn a_fill_marks_only_the_lines_that_received_text() {
        let mut scrollback = Scrollback::new();
        scrollback.append("before\n");
        scrollback.append_as("\n❯ hi\n", None, Some(Role::UserMessageBg));
        scrollback.append("after");
        let frame = scrollback.frame(40, 6);
        let fills: Vec<_> = frame.styled.iter().map(|line| line.fill).collect();
        assert_eq!(frame.lines, ["before", "", "❯ hi", "after", ""]);
        assert_eq!(fills, [None, None, Some(Role::UserMessageBg), None, None]);
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
        assert_eq!(frame.lines, ["⏺ abcd", "  efgh", "  ij", "out", ""]);
    }

    #[test]
    fn a_body_wraps_under_its_indent_and_a_response_under_its_connector() {
        let mut scrollback = Scrollback::new();
        scrollback.append("\n⏺ Teammate @a finished\n");
        scrollback.append_body(&[("  ".into(), None), ("one two three\n".into(), None)]);
        scrollback.append_response(&[("  ⎿  ".into(), None), ("one two three\n".into(), None)]);
        let frame = scrollback.frame(12, 8);
        assert_eq!(
            frame.lines,
            [
                "",
                "⏺ Teammate",
                "@a finished",
                "  one two",
                "  three",
                "  ⎿  one two",
                "     three",
                "",
            ]
        );
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
                "✓ Done",
                ""
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
                "",
                "⏺ ┌─────┬─────┐",
                "  │  a  │  b  │",
                "  ├─────┼─────┤",
                "  │ 1   │ 2   │",
                "  └─────┴─────┘",
                "",
            ]
        );
        assert_eq!(scrollback.frame(12, 10).lines, ["", "⏺ a: 1", "  b: 2", ""]);
        scrollback.append_markdown("second", "⏺ ", true);
        scrollback.append_markdown(" reply", "⏺ ", false);
        assert_eq!(
            scrollback.frame(40, 10).lines[6..],
            ["", "⏺ second reply", ""]
        );
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

    fn done_notice(usage: &str) -> Vec<(String, Option<Role>)> {
        vec![
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
    fn model_retry_updates_its_original_row_and_reflows_without_stale_text() {
        let mut scrollback = Scrollback::new();
        let run = Uuid::new_v4();
        let kind = ModelRequestKind::Conversation;
        scrollback.append("Tool failed\n");
        scrollback.set_model_retry(
            run,
            kind,
            1,
            &[
                (
                    "Model request: a long connection error".into(),
                    Some(Role::Error),
                ),
                (" · attempt 2/11".into(), Some(Role::MutedText)),
            ],
        );
        scrollback.append("Other output\n");
        assert!(scrollback.frame(20, 20).lines.len() > 4);
        scrollback.set_model_retry(
            run,
            kind,
            2,
            &[
                ("Model request: lost".into(), Some(Role::Error)),
                (" · attempt 3/11".into(), Some(Role::MutedText)),
            ],
        );
        let frame = scrollback.frame(80, 20);
        assert_eq!(
            frame.lines,
            [
                "Tool failed",
                "Model request: lost · attempt 3/11",
                "Other output",
                ""
            ]
        );
        assert_eq!(
            roles_of(&frame.styled[1]),
            [
                ("Model request: lost".into(), Some(Role::Error)),
                (" · attempt 3/11".into(), Some(Role::MutedText)),
            ]
        );
        assert_eq!(
            scrollback.frame(20, 20).lines,
            [
                "Tool failed",
                "Model request:",
                "lost · attempt 3/11",
                "Other output",
                ""
            ]
        );
    }

    #[test]
    fn model_retry_keeps_other_requests_and_new_retry_sequences_separate() {
        let mut scrollback = Scrollback::new();
        let run = Uuid::new_v4();
        let other_run = Uuid::new_v4();
        let conversation = ModelRequestKind::Conversation;
        let review = ModelRequestKind::PermissionCheck;
        for (run, kind, attempt, text) in [
            (run, conversation, 1, "old attempt"),
            (run, review, 1, "review attempt"),
            (other_run, conversation, 1, "other run"),
            (run, conversation, 2, "latest attempt"),
            (run, conversation, 1, "new request attempt"),
            (run, conversation, 2, "new request latest"),
        ] {
            scrollback.set_model_retry(run, kind, attempt, &[(text.into(), None)]);
        }
        scrollback.append("Failed: final failure\n");
        assert_eq!(
            scrollback.frame(80, 20).lines,
            [
                "latest attempt",
                "review attempt",
                "other run",
                "new request latest",
                "Failed: final failure",
                "",
            ]
        );
    }

    #[test]
    fn model_retry_tracks_insertions_before_its_row() {
        let mut scrollback = Scrollback::new();
        let run = Uuid::new_v4();
        let kind = ModelRequestKind::Conversation;
        scrollback.set_tool_call(
            run,
            "call",
            &[("Tool".into(), None)],
            ToolCallState::Running,
            None,
        );
        scrollback.set_model_retry(run, kind, 1, &[("old attempt".into(), None)]);
        scrollback.append_tool_result("call", &[("Tool result\n".into(), None)]);
        scrollback.set_model_retry(run, kind, 2, &[("latest attempt".into(), None)]);
        let text = scrollback.frame(80, 20).lines.join("\n");
        assert!(text.contains("Tool result"), "{text}");
        assert!(text.contains("latest attempt"), "{text}");
        assert!(!text.contains("old attempt"), "{text}");
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
        assert_eq!(working.lines, ["", "⏺ reply", "", "✢ Working", ""]);
        assert_eq!(working.styled[1].spans[0].role, Some(Role::Text));

        scrollback.set_activity(None);
        let finished = scrollback.frame(20, 5);
        assert_eq!(finished.lines, ["", "⏺ reply", ""]);
        assert_eq!(finished.styled[1].spans[0].role, Some(Role::Text));
    }

    #[test]
    fn a_running_tool_name_shimmers_until_finished() {
        let mut scrollback = Scrollback::new();
        scrollback.append("before");
        scrollback.set_tool_call(
            Uuid::from_u128(1),
            "call",
            &[
                ("\n".into(), None),
                ("⏺ ".into(), Some(Role::MutedText)),
                ("Shell".into(), Some(Role::ToolName)),
                ("\n".into(), None),
            ],
            ToolCallState::Running,
            None,
        );
        scrollback.append_as("  ⎿ output\n", Some(Role::MutedText), None);
        let running = scrollback.frame_with_motion(20, 5, true);
        assert_eq!(running.lines, ["before", "", "⏺ Shell", "  ⎿ output", ""]);
        assert!(
            running.styled[2]
                .spans
                .iter()
                .any(|span| span.role == Some(Role::RunningShade0))
        );
        assert_eq!(running.styled[3].spans[0].role, Some(Role::MutedText));

        scrollback.finish_tool_call(Uuid::from_u128(1), "call", false);
        assert_eq!(
            scrollback.frame(20, 5).styled[2]
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
        assert_eq!(scrollback.frame(80, 4).lines[2], "row 1500");
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
            ["abcde", "fghij", "中文", "tail", ""]
        );
        assert_eq!(
            scrollback.frame(12, 10).lines,
            ["abcdefghij", "中文", "tail", ""]
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

        // The spaces of a tab the row wraps at are dropped, so the next row starts after it.
        let mut tabs = Scrollback::new();
        tabs.append("\t中文\n后续内容");
        tabs.frame(1, 2);
        tabs.scroll(ScrollAction::First);
        tabs.scroll(ScrollAction::HalfPageDown);
        let tab_anchor = tabs.anchor;
        assert_eq!(
            tab_anchor,
            Position {
                line: 0,
                byte: 1,
                tab_cell: 0
            }
        );
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
        assert_eq!(
            scrollback.frame(20, 100).lines,
            ["中  文�[31m", "    尾", ""]
        );
        assert_eq!(scrollback.lines[0], "中\t文�[31m");
    }

    #[test]
    fn half_page_scroll_pauses_and_reaching_the_bottom_resumes() {
        let mut scrollback = Scrollback::new();
        scrollback.append(&numbered_lines(20));
        assert_eq!(scrollback.frame(40, 6).lines[0], "row 15");
        scrollback.scroll(ScrollAction::HalfPageUp);
        let paused = scrollback.frame(40, 6);
        assert_eq!(paused.lines[0], "row 12");
        assert!(!paused.following);
        scrollback.append("\nrow 20");
        assert_eq!(scrollback.frame(40, 6).lines[0], "row 12");
        scrollback.scroll(ScrollAction::HalfPageDown);
        assert!(!scrollback.frame(40, 6).following);
        scrollback.scroll(ScrollAction::HalfPageDown);
        assert!(scrollback.frame(40, 6).following);
        scrollback.append("\nrow 21");
        assert_eq!(scrollback.frame(40, 6).lines[4], "row 21");
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
        assert_eq!(scrollback.frame(40, 4).lines[0], "row 17");
        scrollback.scroll(ScrollAction::Last);
        let manual_bottom = scrollback.frame(40, 4);
        assert_eq!(manual_bottom.lines[2], "row 20");
        assert!(!manual_bottom.following);
        scrollback.append("\nrow 21");
        scrollback.note_message("new reply".into());
        let paused = scrollback.frame(40, 4);
        assert_eq!(paused.lines[0], manual_bottom.lines[0]);
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
        assert_eq!(visible.lines, ["first", "second", ""]);
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

    fn call_spans(name: &str) -> Vec<(String, Option<Role>)> {
        vec![
            ("\n".into(), None),
            ("⏺ ".into(), Some(Role::MutedText)),
            (name.into(), Some(Role::ToolName)),
            ("\n".into(), None),
        ]
    }

    fn result(text: &str) -> Vec<(String, Option<Role>)> {
        vec![
            ("  ".into(), None),
            ("⎿".into(), Some(Role::MutedText)),
            ("  ".into(), None),
            (format!("{text}\n"), None),
        ]
    }

    fn echo(scrollback: &mut Scrollback, text: &str) -> InputEcho {
        scrollback.start_block();
        let first_line = scrollback.line_count() - 1;
        scrollback.append_as(&format!("❯ {text}\n"), None, Some(Role::UserMessageBg));
        scrollback.mark_input(first_line..scrollback.line_count() - 1)
    }

    #[test]
    fn every_block_has_exactly_one_blank_row_above_it() {
        let run = Uuid::from_u128(1);
        let mut scrollback = Scrollback::new();
        echo(&mut scrollback, "first");
        scrollback.append_markdown("A reply.", "⏺ ", true);
        // A call right after a reply, which still ends on its Markdown line.
        scrollback.set_tool_call(run, "a", &call_spans("Bash"), ToolCallState::Running, None);
        scrollback.append_tool_result("a", &result("done"));
        // Writers that open with several newlines still leave one blank row.
        scrollback.append("\n\n\n");
        scrollback.append_completion_notice(&[("⏺ Done".into(), None), ("\n".into(), None)]);
        scrollback.start_block();
        scrollback.append("⚠ A warning\n");
        assert_eq!(
            scrollback.frame(40, 20).lines,
            [
                "",
                "❯ first",
                "",
                "⏺ A reply.",
                "",
                "⏺ Bash",
                "  ⎿  done",
                "",
                "⏺ Done",
                "",
                "⚠ A warning",
                "",
            ]
        );
    }

    #[test]
    fn hidden_rows_between_blocks_leave_one_blank_row() {
        let mut scrollback = Scrollback::new();
        echo(&mut scrollback, "kept");
        let withdrawn = echo(&mut scrollback, "withdrawn");
        scrollback.append_completion_notice(&[("⏺ Done".into(), None), ("\n".into(), None)]);
        scrollback.hide_echo(withdrawn);
        scrollback.hide_completion_notice();
        echo(&mut scrollback, "next");
        assert_eq!(
            scrollback.frame(40, 20).lines,
            ["", "❯ kept", "", "❯ next", ""]
        );
    }

    #[test]
    fn one_blank_row_ends_the_conversation_idle_or_working() {
        let mut scrollback = Scrollback::new();
        scrollback.append_markdown("Reply.", "⏺ ", true);
        scrollback.append("\n\n");
        assert_eq!(scrollback.frame(30, 10).lines, ["", "⏺ Reply.", ""]);

        scrollback.set_activity(Some(RoleLine::plain("✢ Working")));
        scrollback.set_activity_details(vec![RoleLine::plain("  ⎿  ◼ Task")]);
        assert_eq!(
            scrollback.frame(30, 10).lines,
            ["", "⏺ Reply.", "", "✢ Working", "  ⎿  ◼ Task", ""]
        );

        scrollback.set_activity(None);
        scrollback.set_activity_details(Vec::new());
        echo(&mut scrollback, "!sleep 1");
        assert!(scrollback.set_shell_preview(vec![RoleLine::plain("  ⎿  Running…")]));
        assert_eq!(
            scrollback.frame(30, 10).lines[3..],
            ["❯ !sleep 1", "  ⎿  Running…", ""],
            "a running command's output sits under its echo"
        );
    }

    #[test]
    fn each_result_stays_under_its_own_call() {
        let run = Uuid::from_u128(1);
        let mut scrollback = Scrollback::new();
        scrollback.set_tool_call(run, "a", &call_spans("First"), ToolCallState::Running, None);
        scrollback.set_tool_call(
            run,
            "b",
            &call_spans("Second"),
            ToolCallState::Running,
            None,
        );
        scrollback.append_tool_result("b", &result("second result"));
        scrollback.append_tool_result("a", &result("first result"));
        scrollback.append_tool_result("a", &result("first again"));
        scrollback.finish_tool_call(run, "a", false);
        scrollback.finish_tool_call(run, "b", false);
        scrollback.append_markdown("After both.", "⏺ ", true);
        assert_eq!(
            scrollback.frame(40, 20).lines,
            [
                "",
                "⏺ First",
                "  ⎿  first result",
                "  ⎿  first again",
                "",
                "⏺ Second",
                "  ⎿  second result",
                "",
                "⏺ After both.",
                "",
            ]
        );
    }

    #[test]
    fn consecutive_reads_and_searches_collapse_into_one_summary_row() {
        let run = Uuid::from_u128(1);
        let read = |path: &str| Some(CollapsedCall::Read { path: path.into() });
        let mut scrollback = Scrollback::new();
        let read_spans = call_spans("Read");
        scrollback.set_tool_call(run, "a", &read_spans, ToolCallState::Running, read("a.txt"));
        scrollback.set_tool_call(
            run,
            "b",
            &call_spans("Grep"),
            ToolCallState::Queued,
            Some(CollapsedCall::Search),
        );
        assert_eq!(
            scrollback.frame(80, 10).lines,
            [
                "",
                "⏺ Searching for 1 pattern, reading 1 file… (ctrl+o to expand)",
                ""
            ]
        );
        scrollback.append_tool_result("a", &result("file content"));
        scrollback.finish_tool_call(run, "a", false);
        scrollback.finish_tool_call(run, "b", false);
        // A thinking label passes the group; the same file read again counts once.
        scrollback.append_aside("∴ Thinking\n", None);
        scrollback.set_tool_call(run, "c", &read_spans, ToolCallState::Running, read("a.txt"));
        scrollback.set_tool_call(run, "d", &read_spans, ToolCallState::Running, read("b.txt"));
        scrollback.finish_tool_call(run, "c", false);
        scrollback.finish_tool_call(run, "d", true);
        scrollback.append_markdown("Text ends the group.", "⏺ ", true);
        scrollback.set_tool_call(
            run,
            "e",
            &read_spans,
            ToolCallState::Finished,
            read("c.txt"),
        );
        assert_eq!(
            scrollback.frame(80, 20).lines,
            [
                "",
                "  Searched for 1 pattern, read 2 files (ctrl+o to expand)",
                "",
                "∴ Thinking",
                "",
                "⏺ Text ends the group.",
                "",
                "  Read 1 file (ctrl+o to expand)",
                "",
            ]
        );
    }

    #[test]
    fn results_and_echoes_wrap_between_words_under_their_content() {
        let run = Uuid::from_u128(1);
        let mut scrollback = Scrollback::new();
        scrollback.set_tool_call(run, "a", &call_spans("Bash"), ToolCallState::Running, None);
        scrollback.append_tool_result("a", &result("alpha beta gamma delta epsilon"));
        scrollback.start_block();
        scrollback.append_hanging("! ", None, Some(Role::ShellMessageBg), None, 2);
        scrollback.append_hanging(
            "printf one two three\n",
            None,
            Some(Role::ShellMessageBg),
            None,
            2,
        );
        assert_eq!(
            scrollback.frame(18, 20).lines,
            [
                "",
                "⏺ Bash",
                "  ⎿  alpha beta",
                "     gamma delta",
                "     epsilon",
                "",
                "! printf one two",
                "  three",
                "",
            ]
        );
        assert_eq!(
            scrollback.frame(8, 20).lines[2..5],
            ["  ⎿  alp", "     ha", "     bet"],
            "a word longer than the row breaks between characters"
        );
    }

    /// Mean cost of one followed frame with `rows` conversation rows above a running call and a
    /// live activity row, as while the model thinks.
    #[test]
    #[ignore = "timing benchmark: cargo test --release --config profile.release.debug-assertions=true -p tickloop-agent-console --lib bench_ -- --ignored --nocapture"]
    fn bench_followed_frame_cost_by_conversation_rows() {
        const FRAMES: u32 = 200;
        for rows in [1_000, 10_000, 50_000] {
            let mut scrollback = Scrollback::new();
            scrollback.append(&numbered_lines(rows));
            scrollback.set_tool_call(
                Uuid::from_u128(1),
                "call",
                &[
                    ("\n".into(), None),
                    ("⏺ ".into(), Some(Role::MutedText)),
                    ("Shell".into(), Some(Role::ToolName)),
                    ("\n".into(), None),
                ],
                ToolCallState::Running,
                None,
            );
            scrollback.set_activity(Some(RoleLine::role("• Thinking…", Role::MutedText)));
            assert!(scrollback.frame_with_motion(120, 40, false).following);
            let started = std::time::Instant::now();
            for _ in 0..FRAMES {
                std::hint::black_box(scrollback.frame_with_motion(120, 40, false));
            }
            println!(
                "BENCH scrollback_frame rows={rows} per_frame_us={:.1}",
                started.elapsed().as_secs_f64() * 1e6 / f64::from(FRAMES)
            );
        }
    }
}
