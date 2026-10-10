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
    shimmer: RunningShimmer,
}

impl ThinkingPreview {
    pub(super) fn append(&mut self, delta: &str) {
        for grapheme in delta.graphemes(true) {
            if grapheme.contains('\n') {
                self.line.clear();
                self.line_characters = 0;
                self.shimmer.reset();
            } else if grapheme == "\t" {
                self.push_grapheme(" ");
            } else if !grapheme.chars().any(char::is_control) {
                self.push_grapheme(grapheme);
            }
        }
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

/// Measures only as far as the row reaches, so a long line costs no more than a short one.
fn truncate_to_width(text: &str, width: usize) -> String {
    let available = width.saturating_sub(1);
    let mut used = 0;
    let mut kept = 0;
    for (offset, grapheme) in text.grapheme_indices(true) {
        used += grapheme.width();
        if used > width {
            return format!("{}…", &text[..kept]);
        }
        if used <= available {
            kept = offset + grapheme.len();
        }
    }
    text.to_owned()
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
        preview.append("First line\nSec");
        preview.append("ond line\r\n  第三行");
        preview.append("继续");
        assert_eq!(text(&preview.render(80, true)), "• 第三行继续");
    }

    #[test]
    fn preview_stays_one_row_and_truncates_at_a_grapheme_boundary() {
        let mut preview = ThinkingPreview::default();
        preview.append("价格🙂继续比较这些数据");
        let rendered = text(&preview.render(14, true));
        assert!(!rendered.contains('\n'));
        assert!(rendered.width() <= 14);
        assert!(rendered.ends_with('…'));
        assert!(rendered.contains('🙂'));
    }

    #[test]
    fn appends_received_graphemes_together() {
        let mut preview = ThinkingPreview::default();
        preview.append("你🙂a");
        assert_eq!(text(&preview.render(80, true)), "• 你🙂a");
        preview.append("\n好");
        assert_eq!(text(&preview.render(80, true)), "• 好");
    }
}
