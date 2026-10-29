//! Client attachments are untrusted material, not filesystem reads or execution authority.
use agent_client_protocol::{Error, schema::v1::PromptRequest};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::Semaphore;

// Hold the permit inside the blocking job: cancelling a prompt cannot unbound decode work.
static IMAGE_DECODERS: std::sync::LazyLock<Arc<Semaphore>> =
    std::sync::LazyLock::new(|| Arc::new(Semaphore::new(2)));
use tickloop_code_access::UserInput;
use tickloop_harness::base::image;
use tickloop_harness::protocol::{ImageContext, ImageOrigin, InputContext, ResourceContext};

const MAX_BLOCKS: usize = 64;
const MAX_IMAGES: usize = 4;
const MAX_RESOURCE_TEXT: usize = 1024 * 1024;
const MAX_IMAGE_BASE64: usize = 16 * 1024 * 1024;
const MAX_SOURCE_BASE64: usize = image::MAX_SOURCE_BYTES.div_ceil(3) * 4;
const MAX_URI: usize = 8192;

fn invalid(message: &str) -> Error {
    Error::invalid_params().data(serde_json::json!({"reason":message}))
}

fn optional_string(value: &Value, key: &str) -> Result<Option<String>, Error> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if text.len() <= MAX_URI => Ok(Some(text.clone())),
        _ => Err(invalid("invalid or oversized attachment metadata")),
    }
}

fn uri(value: &Value) -> Result<String, Error> {
    optional_string(value, "uri")?
        .filter(|uri| !uri.is_empty())
        .ok_or_else(|| invalid("attachment URI is required"))
}

