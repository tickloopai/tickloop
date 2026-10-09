use std::{fmt, ops::Range};
use tickloop_agent_protocol::{Image, ImageContext, ImageOrigin};

pub(super) const LARGE_PASTE_CHARACTER_THRESHOLD: usize = 800;
pub(super) const LARGE_PASTE_LINE_THRESHOLD: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct PasteBlockId(u64);

impl PasteBlockId {
    pub(super) fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PasteRegistration {
    Inline,
    Folded(PasteBlockId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PasteRestoreIssue {
    OriginalMissing { id: u64 },
    OriginalChanged { id: u64 },
    OverlappingBlock { id: u64 },
}

impl fmt::Display for PasteRestoreIssue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OriginalMissing { id } => {
                write!(
                    formatter,
                    "pasted text #{id} is unavailable in the stored prompt"
                )
            }
            Self::OriginalChanged { id } => {
                write!(
                    formatter,
                    "pasted text #{id} no longer matches the stored prompt"
                )
            }
            Self::OverlappingBlock { id } => {
                write!(formatter, "pasted text #{id} overlaps another pasted block")
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PasteReconcile {
    pub(super) old_changed: Range<usize>,
    pub(super) new_changed: Range<usize>,
    pub(super) unfolded: Vec<PasteBlockId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PasteDeletion {
    pub(super) draft: String,
    pub(super) cursor: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RenderedPasteBlock {
    pub(super) id: PasteBlockId,
    pub(super) raw_range: Range<usize>,
    pub(super) display_range: Range<usize>,
    pub(super) label: String,
    pub(super) is_image: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PasteProjection {
    pub(super) text: String,
    pub(super) cursor: usize,
    pub(super) blocks: Vec<RenderedPasteBlock>,
    pub(super) issues: Vec<PasteRestoreIssue>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PasteBlockError {
    message: String,
}

impl PasteBlockError {
    fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for PasteBlockError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for PasteBlockError {}

/// A folded text block keeps its pasted text in the draft and only shows a label; an
/// image block's draft text is the literal `[Image #N]` placeholder.
#[derive(Clone, Debug, PartialEq, Eq)]
enum PasteContent {
    Text { line_count: usize },
    Image(Image),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PasteBlock {
    id: PasteBlockId,
    range: Range<usize>,
    fingerprint: u64,
    content: PasteContent,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct PasteBlocks {
    next_id: u64,
    blocks: Vec<PasteBlock>,
}

impl PasteBlocks {
    pub(super) fn new() -> Self {
        Self {
            next_id: 1,
            blocks: Vec::new(),
        }
    }

    pub(super) fn is_large_paste(text: &str) -> bool {
        text.chars().count() > LARGE_PASTE_CHARACTER_THRESHOLD
            || logical_line_count(text) > LARGE_PASTE_LINE_THRESHOLD
    }

    pub(super) fn register(
        &mut self,
        draft: &str,
        range: Range<usize>,
    ) -> Result<PasteRegistration, PasteBlockError> {
        let text = checked_slice(draft, &range)?;
        if !Self::is_large_paste(text) {
            return Ok(PasteRegistration::Inline);
        }
        if self
            .blocks
            .iter()
            .any(|block| ranges_overlap(&block.range, &range))
        {
            return Err(PasteBlockError::message(
                "pasted block overlaps an existing pasted block",
            ));
        }

        let id = self.next_id();
        self.reserve_through(id)?;
        self.insert_block(PasteBlock {
            id,
            range,
            fingerprint: fingerprint(text),
            content: PasteContent::Text {
                line_count: logical_line_count(text),
            },
        });
        Ok(PasteRegistration::Folded(id))
    }

    /// The ID the next folded text or image takes; both kinds share one counter.
    pub(super) fn next_id(&self) -> PasteBlockId {
        PasteBlockId(self.next_id.max(1))
    }

    /// The literal draft text of an image block, which is also the label the model sees.
    pub(super) fn image_placeholder(id: PasteBlockId) -> Result<String, PasteBlockError> {
        Ok(ImageOrigin::Pasted { id: image_id(id)? }.label())
    }

    /// Attaches `image` to its placeholder already in the draft at `range`. Later IDs
    /// continue after `id`, so a restored image never shares a number with a new paste.
    pub(super) fn register_image(
        &mut self,
        draft: &str,
        range: Range<usize>,
        id: PasteBlockId,
        image: Image,
    ) -> Result<(), PasteBlockError> {
        let text = checked_slice(draft, &range)?;
        if text != Self::image_placeholder(id)? {
            return Err(PasteBlockError::message(
                "image placeholder is missing from the draft",
            ));
        }
        if self
            .blocks
            .iter()
            .any(|block| block.id == id || ranges_overlap(&block.range, &range))
        {
            return Err(PasteBlockError::message(
                "image placeholder overlaps an existing pasted block",
            ));
        }
        self.reserve_through(id)?;
        self.insert_block(PasteBlock {
            id,
            range,
            fingerprint: fingerprint(text),
            content: PasteContent::Image(image),
        });
        Ok(())
    }

    /// Re-attaches an image saved with a prompt to the first free `[Image #id]` placeholder
    /// in the draft; without one the image stays detached, as if its placeholder was deleted.
    pub(super) fn restore_image(&mut self, draft: &str, id: u32, image: Image) {
        let id = PasteBlockId(u64::from(id));
        let Ok(placeholder) = Self::image_placeholder(id) else {
            return;
        };
        for (start, _) in draft.match_indices(&placeholder) {
            let range = start..start + placeholder.len();
            if self.register_image(draft, range, id, image.clone()).is_ok() {
                return;
            }
        }
    }

    /// Keeps new IDs above a pasted image already in the conversation, so a resumed Chat
    /// never reuses an image number (Claude Code `getInitialPasteId`).
    pub(super) fn continue_after_image(&mut self, id: u32) {
        self.next_id = self.next_id.max(u64::from(id) + 1);
    }

    fn reserve_through(&mut self, id: PasteBlockId) -> Result<(), PasteBlockError> {
        let next =
            id.0.checked_add(1)
                .ok_or_else(|| PasteBlockError::message("pasted block IDs are exhausted"))?;
        self.next_id = self.next_id.max(next);
        Ok(())
    }

    /// Hands over the blocks of a sent or cleared draft; numbering continues where it was.
    pub(super) fn take(&mut self) -> PasteBlocks {
        PasteBlocks {
            next_id: self.next_id,
            blocks: std::mem::take(&mut self.blocks),
        }
    }

    pub(super) fn clear(&mut self) {
        self.blocks.clear();
    }

    /// Puts back blocks handed over by [`Self::take`] without reusing IDs given out since.
    pub(super) fn restore(&mut self, saved: PasteBlocks) {
        self.next_id = self.next_id.max(saved.next_id);
        self.blocks = saved.blocks;
    }

    /// Images whose placeholders are intact, in placeholder order.
    pub(super) fn images(&self, draft: &str) -> Vec<ImageContext> {
        self.blocks
            .iter()
            .filter(|block| {
                valid_slice(draft, &block.range).map(fingerprint) == Some(block.fingerprint)
            })
            .filter_map(|block| match &block.content {
                PasteContent::Image(image) => Some(ImageContext {
                    origin: ImageOrigin::Pasted {
                        id: image_id(block.id).ok()?,
                    },
                    image: image.clone(),
                }),
                PasteContent::Text { .. } => None,
            })
            .collect()
    }

    fn insert_block(&mut self, block: PasteBlock) {
        self.blocks.push(block);
        self.blocks.sort_by_key(|block| block.range.start);
    }

    pub(super) fn reconcile(&mut self, before: &str, after: &str) -> PasteReconcile {
        let (old_changed, new_changed) = changed_ranges(before, after);
        if old_changed.is_empty() && new_changed.is_empty() {
            return PasteReconcile {
                old_changed,
                new_changed,
                unfolded: Vec::new(),
            };
        }

        let old_length = old_changed.len();
        let new_length = new_changed.len();
        let mut unfolded = Vec::new();
        self.blocks.retain_mut(|block| {
            if old_changed.is_empty() {
                let insertion = old_changed.start;
                if insertion <= block.range.start {
                    shift_range(&mut block.range, old_length, new_length);
                    return true;
                }
                if insertion >= block.range.end {
                    return true;
                }
                unfolded.push(block.id);
                return false;
            }

            if old_changed.end <= block.range.start {
                shift_range(&mut block.range, old_length, new_length);
                return true;
            }
            if old_changed.start >= block.range.end {
                return true;
            }
            unfolded.push(block.id);
            false
        });

        PasteReconcile {
            old_changed,
            new_changed,
            unfolded,
        }
    }

    pub(super) fn delete(
        &mut self,
        draft: &str,
        id: PasteBlockId,
    ) -> Result<Option<PasteDeletion>, PasteBlockError> {
        let Some(index) = self.blocks.iter().position(|block| block.id == id) else {
            return Ok(None);
        };
        let block = &self.blocks[index];
        let text = checked_slice(draft, &block.range)?;
        if fingerprint(text) != block.fingerprint {
            return Err(PasteBlockError::message(
                "pasted block changed; unfold it before deleting",
            ));
        }

        let range = block.range.clone();
        let mut next_draft = draft.to_owned();
        next_draft.replace_range(range.clone(), "");
        self.blocks.remove(index);
        for later in &mut self.blocks[index..] {
            later.range.start -= range.len();
            later.range.end -= range.len();
        }
        Ok(Some(PasteDeletion {
            draft: next_draft,
            cursor: range.start,
        }))
    }

    /// Draft ranges that edits may only remove whole.
    pub(super) fn ranges(&self) -> Vec<Range<usize>> {
        self.blocks
            .iter()
            .map(|block| block.range.clone())
            .collect()
    }

    pub(super) fn block_at_raw_cursor(&self, cursor: usize) -> Option<PasteBlockId> {
        self.blocks
            .iter()
            .find(|block| block.range.start <= cursor && cursor <= block.range.end)
            .map(|block| block.id)
    }

    pub(super) fn cursor_before_block(&self, id: PasteBlockId) -> Option<usize> {
        self.blocks
            .iter()
            .find(|block| block.id == id)
            .map(|block| block.range.start)
    }

    pub(super) fn cursor_after_block(&self, id: PasteBlockId) -> Option<usize> {
        self.blocks
            .iter()
            .find(|block| block.id == id)
            .map(|block| block.range.end)
    }

    pub(super) fn projection(
        &self,
        draft: &str,
        raw_cursor: usize,
        available_columns: usize,
    ) -> PasteProjection {
        let raw_cursor = floor_char_boundary(draft, raw_cursor.min(draft.len()));
        if available_columns == 0 {
            return PasteProjection {
                text: draft.to_owned(),
                cursor: raw_cursor,
                blocks: Vec::new(),
                issues: Vec::new(),
            };
        }

        let mut text = String::new();
        let mut rendered_blocks = Vec::new();
        let mut issues = Vec::new();
        let mut raw_offset = 0;
        let mut projected_cursor = None;

        for block in &self.blocks {
            let Some(block_text) = valid_slice(draft, &block.range) else {
                issues.push(PasteRestoreIssue::OriginalMissing { id: block.id.0 });
                continue;
            };
            if fingerprint(block_text) != block.fingerprint {
                issues.push(PasteRestoreIssue::OriginalChanged { id: block.id.0 });
                continue;
            }
            if block.range.start < raw_offset {
                issues.push(PasteRestoreIssue::OverlappingBlock { id: block.id.0 });
                continue;
            }

            if projected_cursor.is_none() && raw_cursor <= block.range.start {
                projected_cursor = Some(text.len() + raw_cursor.saturating_sub(raw_offset));
            }
            text.push_str(&draft[raw_offset..block.range.start]);
            let display_start = text.len();
            let label = block_label(block, available_columns);
            text.push_str(&label);
            let display_end = text.len();
            if projected_cursor.is_none() && raw_cursor < block.range.end {
                projected_cursor = Some(display_start);
            }
            rendered_blocks.push(RenderedPasteBlock {
                id: block.id,
                raw_range: block.range.clone(),
                display_range: display_start..display_end,
                label,
                is_image: matches!(block.content, PasteContent::Image(_)),
            });
            raw_offset = block.range.end;
        }

        if projected_cursor.is_none() {
            projected_cursor = Some(text.len() + raw_cursor.saturating_sub(raw_offset));
        }
        text.push_str(&draft[raw_offset..]);

        PasteProjection {
            text,
            cursor: projected_cursor.unwrap_or(0),
            blocks: rendered_blocks,
            issues,
        }
    }

    pub(super) fn source_text<'draft>(&self, draft: &'draft str) -> &'draft str {
        draft
    }
}

fn image_id(id: PasteBlockId) -> Result<u32, PasteBlockError> {
    u32::try_from(id.0).map_err(|_| PasteBlockError::message("pasted image IDs are exhausted"))
}

fn block_label(block: &PasteBlock, available_columns: usize) -> String {
    let (full, compact) = match block.content {
        PasteContent::Text { line_count } => {
            let extra_lines = line_count.saturating_sub(1);
            let full = if extra_lines == 0 {
                format!("[Pasted text #{}]", block.id.0)
            } else {
                format!("[Pasted text #{} +{extra_lines} lines]", block.id.0)
            };
            (full, format!("[Paste #{}]", block.id.0))
        }
        PasteContent::Image(_) => {
            let placeholder = format!("[Image #{}]", block.id.0);
            (placeholder.clone(), placeholder)
        }
    };
    if full.len() <= available_columns {
        return full;
    }
    if compact.len() <= available_columns {
        return compact;
    }
    if available_columns == 1 {
        return "…".into();
    }
    truncate_characters(&format!("#{}", block.id.0), available_columns)
}

fn truncate_characters(text: &str, length: usize) -> String {
    text.chars().take(length).collect()
}

fn changed_ranges(before: &str, after: &str) -> (Range<usize>, Range<usize>) {
    let prefix = common_prefix_bytes(before, after);
    let suffix = common_suffix_bytes(&before[prefix..], &after[prefix..]);
    (
        prefix..before.len().saturating_sub(suffix),
        prefix..after.len().saturating_sub(suffix),
    )
}

fn common_prefix_bytes(left: &str, right: &str) -> usize {
    left.chars()
        .zip(right.chars())
        .take_while(|(left, right)| left == right)
        .map(|(character, _)| character.len_utf8())
        .sum()
}

fn common_suffix_bytes(left: &str, right: &str) -> usize {
    left.chars()
        .rev()
        .zip(right.chars().rev())
        .take_while(|(left, right)| left == right)
        .map(|(character, _)| character.len_utf8())
        .sum()
}

fn shift_range(range: &mut Range<usize>, old_length: usize, new_length: usize) {
    if new_length >= old_length {
        let shift = new_length - old_length;
        range.start = range.start.saturating_add(shift);
        range.end = range.end.saturating_add(shift);
    } else {
        let shift = old_length - new_length;
        range.start = range.start.saturating_sub(shift);
        range.end = range.end.saturating_sub(shift);
    }
}

fn ranges_overlap(left: &Range<usize>, right: &Range<usize>) -> bool {
    left.start < right.end && right.start < left.end
}

fn checked_slice<'text>(
    text: &'text str,
    range: &Range<usize>,
) -> Result<&'text str, PasteBlockError> {
    valid_slice(text, range).ok_or_else(|| {
        PasteBlockError::message("pasted block range is outside the draft or not UTF-8 aligned")
    })
}

fn valid_slice<'text>(text: &'text str, range: &Range<usize>) -> Option<&'text str> {
    (range.start <= range.end
        && range.end <= text.len()
        && text.is_char_boundary(range.start)
        && text.is_char_boundary(range.end))
    .then(|| &text[range.clone()])
}

fn logical_line_count(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let mut lines = 1usize;
    let mut previous_was_carriage_return = false;
    for character in text.chars() {
        match character {
            '\r' => {
                lines += 1;
                previous_was_carriage_return = true;
            }
            '\n' if previous_was_carriage_return => previous_was_carriage_return = false,
            '\n' => lines += 1,
            _ => previous_was_carriage_return = false,
        }
    }
    lines
}

fn fingerprint(text: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    fn large(label: &str) -> String {
        format!("{label}{}", "x".repeat(801))
    }

    fn folded_id(registration: PasteRegistration) -> PasteBlockId {
        match registration {
            PasteRegistration::Folded(id) => id,
            PasteRegistration::Inline => panic!("expected folded paste"),
        }
    }

    #[test]
    fn threshold_is_strict_for_characters_and_logical_lines() {
        assert!(!PasteBlocks::is_large_paste(&"x".repeat(800)));
        assert!(PasteBlocks::is_large_paste(&"x".repeat(801)));
        assert!(!PasteBlocks::is_large_paste("one\ntwo\nthree"));
        assert!(PasteBlocks::is_large_paste("one\ntwo\nthree\nfour"));
        assert!(PasteBlocks::is_large_paste("one\r\ntwo\rthree\nfour"));
    }

    #[test]
    fn projection_folds_multiple_blocks_but_source_text_stays_exact() {
        let first = large("first:");
        let second = "甲\n乙\n丙\n丁";
        let draft = format!("before {first} between {second} after");
        let first_start = "before ".len();
        let first_end = first_start + first.len();
        let second_start = first_end + " between ".len();
        let second_end = second_start + second.len();
        let mut blocks = PasteBlocks::new();
        let first_id = folded_id(blocks.register(&draft, first_start..first_end).unwrap());
        let second_id = folded_id(blocks.register(&draft, second_start..second_end).unwrap());

        let projection = blocks.projection(&draft, draft.len(), 80);
        assert_eq!(first_id.get(), 1);
        assert_eq!(second_id.get(), 2);
        assert_eq!(projection.blocks.len(), 2);
        assert!(
            projection
                .text
                .starts_with("before [Pasted text #1] between ")
        );
        assert!(projection.text.contains("[Pasted text #2 +3 lines]"));
        assert!(projection.text.ends_with(" after"));
        assert_eq!(projection.cursor, projection.text.len());
        assert_eq!(blocks.source_text(&draft), draft);
        assert!(!blocks.source_text(&draft).contains("[Pasted text"));
    }

    #[test]
    fn nearby_utf8_edits_shift_only_later_ranges() {
        let first = large("第一块");
        let second = large("第二块");
        let before = format!("头 {first} 中 {second} 尾");
        let first_start = "头 ".len();
        let first_end = first_start + first.len();
        let second_start = first_end + " 中 ".len();
        let second_end = second_start + second.len();
        let mut blocks = PasteBlocks::new();
        let first_id = folded_id(blocks.register(&before, first_start..first_end).unwrap());
        let second_id = folded_id(blocks.register(&before, second_start..second_end).unwrap());

        let after = format!("新{before}");
        let report = blocks.reconcile(&before, &after);
        assert!(report.unfolded.is_empty());
        assert_eq!(blocks.cursor_before_block(first_id), Some("新头 ".len()));
        assert_eq!(
            blocks.cursor_before_block(second_id),
            Some(second_start + "新".len())
        );

        let between = blocks.cursor_after_block(first_id).unwrap();
        let mut edited = after.clone();
        edited.insert_str(between, "附近");
        blocks.reconcile(&after, &edited);
        assert_eq!(blocks.cursor_after_block(first_id), Some(between));
        assert_eq!(
            blocks.cursor_before_block(second_id),
            Some(second_start + "新附近".len())
        );
    }

    #[test]
    fn editing_inside_a_block_unfolds_it_without_changing_raw_text() {
        let paste = large("body:");
        let before = format!("pre{paste}post");
        let start = "pre".len();
        let end = start + paste.len();
        let mut blocks = PasteBlocks::new();
        let id = folded_id(blocks.register(&before, start..end).unwrap());
        let mut after = before.clone();
        after.insert_str(start + "body:".len(), "EDIT");

        let report = blocks.reconcile(&before, &after);
        assert_eq!(report.unfolded, [id]);
        assert!(blocks.projection(&after, after.len(), 80).blocks.is_empty());
        assert!(after.contains("body:EDIT"));
    }

    #[test]
    fn deleting_one_block_removes_exact_raw_content_and_shifts_the_next() {
        let first = large("first");
        let second = large("second");
        let draft = format!("a{first}b{second}c");
        let first_start = 1;
        let first_end = first_start + first.len();
        let second_start = first_end + 1;
        let second_end = second_start + second.len();
        let mut blocks = PasteBlocks::new();
        let first_id = folded_id(blocks.register(&draft, first_start..first_end).unwrap());
        let second_id = folded_id(blocks.register(&draft, second_start..second_end).unwrap());

        let deletion = blocks.delete(&draft, first_id).unwrap().unwrap();
        assert_eq!(deletion.draft, format!("ab{second}c"));
        assert_eq!(deletion.cursor, 1);
        assert_eq!(blocks.cursor_before_block(second_id), Some(2));
        assert_eq!(blocks.source_text(&deletion.draft), deletion.draft.as_str());
    }

    #[test]
    fn resize_changes_only_the_label_and_preserves_raw_identity_and_mapping() {
        let paste = "一\n二\n三\n四";
        let draft = format!("前{paste}后");
        let start = "前".len();
        let end = start + paste.len();
        let mut blocks = PasteBlocks::new();
        let id = folded_id(blocks.register(&draft, start..end).unwrap());

        let wide = blocks.projection(&draft, end, 80);
        let compact = blocks.projection(&draft, end, 10);
        let tiny = blocks.projection(&draft, end, 1);
        assert_eq!(wide.blocks[0].label, "[Pasted text #1 +3 lines]");
        assert_eq!(compact.blocks[0].label, "[Paste #1]");
        assert_eq!(tiny.blocks[0].label, "…");
        assert_eq!(wide.blocks[0].raw_range, start..end);
        assert_eq!(compact.blocks[0].raw_range, start..end);
        assert_eq!(tiny.cursor, tiny.blocks[0].display_range.end);
        assert_eq!(blocks.cursor_after_block(id), Some(end));
        assert_eq!(blocks.source_text(&draft), draft);
    }

    #[test]
    fn invalid_utf8_ranges_are_rejected_without_panicking() {
        let draft = format!("中{}", "x".repeat(801));
        let mut blocks = PasteBlocks::new();
        let error = blocks.register(&draft, 1..draft.len()).unwrap_err();
        assert!(error.to_string().contains("UTF-8"));
        assert!(blocks.blocks.is_empty());
    }

    #[test]
    fn insertion_at_block_boundaries_stays_outside_the_fold() {
        let paste = large("block:");
        let before = format!("a{paste}b");
        let start = 1;
        let end = start + paste.len();
        let mut blocks = PasteBlocks::new();
        let id = folded_id(blocks.register(&before, start..end).unwrap());

        let mut at_start = before.clone();
        at_start.insert(start, '左');
        let report = blocks.reconcile(&before, &at_start);
        assert!(report.unfolded.is_empty());
        assert_eq!(blocks.cursor_before_block(id), Some(start + "左".len()));

        let current_end = blocks.cursor_after_block(id).unwrap();
        let mut at_end = at_start.clone();
        at_end.insert(current_end, '右');
        let report = blocks.reconcile(&at_start, &at_end);
        assert!(report.unfolded.is_empty());
        assert_eq!(blocks.cursor_after_block(id), Some(current_end));
    }

    fn image(width: u32) -> Image {
        Image {
            media_type: tickloop_agent_protocol::ImageMediaType::Png,
            data: "iVBORw0KGgo=".into(),
            width,
            height: 1,
        }
    }

    fn attach(blocks: &mut PasteBlocks, draft: &mut String, width: u32) -> PasteBlockId {
        let id = blocks.next_id();
        let placeholder = PasteBlocks::image_placeholder(id).unwrap();
        let start = draft.len();
        draft.push_str(&placeholder);
        blocks
            .register_image(draft, start..draft.len(), id, image(width))
            .unwrap();
        id
    }

    #[test]
    fn images_share_numbering_with_folded_text_and_keep_it_after_the_draft_is_sent() {
        let paste = large("text:");
        let mut draft = paste.clone();
        let mut blocks = PasteBlocks::new();
        folded_id(blocks.register(&draft, 0..draft.len()).unwrap());
        let image_id = attach(&mut blocks, &mut draft, 2);

        let projection = blocks.projection(&draft, draft.len(), 80);
        assert_eq!(image_id.get(), 2);
        assert_eq!(projection.text, "[Pasted text #1][Image #2]");
        assert_eq!(
            projection
                .blocks
                .iter()
                .map(|block| block.is_image)
                .collect::<Vec<_>>(),
            [false, true]
        );
        assert_eq!(blocks.source_text(&draft), format!("{paste}[Image #2]"));

        let sent = blocks.take();
        assert_eq!(sent.images(&draft).len(), 1);
        assert_eq!(blocks.next_id().get(), 3);
        blocks.continue_after_image(7);
        assert_eq!(blocks.next_id().get(), 8);
    }

    #[test]
    fn a_saved_image_returns_only_to_its_own_free_placeholder() {
        let draft = "[Image #1] and [Image #2] and [Image #2]";
        let mut blocks = PasteBlocks::new();
        let second = draft.find("[Image #2]").unwrap();
        assert!(
            blocks
                .register_image(draft, 0.."[Image #1]".len(), PasteBlockId(2), image(1))
                .is_err()
        );

        blocks.restore_image(draft, 2, image(2));
        blocks.restore_image(draft, 2, image(3));
        blocks.restore_image(draft, 9, image(9));

        let images = blocks.images(draft);
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].origin, ImageOrigin::Pasted { id: 2 });
        assert_eq!(images[0].image.width, 2);
        assert_eq!(blocks.cursor_before_block(PasteBlockId(2)), Some(second));
        assert_eq!(blocks.next_id().get(), 3);
    }
}
