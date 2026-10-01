//! Image limits after Claude Code: at most 100 images per request (`API_MAX_MEDIA_PER_REQUEST`),
//! and images become `[image]` before summaries and after an image rejection
//! (`stripImagesFromMessages`).

use super::{Content, Message, ModelRequest, ToolResultBlock, ToolResultContent};

const MAX_IMAGES_PER_REQUEST: usize = 100;
const IMAGE_PLACEHOLDER: &str = "[image]";

/// Replaces every image with an `[image]` text block so summaries and retries never carry them.
pub(crate) fn strip_images(messages: &mut [Message]) {
    replace_images(messages, || true);
}

pub(super) fn limit_images_per_request(messages: &mut [Message]) {
    let mut excess = count_images(messages).saturating_sub(MAX_IMAGES_PER_REQUEST);
    replace_images(messages, || {
        let replaces_oldest = excess > 0;
        excess = excess.saturating_sub(1);
        replaces_oldest
    });
}

/// The request to retry with when the service rejects its images; `None` when it has none.
pub(super) fn image_free_copy(request: &ModelRequest) -> Option<ModelRequest> {
    (count_images(&request.messages) > 0).then(|| {
        let mut copy = request.clone();
        strip_images(&mut copy.messages);
        copy
    })
}

/// Matches the size, dimension and count rejections Claude Code recognizes
/// (`isMediaSizeError`), plus the OpenAI-style "too many images" wording.
pub(super) fn rejects_images(error_body: &str) -> bool {
    let body = error_body.to_lowercase();
    body.contains("image")
        && ["exceed", "too many", "too large"]
            .iter()
            .any(|reason| body.contains(reason))
}

fn count_images(messages: &[Message]) -> usize {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .map(|content| match content {
            Content::Image { .. } => 1,
            Content::ToolResult {
                content: ToolResultContent::Blocks(blocks),
                ..
            } => blocks
                .iter()
                .filter(|block| matches!(block, ToolResultBlock::Image { .. }))
                .count(),
            _ => 0,
        })
        .sum()
}

/// Visits images oldest first, including those nested in tool results, and replaces those
/// `should_replace` selects.
fn replace_images(messages: &mut [Message], mut should_replace: impl FnMut() -> bool) {
    for content in messages.iter_mut().flat_map(|message| &mut message.content) {
        match content {
            Content::Image { .. } if should_replace() => {
                *content = Content::Text {
                    text: IMAGE_PLACEHOLDER.into(),
                };
            }
            Content::ToolResult {
                content: ToolResultContent::Blocks(blocks),
                ..
            } => {
                for block in blocks {
                    if matches!(block, ToolResultBlock::Image { .. }) && should_replace() {
                        *block = ToolResultBlock::Text {
                            text: IMAGE_PLACEHOLDER.into(),
                        };
                    }
                }
            }
            _ => {}
        }
    }
}
