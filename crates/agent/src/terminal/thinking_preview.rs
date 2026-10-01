use std::collections::VecDeque;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::{
    running_shimmer::RunningShimmer,
    style::{Role, RoleLine},
};

const PREFIX: &str = "• ";
const MAX_LINE_CHARACTERS: usize = 4096;

#[derive(Default)]
pub(super) struct ThinkingPreview {
    line: String,
    line_characters: usize,
    pending: VecDeque<String>,
    shimmer: RunningShimmer,
}

impl ThinkingPreview {
    pub(super) fn enqueue(&mut self, delta: &str) {
        self.pending
            .extend(delta.graphemes(true).map(str::to_owned));
    }

    pub(super) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    pub(super) fn advance(&mut self) -> bool {
        let Some(grapheme) = self.pending.pop_front() else {
            return false;
        };
        if grapheme.contains('\n') {
            self.line.clear();
            self.line_characters = 0;
            self.shimmer.reset();
        } else if grapheme == "\t" {
            self.push_grapheme(" ");
        } else if !grapheme.chars().any(char::is_control) {
            self.push_grapheme(&grapheme);
        }
        true
    }

    fn push_grapheme(&mut self, grapheme: &str) {
        if self.line_characters < MAX_LINE_CHARACTERS {
            self.line.push_str(grapheme);
            self.line_characters += 1;
        }
    }

    pub(super) fn spans(
        &self,
        width: usize,
        prefers_reduced_motion: bool,
    ) -> Vec<(String, Option<Role>)> {
        self.render(width, prefers_reduced_motion)
    }

    fn render(&self, width: usize, prefers_reduced_motion: bool) -> Vec<(String, Option<Role>)> {
        if width <= PREFIX.width() {
            return vec![("•".into(), Some(Role::MutedText))];
        }
        let text = if self.line.trim().is_empty() {
            "Thinking…"
        } else {
            self.line.trim_start()
        };
        let visible = truncate_to_width(text, width - PREFIX.width());
        let mut row = RoleLine::default();
        row.push(PREFIX, Some(Role::MutedText));

        self.shimmer
            .append(&mut row, &visible, prefers_reduced_motion);
        row.spans
            .into_iter()
            .map(|span| (span.text, span.role))
            .collect()
    }
}

fn truncate_to_width(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    let available = width.saturating_sub(1);
    let mut visible = String::new();
    let mut used = 0;
    for grapheme in text.graphemes(true) {
        let glyph_width = grapheme.width();
        if used + glyph_width > available {
            break;
        }
        visible.push_str(grapheme);
        used += glyph_width;
    }
    visible.push('…');
    visible
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(spans: &[(String, Option<Role>)]) -> String {
        spans.iter().map(|(text, _)| text.as_str()).collect()
    }

    #[test]
    fn newest_thinking_line_replaces_the_previous_line_across_deltas() {
        let mut preview = ThinkingPreview::default();
        preview.enqueue("First line\nSec");
        preview.enqueue("ond line\r\n  第三行");
        preview.enqueue("继续");
        while preview.advance() {}
        assert_eq!(text(&preview.render(80, true)), "• 第三行继续");
    }

    #[test]
    fn preview_stays_one_row_and_truncates_at_a_grapheme_boundary() {
        let mut preview = ThinkingPreview::default();
        preview.enqueue("价格🙂继续比较这些数据");
        while preview.advance() {}
        let rendered = text(&preview.render(14, true));
        assert!(!rendered.contains('\n'));
        assert!(rendered.width() <= 14);
        assert!(rendered.ends_with('…'));
        assert!(rendered.contains('🙂'));
    }

    #[test]
    fn advances_one_grapheme_per_frame() {
        let mut preview = ThinkingPreview::default();
        preview.enqueue("你🙂a\n好");
        assert!(preview.advance());
        assert_eq!(text(&preview.render(80, true)), "• 你");
        assert!(preview.advance());
        assert_eq!(text(&preview.render(80, true)), "• 你🙂");
        assert!(preview.advance());
        assert_eq!(text(&preview.render(80, true)), "• 你🙂a");
        assert!(preview.advance());
        assert_eq!(text(&preview.render(80, true)), "• Thinking…");
        assert!(preview.advance());
        assert_eq!(text(&preview.render(80, true)), "• 好");
        assert!(!preview.has_pending());
    }
}
