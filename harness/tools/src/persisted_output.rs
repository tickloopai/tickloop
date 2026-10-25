//! Claude Code's `<persisted-output>` message (`utils/toolResultStorage.ts`): a preview of a large
//! result and the file that holds all of it. Lengths count UTF-16 code units, as in JavaScript.

use std::path::Path;

use super::format_file_size;

pub const PERSISTED_OUTPUT_TAG: &str = "<persisted-output>";
pub(crate) const PERSISTED_OUTPUT_CLOSING_TAG: &str = "</persisted-output>";
pub const SAVED_TO_LABEL: &str = "Full output saved to: ";
pub const PREVIEW_LABEL: &str = "\n\nPreview (first ";
const PREVIEW_SIZE_BYTES: usize = 2000;

/// Claude Code `buildLargeToolResultMessage`. `original_size` is the size of the whole output;
/// `content` holds at least its beginning, from which the preview is taken.
pub fn large_output_message(path: &Path, original_size: usize, content: &str) -> String {
    let (preview, has_more) = generate_preview(content, PREVIEW_SIZE_BYTES);
    format!(
        "{PERSISTED_OUTPUT_TAG}\nOutput too large ({}). {SAVED_TO_LABEL}{}{PREVIEW_LABEL}{}):\n{preview}{}{PERSISTED_OUTPUT_CLOSING_TAG}",
        format_file_size(original_size),
        path.display(),
        format_file_size(PREVIEW_SIZE_BYTES),
        if has_more { "\n...\n" } else { "\n" },
    )
}

/// Claude Code `generatePreview`: the first `max_units` UTF-16 code units, cut at the last newline
/// when one falls in the second half. JavaScript may split a surrogate pair; this stops before
/// that character instead.
pub fn generate_preview(content: &str, max_units: usize) -> (&str, bool) {
    if utf16_length(content) <= max_units {
        return (content, false);
    }
    let mut units = 0;
    let mut limit = content.len();
    let mut last_newline = None;
    for (byte, character) in content.char_indices() {
        let width = character.len_utf16();
        if units + width > max_units {
            limit = byte;
            break;
        }
        if character == '\n' {
            last_newline = Some((units, byte));
        }
        units += width;
    }
    let cut = match last_newline {
        Some((unit, byte)) if unit > max_units / 2 => byte,
        _ => limit,
    };
    (&content[..cut], true)
}

pub fn utf16_length(text: &str) -> usize {
    text.encode_utf16().count()
}
