use std::{
    ops::Range,
    path::{Path, PathBuf},
};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::file_change::{FileChangeAction, FileChangeLineKind, FileChangeReview};

const TAB_COLUMNS: usize = 4;
const MAX_CONSECUTIVE_ZERO_WIDTH_CHARACTERS: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DiffViewport {
    pub width: usize,
    pub height: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiffNavigationAction {
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DiffNavigation {
    pub anchor: Option<DiffAnchor>,
    pub stick_to_end: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffAnchor {
    pub path: PathBuf,
    pub location: DiffLocation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiffLocation {
    Header {
        row: usize,
        cell_offset: usize,
    },
    HunkHeader {
        hunk: usize,
        cell_offset: usize,
    },
    SourceLine {
        hunk: usize,
        line: usize,
        cell_offset: usize,
    },
    Error {
        row: usize,
        cell_offset: usize,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiffLineKind {
    Header,
    Hunk,
    Context,
    Added,
    Removed,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderedDiffLine {
    pub kind: DiffLineKind,
    pub old_line: Option<u64>,
    pub new_line: Option<u64>,
    pub text: String,
    pub continuation: bool,
    pub anchor: DiffAnchor,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffFrame {
    pub lines: Vec<RenderedDiffLine>,
    pub navigation: DiffNavigation,
    pub top_row: usize,
    pub total_rows: usize,
    pub can_move_up: bool,
    pub can_move_down: bool,
}

pub fn render_diff(
    review: &FileChangeReview,
    navigation: &DiffNavigation,
    viewport: DiffViewport,
) -> DiffFrame {
    if viewport.width == 0 || viewport.height == 0 {
        return empty_frame();
    }
    frame(
        &ReviewLayout::new(review, viewport.width),
        navigation,
        viewport,
    )
}

pub fn navigate_diff(
    review: &FileChangeReview,
    navigation: &DiffNavigation,
    viewport: DiffViewport,
    action: DiffNavigationAction,
) -> DiffNavigation {
    if viewport.width == 0 || viewport.height == 0 {
        return DiffNavigation::default();
    }
    navigate(
        &ReviewLayout::new(review, viewport.width),
        navigation,
        viewport,
        action,
    )
}

pub fn render_error(
    title: &str,
    message: &str,
    navigation: &DiffNavigation,
    viewport: DiffViewport,
) -> DiffFrame {
    if viewport.width == 0 || viewport.height == 0 {
        return empty_frame();
    }
    frame(
        &ErrorLayout::new(title, message, viewport.width),
        navigation,
        viewport,
    )
}

pub fn navigate_error(
    title: &str,
    message: &str,
    navigation: &DiffNavigation,
    viewport: DiffViewport,
    action: DiffNavigationAction,
) -> DiffNavigation {
    if viewport.width == 0 || viewport.height == 0 {
        return DiffNavigation::default();
    }
    navigate(
        &ErrorLayout::new(title, message, viewport.width),
        navigation,
        viewport,
        action,
    )
}

fn empty_frame() -> DiffFrame {
    DiffFrame {
        lines: Vec::new(),
        navigation: DiffNavigation::default(),
        top_row: 0,
        total_rows: 0,
        can_move_up: false,
        can_move_down: false,
    }
}

fn frame(
    layout: &impl DiffLayout,
    navigation: &DiffNavigation,
    viewport: DiffViewport,
) -> DiffFrame {
    let measurement = layout.measure(navigation.anchor.as_ref());
    let total_rows = measurement.total_rows;
    let maximum_top = total_rows.saturating_sub(viewport.height.max(1));
    let top_row = if navigation.stick_to_end {
        maximum_top
    } else {
        measurement.anchor_row.min(maximum_top)
    };
    let lines = layout.materialize(top_row..top_row.saturating_add(viewport.height));
    let normalized_navigation = DiffNavigation {
        anchor: lines.first().map(|row| row.anchor.clone()),
        stick_to_end: navigation.stick_to_end,
    };
    DiffFrame {
        lines,
        navigation: normalized_navigation,
        top_row,
        total_rows,
        can_move_up: top_row > 0,
        can_move_down: top_row < maximum_top,
    }
}

fn navigate(
    layout: &impl DiffLayout,
    navigation: &DiffNavigation,
    viewport: DiffViewport,
    action: DiffNavigationAction,
) -> DiffNavigation {
    let measurement = layout.measure(navigation.anchor.as_ref());
    if measurement.total_rows == 0 {
        return DiffNavigation::default();
    }
    let maximum_top = measurement
        .total_rows
        .saturating_sub(viewport.height.max(1));
    let current = if navigation.stick_to_end {
        maximum_top
    } else {
        measurement.anchor_row.min(maximum_top)
    };
    let page = viewport.height.max(1);
    let target = match action {
        DiffNavigationAction::Up => current.saturating_sub(1),
        DiffNavigationAction::Down => current.saturating_add(1).min(maximum_top),
        DiffNavigationAction::PageUp => current.saturating_sub(page),
        DiffNavigationAction::PageDown => current.saturating_add(page).min(maximum_top),
        DiffNavigationAction::Home => 0,
        DiffNavigationAction::End => maximum_top,
    };
    DiffNavigation {
        anchor: layout.anchor_at(target),
        stick_to_end: matches!(
            action,
            DiffNavigationAction::Down | DiffNavigationAction::PageDown | DiffNavigationAction::End
        ) && target == maximum_top,
    }
}

#[cfg(test)]
fn same_logical_row(candidate: &DiffAnchor, target: &DiffAnchor) -> bool {
    if candidate.path != target.path {
        return false;
    }
    match (&candidate.location, &target.location) {
        (DiffLocation::Header { row: left, .. }, DiffLocation::Header { row: right, .. }) => {
            left == right
        }
        (
            DiffLocation::HunkHeader {
                hunk: left_hunk, ..
            },
            DiffLocation::HunkHeader {
                hunk: right_hunk, ..
            },
        ) => left_hunk == right_hunk,
        (
            DiffLocation::SourceLine {
                hunk: left_hunk,
                line: left_line,
                ..
            },
            DiffLocation::SourceLine {
                hunk: right_hunk,
                line: right_line,
                ..
            },
        ) => left_hunk == right_hunk && left_line == right_line,
        (DiffLocation::Error { row: left, .. }, DiffLocation::Error { row: right, .. }) => {
            left == right
        }
        _ => false,
    }
}

fn cell_offset(location: &DiffLocation) -> usize {
    match location {
        DiffLocation::Header { cell_offset, .. }
        | DiffLocation::HunkHeader { cell_offset, .. }
        | DiffLocation::SourceLine { cell_offset, .. }
        | DiffLocation::Error { cell_offset, .. } => *cell_offset,
    }
}

#[derive(Clone, Copy)]
enum LogicalLocation {
    Header { row: usize },
    HunkHeader { hunk: usize },
    SourceLine { hunk: usize, line: usize },
    Error { row: usize },
}

impl LogicalLocation {
    fn with_cell_offset(self, cell_offset: usize) -> DiffLocation {
        match self {
            Self::Header { row } => DiffLocation::Header { row, cell_offset },
            Self::HunkHeader { hunk } => DiffLocation::HunkHeader { hunk, cell_offset },
            Self::SourceLine { hunk, line } => DiffLocation::SourceLine {
                hunk,
                line,
                cell_offset,
            },
            Self::Error { row } => DiffLocation::Error { row, cell_offset },
        }
    }

    fn matches(self, location: &DiffLocation) -> bool {
        matches!(
            (self, location),
            (Self::Header { row: left }, DiffLocation::Header { row: right, .. })
                | (Self::HunkHeader { hunk: left }, DiffLocation::HunkHeader { hunk: right, .. })
                | (Self::Error { row: left }, DiffLocation::Error { row: right, .. })
                if left == *right
        ) || matches!(
            (self, location),
            (
                Self::SourceLine {
                    hunk: left_hunk,
                    line: left_line,
                },
                DiffLocation::SourceLine {
                    hunk: right_hunk,
                    line: right_line,
                    ..
                },
            ) if left_hunk == *right_hunk && left_line == *right_line
        )
    }
}

struct LogicalLine<'a> {
    path: &'a Path,
    kind: DiffLineKind,
    old_line: Option<u64>,
    new_line: Option<u64>,
    text: &'a str,
    prefix: String,
    continuation_prefix: String,
    content_width: usize,
    rendered_width: usize,
    location: LogicalLocation,
}

impl LogicalLine<'_> {
    fn matches_anchor(&self, anchor: &DiffAnchor) -> bool {
        self.path == anchor.path && self.location.matches(&anchor.location)
    }

    fn anchor(&self, cell_offset: usize) -> DiffAnchor {
        DiffAnchor {
            path: self.path.to_path_buf(),
            location: self.location.with_cell_offset(cell_offset),
        }
    }

    fn render_segment(&self, segment: WrappedSegment) -> RenderedDiffLine {
        let continuation = segment.segment_index > 0;
        let prefix = if continuation {
            &self.continuation_prefix
        } else {
            &self.prefix
        };
        let text = if display_width(prefix) >= self.rendered_width {
            prefix_to_width(prefix, self.rendered_width)
        } else {
            format!("{prefix}{}", segment.text)
        };
        RenderedDiffLine {
            kind: self.kind,
            old_line: (!continuation).then_some(self.old_line).flatten(),
            new_line: (!continuation).then_some(self.new_line).flatten(),
            text,
            continuation,
            anchor: self.anchor(segment.cell_offset),
        }
    }
}

#[derive(Clone, Copy)]
struct LayoutMeasurement {
    total_rows: usize,
    anchor_row: usize,
}

trait DiffLayout {
    fn visit(&self, visitor: &mut dyn FnMut(LogicalLine<'_>) -> bool);

    fn measure(&self, anchor: Option<&DiffAnchor>) -> LayoutMeasurement {
        let mut total_rows = 0usize;
        let mut anchor_row = 0;
        let mut found_anchor = false;
        self.visit(&mut |line| {
            let matching_cell = anchor
                .filter(|anchor| line.matches_anchor(anchor))
                .map(|anchor| cell_offset(&anchor.location));
            let wrapped = measure_wrapped(line.text, line.content_width, matching_cell);
            if matching_cell.is_some() && !found_anchor {
                anchor_row = total_rows.saturating_add(wrapped.anchor_segment);
                found_anchor = true;
            }
            total_rows = total_rows.saturating_add(wrapped.segment_count);
            true
        });
        LayoutMeasurement {
            total_rows,
            anchor_row,
        }
    }

    fn anchor_at(&self, row: usize) -> Option<DiffAnchor> {
        let mut current_row = 0usize;
        let mut result = None;
        self.visit(&mut |line| {
            let segment_count = measure_wrapped(line.text, line.content_width, None).segment_count;
            let next_row = current_row.saturating_add(segment_count);
            if row < next_row {
                let segment_index = row.saturating_sub(current_row);
                result = Some(line.anchor(wrapped_segment_start(
                    line.text,
                    line.content_width,
                    segment_index,
                )));
                return false;
            }
            current_row = next_row;
            true
        });
        result
    }

    fn materialize(&self, range: Range<usize>) -> Vec<RenderedDiffLine> {
        let mut rows = Vec::with_capacity(range.end.saturating_sub(range.start));
        let mut current_row = 0usize;
        self.visit(&mut |line| {
            let segment_count = measure_wrapped(line.text, line.content_width, None).segment_count;
            let next_row = current_row.saturating_add(segment_count);
            if range.start < next_row && current_row < range.end {
                let local_start = range.start.saturating_sub(current_row);
                let local_end = range.end.min(next_row).saturating_sub(current_row);
                rows.extend(
                    wrapped_segments(line.text, line.content_width, local_start..local_end)
                        .into_iter()
                        .map(|segment| line.render_segment(segment)),
                );
            }
            current_row = next_row;
            current_row < range.end
        });
        rows
    }
}

struct ReviewLayout<'a> {
    review: &'a FileChangeReview,
    width: usize,
    gutter: Gutter,
}

impl<'a> ReviewLayout<'a> {
    fn new(review: &'a FileChangeReview, width: usize) -> Self {
        let width = width.max(1);
        Self {
            review,
            width,
            gutter: Gutter::for_review(review, width),
        }
    }

    fn plain_line<'b>(
        &'b self,
        kind: DiffLineKind,
        text: &'b str,
        location: LogicalLocation,
    ) -> LogicalLine<'b> {
        LogicalLine {
            path: &self.review.path,
            kind,
            old_line: None,
            new_line: None,
            text,
            prefix: String::new(),
            continuation_prefix: String::new(),
            content_width: self.width,
            rendered_width: self.width,
            location,
        }
    }
}

impl DiffLayout for ReviewLayout<'_> {
    fn visit(&self, visitor: &mut dyn FnMut(LogicalLine<'_>) -> bool) {
        let header_lines = [
            format!(
                "{} {}",
                action_label(self.review.action),
                self.review.path.display()
            ),
            format!(
                "+{} -{}  {} -> {} bytes",
                added_lines(self.review),
                removed_lines(self.review),
                self.review.before.byte_len,
                self.review.after.byte_len
            ),
            format!(
                "EOF newline: {} -> {}",
                yes_no(self.review.before.ends_with_newline),
                yes_no(self.review.after.ends_with_newline)
            ),
        ];
        for (row, text) in header_lines.iter().enumerate() {
            if !visitor(self.plain_line(
                DiffLineKind::Header,
                text,
                LogicalLocation::Header { row },
            )) {
                return;
            }
        }
        for (hunk_index, hunk) in self.review.hunks.iter().enumerate() {
            let hunk_header = format!(
                "@@ -{},{} +{},{} @@",
                hunk.old_start, hunk.old_len, hunk.new_start, hunk.new_len
            );
            if !visitor(self.plain_line(
                DiffLineKind::Hunk,
                &hunk_header,
                LogicalLocation::HunkHeader { hunk: hunk_index },
            )) {
                return;
            }
            for (line_index, line) in hunk.lines.iter().enumerate() {
                let kind = match line.kind {
                    FileChangeLineKind::Context => DiffLineKind::Context,
                    FileChangeLineKind::Added => DiffLineKind::Added,
                    FileChangeLineKind::Removed => DiffLineKind::Removed,
                };
                let sigil = match kind {
                    DiffLineKind::Added => '+',
                    DiffLineKind::Removed => '-',
                    _ => ' ',
                };
                let prefix = self
                    .gutter
                    .prefix(line.old_line, line.new_line, sigil, false);
                let content_width = self.width.saturating_sub(display_width(&prefix)).max(1);
                if !visitor(LogicalLine {
                    path: &self.review.path,
                    kind,
                    old_line: line.old_line,
                    new_line: line.new_line,
                    text: &line.text,
                    continuation_prefix: self.gutter.prefix(None, None, sigil, true),
                    prefix,
                    content_width,
                    rendered_width: self.width,
                    location: LogicalLocation::SourceLine {
                        hunk: hunk_index,
                        line: line_index,
                    },
                }) {
                    return;
                }
            }
        }
    }
}

struct ErrorLayout<'a> {
    title: &'a str,
    message: &'a str,
    width: usize,
    path: PathBuf,
}

impl<'a> ErrorLayout<'a> {
    fn new(title: &'a str, message: &'a str, width: usize) -> Self {
        Self {
            title,
            message,
            width: width.max(1),
            path: PathBuf::new(),
        }
    }
}

impl DiffLayout for ErrorLayout<'_> {
    fn visit(&self, visitor: &mut dyn FnMut(LogicalLine<'_>) -> bool) {
        for (row, text) in [self.title, self.message].into_iter().enumerate() {
            if !visitor(LogicalLine {
                path: &self.path,
                kind: DiffLineKind::Error,
                old_line: None,
                new_line: None,
                text,
                prefix: String::new(),
                continuation_prefix: String::new(),
                content_width: self.width,
                rendered_width: self.width,
                location: LogicalLocation::Error { row },
            }) {
                return;
            }
        }
    }
}

fn added_lines(review: &FileChangeReview) -> u64 {
    review
        .hunks
        .iter()
        .flat_map(|hunk| &hunk.lines)
        .filter(|line| line.kind == FileChangeLineKind::Added)
        .count() as u64
}

fn removed_lines(review: &FileChangeReview) -> u64 {
    review
        .hunks
        .iter()
        .flat_map(|hunk| &hunk.lines)
        .filter(|line| line.kind == FileChangeLineKind::Removed)
        .count() as u64
}

fn action_label(action: FileChangeAction) -> &'static str {
    match action {
        FileChangeAction::Create => "Create",
        FileChangeAction::Overwrite => "Overwrite",
        FileChangeAction::Edit => "Edit",
        FileChangeAction::ReplaceAll => "Replace all",
        FileChangeAction::Delete => "Delete",
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

#[derive(Clone, Copy)]
enum Gutter {
    Both { old: usize, new: usize },
    One { digits: usize },
    Sigil,
}

impl Gutter {
    fn for_review(review: &FileChangeReview, width: usize) -> Self {
        let old_digits = decimal_digits(
            review
                .hunks
                .iter()
                .map(|hunk| hunk.old_start.saturating_add(hunk.old_len))
                .max()
                .unwrap_or(0),
        );
        let new_digits = decimal_digits(
            review
                .hunks
                .iter()
                .map(|hunk| hunk.new_start.saturating_add(hunk.new_len))
                .max()
                .unwrap_or(0),
        );
        let both_width = old_digits + new_digits + 4;
        if width > both_width {
            Self::Both {
                old: old_digits,
                new: new_digits,
            }
        } else {
            let digits = old_digits.max(new_digits);
            if width > digits + 3 {
                Self::One { digits }
            } else {
                Self::Sigil
            }
        }
    }

    fn prefix(
        self,
        old_line: Option<u64>,
        new_line: Option<u64>,
        sigil: char,
        continuation: bool,
    ) -> String {
        match self {
            Self::Both { old, new } => format!(
                "{} {} {sigil} ",
                number(old_line.filter(|_| !continuation), old),
                number(new_line.filter(|_| !continuation), new)
            ),
            Self::One { digits } => {
                let line = if continuation {
                    None
                } else {
                    match sigil {
                        '+' => new_line,
                        '-' => old_line,
                        _ => new_line.or(old_line),
                    }
                };
                format!("{} {sigil} ", number(line, digits))
            }
            Self::Sigil => sigil.to_string(),
        }
    }
}

fn number(value: Option<u64>, width: usize) -> String {
    value.map_or_else(|| " ".repeat(width), |value| format!("{value:>width$}"))
}

fn decimal_digits(value: u64) -> usize {
    value.max(1).ilog10() as usize + 1
}

#[derive(Debug)]
struct WrappedSegment {
    segment_index: usize,
    cell_offset: usize,
    text: String,
}

#[derive(Clone, Copy)]
struct WrappedMeasurement {
    segment_count: usize,
    anchor_segment: usize,
}

fn measure_wrapped(text: &str, width: usize, anchor_cell: Option<usize>) -> WrappedMeasurement {
    let width = width.max(1);
    let mut output_width = 0usize;
    let mut logical_cell = 0usize;
    let mut segment_count = 1usize;
    let mut anchor_segment = 0;
    visit_wrappable_characters(text, width, &mut |_, character_width| {
        if output_width > 0 && output_width.saturating_add(character_width) > width {
            output_width = 0;
            segment_count = segment_count.saturating_add(1);
            if anchor_cell.is_some_and(|anchor| logical_cell <= anchor) {
                anchor_segment = segment_count.saturating_sub(1);
            }
        }
        output_width = output_width.saturating_add(character_width);
        logical_cell = logical_cell.saturating_add(character_width);
        true
    });
    WrappedMeasurement {
        segment_count,
        anchor_segment,
    }
}

fn wrapped_segment_start(text: &str, width: usize, target_segment: usize) -> usize {
    if target_segment == 0 {
        return 0;
    }
    let width = width.max(1);
    let mut output_width = 0usize;
    let mut logical_cell = 0usize;
    let mut segment_index = 0usize;
    let mut result = 0usize;
    visit_wrappable_characters(text, width, &mut |_, character_width| {
        if output_width > 0 && output_width.saturating_add(character_width) > width {
            output_width = 0;
            segment_index = segment_index.saturating_add(1);
            if segment_index == target_segment {
                result = logical_cell;
                return false;
            }
        }
        output_width = output_width.saturating_add(character_width);
        logical_cell = logical_cell.saturating_add(character_width);
        true
    });
    result
}

fn wrapped_segments(text: &str, width: usize, range: Range<usize>) -> Vec<WrappedSegment> {
    if range.is_empty() {
        return Vec::new();
    }
    let width = width.max(1);
    let mut segments = Vec::with_capacity(range.end.saturating_sub(range.start));
    let mut output = String::new();
    let mut output_width = 0usize;
    let mut logical_cell = 0usize;
    let mut start_cell = 0usize;
    let mut segment_index = 0usize;
    visit_wrappable_characters(text, width, &mut |character, character_width| {
        if output_width > 0 && output_width.saturating_add(character_width) > width {
            if range.contains(&segment_index) {
                segments.push(WrappedSegment {
                    segment_index,
                    cell_offset: start_cell,
                    text: std::mem::take(&mut output),
                });
            }
            segment_index = segment_index.saturating_add(1);
            start_cell = logical_cell;
            output_width = 0;
            if segment_index >= range.end {
                return false;
            }
        }
        if range.contains(&segment_index) {
            output.push(character);
        }
        output_width = output_width.saturating_add(character_width);
        logical_cell = logical_cell.saturating_add(character_width);
        true
    });
    if range.contains(&segment_index) {
        segments.push(WrappedSegment {
            segment_index,
            cell_offset: start_cell,
            text: output,
        });
    }
    segments
}

fn visit_wrappable_characters(
    text: &str,
    width: usize,
    visitor: &mut dyn FnMut(char, usize) -> bool,
) -> bool {
    let mut consecutive_zero_width_characters = 0;
    visit_visible_characters(text, &mut |character| {
        let character_width = character.width().unwrap_or(0);
        let exceeds_zero_width_limit = if character_width == 0 {
            consecutive_zero_width_characters += 1;
            consecutive_zero_width_characters > MAX_CONSECUTIVE_ZERO_WIDTH_CHARACTERS
        } else {
            consecutive_zero_width_characters = 0;
            false
        };
        if character_width <= width && !exceeds_zero_width_limit {
            return visitor(character, character_width);
        }
        for escaped in character.escape_unicode() {
            if !visitor(escaped, escaped.width().unwrap_or(0)) {
                return false;
            }
        }
        true
    })
}

fn visit_visible_characters(text: &str, visitor: &mut dyn FnMut(char) -> bool) -> bool {
    let mut column = 0;
    for character in text.chars() {
        if character == '\t' {
            let spaces = TAB_COLUMNS - column % TAB_COLUMNS;
            for _ in 0..spaces {
                if !visitor(' ') {
                    return false;
                }
            }
            column += spaces;
        } else if let Some(control_picture) = control_picture(character) {
            if !visitor(control_picture) {
                return false;
            }
            column += control_picture.width().unwrap_or(1);
        } else {
            if !visitor(character) {
                return false;
            }
            column += character.width().unwrap_or(0);
        }
    }
    true
}

fn control_picture(character: char) -> Option<char> {
    match character as u32 {
        0x00..=0x1f => char::from_u32(0x2400 + character as u32),
        0x7f => Some('␡'),
        0x80..=0x9f => Some('�'),
        _ if character.is_control() => Some('�'),
        _ => None,
    }
}

fn prefix_to_width(prefix: &str, width: usize) -> String {
    let mut output = String::new();
    let mut cells = 0;
    for character in prefix.chars() {
        let character_width = character.width().unwrap_or(0);
        if cells + character_width > width {
            break;
        }
        output.push(character);
        cells += character_width;
    }
    output
}

fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, time::Duration};

    use super::*;
    use crate::file_change::{FileChangeHunk, FileChangeLine, FileVersion};

    fn version(bytes: u64, lines: u64, newline: bool) -> FileVersion {
        FileVersion {
            exists: bytes > 0 || lines > 0,
            identity: None,
            byte_len: bytes,
            line_count: lines,
            ends_with_newline: newline,
            sha256: format!("hash-{bytes}-{lines}-{newline}"),
        }
    }

    fn line(
        kind: FileChangeLineKind,
        old_line: Option<u64>,
        new_line: Option<u64>,
        text: &str,
    ) -> FileChangeLine {
        FileChangeLine {
            kind,
            old_line,
            new_line,
            text: text.into(),
            has_newline: true,
        }
    }

    fn review(lines: Vec<FileChangeLine>) -> FileChangeReview {
        FileChangeReview {
            version: 1,
            action: FileChangeAction::Edit,
            path: PathBuf::from("src/猫.rs"),
            before: version(120, 12, true),
            after: version(140, 13, false),
            hunks: vec![FileChangeHunk {
                old_start: 8,
                old_len: 2,
                new_start: 8,
                new_len: 3,
                lines,
            }],
        }
    }

    #[test]
    fn renders_stable_headers_stats_hunks_and_line_numbers() {
        let review = review(vec![
            line(FileChangeLineKind::Context, Some(8), Some(8), "same"),
            line(FileChangeLineKind::Removed, Some(9), None, "old"),
            line(FileChangeLineKind::Added, None, Some(9), "new"),
            line(FileChangeLineKind::Added, None, Some(10), "more"),
        ]);
        let frame = render_diff(
            &review,
            &DiffNavigation::default(),
            DiffViewport {
                width: 80,
                height: 20,
            },
        );

        assert_eq!(frame.lines[0].text, "Edit src/猫.rs");
        assert_eq!(frame.lines[1].text, "+2 -1  120 -> 140 bytes");
        assert_eq!(frame.lines[2].text, "EOF newline: yes -> no");
        assert_eq!(frame.lines[3].text, "@@ -8,2 +8,3 @@");
        assert_eq!(frame.lines[4].old_line, Some(8));
        assert_eq!(frame.lines[4].new_line, Some(8));
        assert!(frame.lines[5].text.contains("- old"));
        assert!(frame.lines[6].text.contains("+ new"));
    }

    #[test]
    fn renders_delete_as_a_stable_action_instead_of_an_edit() {
        let mut review = review(vec![line(
            FileChangeLineKind::Removed,
            Some(8),
            None,
            "deleted",
        )]);
        review.action = FileChangeAction::Delete;
        review.after = version(0, 0, false);
        review.after.exists = false;
        let frame = render_diff(
            &review,
            &DiffNavigation::default(),
            DiffViewport {
                width: 40,
                height: 10,
            },
        );

        assert_eq!(frame.lines[0].text, "Delete src/猫.rs");
        assert!(
            frame
                .lines
                .iter()
                .any(|line| line.kind == DiffLineKind::Removed)
        );
    }

    #[test]
    fn wraps_unicode_tabs_and_control_characters_without_terminal_controls() {
        let review = review(vec![line(
            FileChangeLineKind::Added,
            None,
            Some(8),
            "中\t文\u{1b}[31m marker-marker-marker",
        )]);
        let frame = render_diff(
            &review,
            &DiffNavigation::default(),
            DiffViewport {
                width: 12,
                height: 100,
            },
        );
        let added: Vec<_> = frame
            .lines
            .iter()
            .filter(|line| line.kind == DiffLineKind::Added)
            .collect();

        assert!(added.len() > 1);
        assert!(added.iter().all(|line| display_width(&line.text) <= 12));
        assert!(
            added
                .iter()
                .all(|line| !line.text.chars().any(char::is_control))
        );
        assert!(added.iter().all(|line| line.text.contains('+')));
        assert_eq!(added[0].new_line, Some(8));
        assert!(added.iter().skip(1).all(|line| line.new_line.is_none()));
        assert!(
            added
                .iter()
                .map(|line| line.text.as_str())
                .any(|text| text.contains('␛'))
        );
    }

    #[test]
    fn multiple_hunks_and_eof_state_are_explicit() {
        let mut review = review(vec![line(
            FileChangeLineKind::Removed,
            Some(8),
            None,
            "first",
        )]);
        review.hunks.push(FileChangeHunk {
            old_start: 100,
            old_len: 1,
            new_start: 101,
            new_len: 1,
            lines: vec![line(FileChangeLineKind::Added, None, Some(101), "last")],
        });
        let frame = render_diff(
            &review,
            &DiffNavigation::default(),
            DiffViewport {
                width: 40,
                height: 30,
            },
        );

        assert_eq!(
            frame
                .lines
                .iter()
                .filter(|line| line.kind == DiffLineKind::Hunk)
                .count(),
            2
        );
        assert!(frame.lines.iter().any(|line| line.text.contains("+101,1")));
        assert!(
            frame
                .lines
                .iter()
                .any(|line| line.text.contains("yes -> no"))
        );
    }

    #[test]
    fn narrow_layout_drops_numbers_before_sigils_and_keeps_every_changed_line() {
        let review = review(
            (0..20)
                .map(|index| {
                    line(
                        FileChangeLineKind::Added,
                        None,
                        Some(index + 1),
                        if index == 19 { "终点" } else { "x" },
                    )
                })
                .collect(),
        );
        let viewport = DiffViewport {
            width: 2,
            height: 3,
        };
        let navigation = navigate_diff(
            &review,
            &DiffNavigation::default(),
            viewport,
            DiffNavigationAction::End,
        );
        let frame = render_diff(&review, &navigation, viewport);

        assert!(!frame.can_move_down);
        assert!(
            frame
                .lines
                .iter()
                .all(|line| display_width(&line.text) <= 2)
        );
        assert!(
            frame
                .lines
                .iter()
                .filter(|line| line.kind == DiffLineKind::Added)
                .all(|line| line.text.starts_with('+'))
        );
        assert!(frame.lines.iter().any(|line| {
            matches!(
                line.anchor.location,
                DiffLocation::SourceLine { line: 19, .. }
            )
        }));
    }

    #[test]
    fn all_navigation_actions_reach_the_expected_boundaries() {
        let review = review(
            (0..30)
                .map(|index| line(FileChangeLineKind::Added, None, Some(index + 1), "changed"))
                .collect(),
        );
        let viewport = DiffViewport {
            width: 20,
            height: 4,
        };
        let start = DiffNavigation::default();
        let down = navigate_diff(&review, &start, viewport, DiffNavigationAction::Down);
        assert_eq!(render_diff(&review, &down, viewport).top_row, 1);
        let page = navigate_diff(&review, &down, viewport, DiffNavigationAction::PageDown);
        assert_eq!(render_diff(&review, &page, viewport).top_row, 5);
        let up = navigate_diff(&review, &page, viewport, DiffNavigationAction::Up);
        assert_eq!(render_diff(&review, &up, viewport).top_row, 4);
        let page_up = navigate_diff(&review, &up, viewport, DiffNavigationAction::PageUp);
        assert_eq!(render_diff(&review, &page_up, viewport).top_row, 0);
        let end = navigate_diff(&review, &page, viewport, DiffNavigationAction::End);
        assert!(!render_diff(&review, &end, viewport).can_move_down);
        let home = navigate_diff(&review, &end, viewport, DiffNavigationAction::Home);
        assert!(!render_diff(&review, &home, viewport).can_move_up);
    }

    #[test]
    fn resize_keeps_the_same_source_line_and_nearest_cell_offset() {
        let review = review(vec![line(
            FileChangeLineKind::Added,
            None,
            Some(8),
            "alpha-中文-beta-gamma-delta-epsilon",
        )]);
        let narrow = DiffViewport {
            width: 11,
            height: 2,
        };
        let end = navigate_diff(
            &review,
            &DiffNavigation::default(),
            narrow,
            DiffNavigationAction::End,
        );
        let before = render_diff(&review, &end, narrow);
        let source_anchor = before
            .lines
            .iter()
            .find(|line| line.kind == DiffLineKind::Added)
            .unwrap()
            .anchor
            .clone();
        let resized = render_diff(
            &review,
            &DiffNavigation {
                anchor: Some(source_anchor.clone()),
                stick_to_end: false,
            },
            DiffViewport {
                width: 15,
                height: 2,
            },
        );
        let after = resized
            .lines
            .iter()
            .find(|line| line.kind == DiffLineKind::Added)
            .unwrap();

        assert!(same_logical_row(&source_anchor, &after.anchor));
        assert!(
            cell_offset(&source_anchor.location).abs_diff(cell_offset(&after.anchor.location))
                <= 10
        );
    }

    #[test]
    fn end_navigation_remains_at_the_end_after_narrowing_the_viewport() {
        let review = review(
            (0..40)
                .map(|index| {
                    line(
                        FileChangeLineKind::Added,
                        None,
                        Some(index + 1),
                        if index == 39 {
                            "TAIL-MARKER-at-the-physical-end"
                        } else {
                            "ordinary changed line"
                        },
                    )
                })
                .collect(),
        );
        let wide = DiffViewport {
            width: 140,
            height: 40,
        };
        let end = navigate_diff(
            &review,
            &DiffNavigation::default(),
            wide,
            DiffNavigationAction::End,
        );
        assert!(end.stick_to_end);

        let narrow = render_diff(
            &review,
            &end,
            DiffViewport {
                width: 12,
                height: 12,
            },
        );

        assert!(!narrow.can_move_down);
        assert!(narrow.navigation.stick_to_end);
        assert!(narrow.lines.iter().any(|line| {
            matches!(
                line.anchor.location,
                DiffLocation::SourceLine { line: 39, .. }
            )
        }));
    }

    #[test]
    fn empty_change_and_zero_sized_viewports_are_total() {
        let mut review = review(Vec::new());
        review.hunks.clear();
        review.before = version(0, 0, false);
        review.after = version(0, 0, false);
        let zero = render_diff(
            &review,
            &DiffNavigation::default(),
            DiffViewport {
                width: 0,
                height: 0,
            },
        );
        let visible = render_diff(
            &review,
            &DiffNavigation::default(),
            DiffViewport {
                width: 20,
                height: 10,
            },
        );

        assert!(zero.lines.is_empty());
        assert_eq!(zero.total_rows, 0);
        assert!(visible.lines.iter().any(|line| line.text.contains("+0 -0")));
    }

    #[test]
    fn typed_error_is_wrapped_navigable_and_control_safe() {
        let viewport = DiffViewport {
            width: 8,
            height: 2,
        };
        let end = navigate_error(
            "Review unavailable",
            "artifact\u{1b} missing after restart",
            &DiffNavigation::default(),
            viewport,
            DiffNavigationAction::End,
        );
        let frame = render_error(
            "Review unavailable",
            "artifact\u{1b} missing after restart",
            &end,
            viewport,
        );

        assert!(!frame.can_move_down);
        assert!(
            frame
                .lines
                .iter()
                .all(|line| line.kind == DiffLineKind::Error)
        );
        assert!(
            frame
                .lines
                .iter()
                .all(|line| display_width(&line.text) <= 8)
        );
        assert!(
            frame
                .lines
                .iter()
                .all(|line| !line.text.chars().any(char::is_control))
        );
    }

    #[test]
    fn multi_megabyte_line_materializes_only_the_viewport_and_navigates_quickly() {
        const LONG_LINE_BYTES: usize = 8 * 1024 * 1024;
        let mut text = "a".repeat(LONG_LINE_BYTES);
        text.push_str("\t中\u{1b}Z");
        let review = review(vec![line(FileChangeLineKind::Added, None, Some(8), &text)]);
        let viewport = DiffViewport {
            width: 2,
            height: 3,
        };

        // CPU time of this thread, so parallel tests on a loaded machine never stretch the bound.
        let thread_cpu_time = || {
            let mut time = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            assert_eq!(
                unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) },
                0
            );
            Duration::new(time.tv_sec as u64, time.tv_nsec as u32)
        };
        let started = thread_cpu_time();
        let end = navigate_diff(
            &review,
            &DiffNavigation::default(),
            viewport,
            DiffNavigationAction::End,
        );
        let end_frame = render_diff(&review, &end, viewport);
        let up = navigate_diff(&review, &end, viewport, DiffNavigationAction::Up);
        let up_frame = render_diff(&review, &up, viewport);
        let resized = render_diff(
            &review,
            &up,
            DiffViewport {
                width: 3,
                height: 3,
            },
        );

        assert!(thread_cpu_time() - started < Duration::from_secs(10));
        assert_eq!(end_frame.lines.len(), viewport.height);
        assert!(end_frame.total_rows > LONG_LINE_BYTES);
        assert!(!end_frame.can_move_down);
        assert!(end_frame.navigation.stick_to_end);
        assert!(end_frame.lines.iter().any(|line| line.text == "+Z"));
        assert!(
            end_frame
                .lines
                .iter()
                .all(|line| !line.text.chars().any(char::is_control))
        );
        assert_eq!(up_frame.lines.len(), viewport.height);
        assert!(up_frame.can_move_down);
        assert!(!up_frame.navigation.stick_to_end);
        assert_eq!(resized.lines.len(), viewport.height);
        assert!(same_logical_row(
            up.anchor.as_ref().unwrap(),
            resized.navigation.anchor.as_ref().unwrap()
        ));
    }

    #[test]
    fn layout_window_allocation_is_bounded_by_the_requested_range() {
        let review = review(vec![line(
            FileChangeLineKind::Added,
            None,
            Some(8),
            &"a".repeat(64 * 1024),
        )]);
        let layout = ReviewLayout::new(&review, 2);
        let measurement = layout.measure(None);
        let window_start = measurement.total_rows.saturating_sub(3);
        let lines = layout.materialize(window_start..measurement.total_rows);

        assert!(measurement.total_rows > 60_000);
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn one_and_two_column_viewports_preserve_changed_row_reachability() {
        let review = review(vec![line(FileChangeLineKind::Added, None, Some(8), "abcd")]);
        let one_column = DiffViewport {
            width: 1,
            height: 4,
        };
        let one_column_end = navigate_diff(
            &review,
            &DiffNavigation::default(),
            one_column,
            DiffNavigationAction::End,
        );
        let one_column_frame = render_diff(&review, &one_column_end, one_column);
        let two_column = DiffViewport {
            width: 2,
            height: 4,
        };
        let two_column_end = navigate_diff(
            &review,
            &DiffNavigation::default(),
            two_column,
            DiffNavigationAction::End,
        );
        let two_column_frame = render_diff(&review, &two_column_end, two_column);

        assert_eq!(one_column_frame.lines.len(), 4);
        assert!(one_column_frame.lines.iter().all(|line| line.text == "+"));
        assert_eq!(
            one_column_frame
                .lines
                .iter()
                .map(|line| cell_offset(&line.anchor.location))
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        assert_eq!(
            two_column_frame
                .lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            vec!["+a", "+b", "+c", "+d"]
        );
        assert!(!one_column_frame.can_move_down);
        assert!(!two_column_frame.can_move_down);
    }

    #[test]
    fn pathological_zero_width_sequences_cannot_create_an_unbounded_rendered_line() {
        let review = review(vec![line(
            FileChangeLineKind::Added,
            None,
            Some(8),
            &"\u{301}".repeat(1024),
        )]);
        let viewport = DiffViewport {
            width: 2,
            height: 1,
        };
        let source_start = DiffNavigation {
            anchor: Some(DiffAnchor {
                path: review.path.clone(),
                location: DiffLocation::SourceLine {
                    hunk: 0,
                    line: 0,
                    cell_offset: 0,
                },
            }),
            stick_to_end: false,
        };
        let first_source_row = render_diff(&review, &source_start, viewport);
        let end = navigate_diff(&review, &source_start, viewport, DiffNavigationAction::End);
        let last_source_row = render_diff(&review, &end, viewport);

        assert!(first_source_row.total_rows > 7_000);
        assert!(first_source_row.lines[0].text.len() < 64);
        assert_eq!(last_source_row.lines.len(), 1);
        assert!(!last_source_row.can_move_down);
    }
}