pub(super) async fn prompt_input(request: &PromptRequest) -> Result<UserInput, Error> {
    let prompt = serde_json::to_value(&request.prompt).map_err(Error::into_internal_error)?;
    let blocks = prompt.as_array().ok_or_else(|| invalid("invalid prompt"))?;
    if blocks.len() > MAX_BLOCKS {
        return Err(invalid("ACP prompt exceeds 64 content blocks"));
    }
    let mut text = String::new();
    let mut text_bytes = 0usize;
    let mut contexts = Vec::new();
    let mut resource_bytes = 0usize;
    let mut image_bytes = 0usize;
    let mut images = 0u32;
    for block in blocks {
        match block["type"].as_str() {
            Some("text") => {
                let part = block["text"]
                    .as_str()
                    .ok_or_else(|| invalid("invalid text content"))?;
                text_bytes = text_bytes.saturating_add(part.len()).saturating_add(1);
                if text_bytes > super::MAX_PROMPT {
                    return Err(invalid("ACP prompt text exceeds 256 KiB"));
                }
                if contexts.is_empty() {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(part);
                } else if !part.is_empty() {
                    contexts.push(InputContext::PromptText {
                        text: part.to_owned(),
                    });
                }
            }
            Some("resource_link") => contexts.push(InputContext::Resource(ResourceContext {
                uri: uri(block)?,
                mime_type: optional_string(block, "mimeType")?,
                name: optional_string(block, "name")?,
                content: None,
            })),
            Some("resource") => {
                let resource = &block["resource"];
                let content = resource["text"]
                    .as_str()
                    .ok_or_else(|| invalid("only text embedded resources are supported"))?;
                resource_bytes = resource_bytes.saturating_add(content.len());
                if resource_bytes > MAX_RESOURCE_TEXT {
                    return Err(invalid("ACP embedded text exceeds 1 MiB"));
                }
                contexts.push(InputContext::Resource(ResourceContext {
                    uri: uri(resource)?,
                    mime_type: optional_string(resource, "mimeType")?,
                    name: optional_string(block, "name")?,
                    content: Some(content.to_owned()),
                }));
            }
            Some("image") => {
                images += 1;
                if images as usize > MAX_IMAGES {
                    return Err(invalid("ACP prompt exceeds four images"));
                }
                let data = block["data"]
                    .as_str()
                    .filter(|data| !data.is_empty())
                    .ok_or_else(|| {
                        invalid("inline image data is required; image URIs are not fetched")
                    })?;
                if data.len() > MAX_SOURCE_BASE64 {
                    return Err(invalid("ACP source image exceeds limit"));
                }
                image_bytes = image_bytes.saturating_add(data.len());
                if image_bytes > MAX_IMAGE_BASE64 {
                    return Err(invalid("ACP image data exceeds 16 MiB"));
                }
                let mime = block["mimeType"]
                    .as_str()
                    .ok_or_else(|| invalid("image MIME type is required"))?;
                let permit = IMAGE_DECODERS
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(|_| invalid("image preparation unavailable"))?;
                let source = STANDARD
                    .decode(data)
                    .map_err(|_| invalid("invalid image base64"))?;
                if image::signature_media_type(&source) != Some(mime) {
                    return Err(invalid(
                        "image MIME type does not match its supported format",
                    ));
                }
                let prepared = tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    image::prepare(&source)
                })
                .await
                .map_err(|_| invalid("image preparation failed"))?
                .map_err(|_| invalid("image could not be safely decoded or prepared"))?;
                contexts.push(InputContext::Image(ImageContext {
                    origin: ImageOrigin::Attached {
                        id: images,
                        uri: optional_string(block, "uri")?,
                    },
                    image: prepared,
                }));
            }
            _ => return Err(invalid("unsupported ACP prompt content type")),
        }
    }
    let prepared_bytes: usize = contexts
        .iter()
        .filter_map(|context| match context {
            InputContext::Image(image) => Some(image.image.data.len()),
            _ => None,
        })
        .sum();
    if prepared_bytes > MAX_IMAGE_BASE64 {
        return Err(invalid("prepared ACP images exceed 16 MiB"));
    }
    if text.trim().is_empty() && contexts.is_empty() {
        return Err(invalid("prompt must contain text or supported attachments"));
    }
    Ok(UserInput::Text {
        text,
        contexts,
        paths: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(prompt: Value) -> PromptRequest {
        serde_json::from_value(serde_json::json!({"sessionId":"test", "prompt":prompt})).unwrap()
    }
    #[tokio::test]
    async fn resource_links_are_references_not_file_reads() {
        let input = prompt_input(&request(serde_json::json!([
            {"type":"resource_link","uri":"file:///not/read","name":"context"},
            {"type":"resource","resource":{"uri":"test://data","text":"untrusted instructions"}}
        ])))
        .await
        .unwrap();
        let UserInput::Text {
            text,
            contexts,
            paths,
        } = input
        else {
            panic!()
        };
        assert!(text.is_empty() && paths.is_empty());
        assert_eq!(contexts.len(), 2);
        assert!(contexts.iter().all(|context| context.path().is_none()));
    }
    #[tokio::test]
    async fn rejects_binary_and_bad_image_data() {
        for prompt in [
            serde_json::json!([{"type":"resource","resource":{"uri":"test://data","blob":"aGVsbG8=","mimeType":"application/octet-stream"}}]),
            serde_json::json!([{"type":"image","data":"aGVsbG8=","mimeType":"image/png"}]),
        ] {
            assert!(prompt_input(&request(prompt)).await.is_err());
        }
    }
    #[tokio::test]
    async fn mixed_blocks_preserve_order_and_user_text() {
        let input = prompt_input(&request(serde_json::json!([
            {"type":"text","text":"before"},
            {"type":"resource_link","uri":"test://ref","name":"ref"},
            {"type":"text","text":"between"},
            {"type":"resource","resource":{"uri":"test://data","text":"material"}},
            {"type":"text","text":"after"}
        ])))
        .await
        .unwrap();
        let UserInput::Text { text, contexts, .. } = input else {
            panic!()
        };
        assert_eq!(text, "before");
        assert!(matches!(&contexts[0], InputContext::Resource(_)));
        assert!(matches!(&contexts[1], InputContext::PromptText { text } if text == "between"));
        assert!(matches!(&contexts[2], InputContext::Resource(_)));
        assert!(matches!(&contexts[3], InputContext::PromptText { text } if text == "after"));
    }
    #[tokio::test]
    async fn text_and_aggregate_limits_are_enforced() {
        assert!(
            prompt_input(&request(
                serde_json::json!([{"type":"text","text":"x".repeat(super::super::MAX_PROMPT+1)}])
            ))
            .await
            .is_err()
        );
        let prompt: Vec<_> = (0..MAX_BLOCKS + 1)
            .map(|_| serde_json::json!({"type":"text","text":"x"}))
            .collect();
        assert!(
            prompt_input(&request(serde_json::json!(prompt)))
                .await
                .is_err()
        );
    }
}
