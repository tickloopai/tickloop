use std::collections::{HashMap, HashSet};

use sha2::{Digest, Sha256};

use super::{ModelRequest, UNSUPPORTED_IMAGE_TEXT};
use tickloop_harness_protocol::{
    Adapter, Content, Message, ModelConfig, PrivateHistorySource, Protocol, Role, ToolResultBlock,
    ToolResultContent,
};

pub(crate) fn compatible_source(
    source: Option<&PrivateHistorySource>,
    target: Option<&PrivateHistorySource>,
) -> bool {
    let (Some(source), Some(target)) = (source, target) else {
        return false;
    };
    let mut expected = target.clone();
    if expected.credential_identity.is_none() {
        expected.credential_identity = source.credential_identity.clone();
    }
    source == &expected
}

/// Runtime checks model provenance; the client repeats projection with the actual credential
/// identity before encoding. Neither pass changes the canonical conversation.
pub fn project_request(
    mut request: ModelRequest,
    target: &ModelConfig,
    target_source: Option<&PrivateHistorySource>,
) -> ModelRequest {
    let has_sources = request.history_sources.len() == request.messages.len();
    if !has_sources {
        return request;
    }
    let mut messages = Vec::with_capacity(request.messages.len());
    let mut sources = Vec::with_capacity(request.history_sources.len());
    for (mut message, source) in request.messages.into_iter().zip(request.history_sources) {
        let compatible = compatible_source(source.as_ref(), target_source);
        message.content = message
            .content
            .into_iter()
            .flat_map(|block| project_block(block, compatible, target))
            .collect();
        if !message.content.is_empty() {
            messages.push(message);
            sources.push(source);
        }
    }
    normalize_tool_ids(&mut messages, target.adapter());
    request.messages = messages;
    request.history_sources = sources;
    request
}

fn project_block(block: Content, compatible: bool, target: &ModelConfig) -> Vec<Content> {
    match block {
        Content::Thinking {
            thinking,
            signature,
        } if !compatible => {
            let _ = signature;
            readable_text(thinking)
        }
        Content::ProtocolData { protocol, value } if !compatible || protocol != target.protocol => {
            if crate::native_tool_search::is_record(protocol, &value) {
                crate::native_tool_search::as_text(&value)
            } else if protocol == Protocol::Responses && value["type"] == "reasoning" {
                [
                    (&value["summary"], "summary_text"),
                    (&value["content"], "reasoning_text"),
                ]
                .into_iter()
                .flat_map(|(parts, kind)| {
                    parts
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(move |part| part["type"] == kind)
                })
                .filter_map(|part| part["text"].as_str())
                .flat_map(|text| readable_text(text.to_owned()))
                .collect()
            } else {
                Vec::new()
            }
        }
        Content::Image { .. } if !target.accepts_images() => vec![Content::Text {
            text: UNSUPPORTED_IMAGE_TEXT.into(),
        }],
        Content::ToolResult {
            tool_use_id,
            content: ToolResultContent::Blocks(mut blocks),
            is_error,
        } if !target.accepts_images() => {
            for block in &mut blocks {
                if matches!(block, ToolResultBlock::Image { .. }) {
                    *block = ToolResultBlock::Text {
                        text: UNSUPPORTED_IMAGE_TEXT.into(),
                    };
                }
            }
            vec![Content::ToolResult {
                tool_use_id,
                content: ToolResultContent::Blocks(blocks),
                is_error,
            }]
        }
        block => vec![block],
    }
}

fn readable_text(text: String) -> Vec<Content> {
    if text.trim().is_empty() {
        Vec::new()
    } else {
        vec![Content::Text { text }]
    }
}

fn normalize_tool_ids(messages: &mut [Message], adapter: Adapter) {
    let valid = |id: &str| {
        !id.is_empty()
            && id.len() <= 64
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
    };
    let mut used = messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            Content::ToolUse { id, .. } if valid(id) => Some(id.clone()),
            _ => None,
        })
        .collect::<HashSet<_>>();
    let mut mapping = HashMap::new();
    for message in messages.iter() {
        for block in &message.content {
            if let Content::ToolUse { id, .. } = block {
                if valid(id) || adapter == Adapter::CodexResponses && id.contains('|') {
                    continue;
                }
                if mapping.contains_key(id) {
                    continue;
                }
                let digest = Sha256::digest(id.as_bytes());
                let base = format!(
                    "tl_{}",
                    digest
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<String>()
                );
                let mut candidate = base[..59].to_owned();
                let mut suffix = 0;
                while !used.insert(candidate.clone()) {
                    suffix += 1;
                    candidate = format!("{}_{suffix}", &base[..48]);
                }
                mapping.insert(id.clone(), candidate);
            }
        }
    }
    for message in messages {
        for block in &mut message.content {
            let id = match block {
                Content::ToolUse { id, .. } if message.role == Role::Assistant => id,
                Content::ToolResult { tool_use_id, .. } => tool_use_id,
                _ => continue,
            };
            if let Some(mapped) = mapping.get(id) {
                *id = mapped.clone();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ModelConfigExt;
    use serde_json::json;
    use tickloop_harness_protocol::ImageSource;
    use tickloop_harness_protocol::InputModality;
    use tickloop_harness_protocol::ModelAuthSource;
    use tickloop_harness_protocol::ModelProvider;

    fn model(protocol: Protocol) -> ModelConfig {
        ModelConfig {
            name: "test".into(),
            protocol,
            base_url: "http://127.0.0.1:9999".into(),
            model: "test".into(),
            api_key_env: String::new(),
            max_output_tokens: 4096,
            provider: ModelProvider::Api,
            auth_source: ModelAuthSource::ApiKeyEnv,
            provider_id: "test".into(),
            provider_name: "Test".into(),
            model_id: "test".into(),
            connection: None,
            card: None,
            input_modalities: vec![InputModality::Text],
        }
    }

    fn request(messages: Vec<Message>, source: Option<PrivateHistorySource>) -> ModelRequest {
        ModelRequest {
            deferred_tools: Vec::new(),
            history_sources: messages.iter().map(|_| source.clone()).collect(),
            messages,
            tools: Vec::new(),
            structured_output: None,
            temperature: None,
            reasoning_effort: None,
        }
    }

    #[test]
    fn matching_source_preserves_blocks_and_cache_prefix_while_changed_credentials_strip_signature()
    {
        let target = model(Protocol::Messages);
        let mut source = target.private_history_source().unwrap();
        source.credential_identity = Some("account-a".into());
        let original = request(
            vec![Message {
                role: Role::Assistant,
                content: vec![
                    Content::Thinking {
                        thinking: "readable thinking".into(),
                        signature: Some("signed".into()),
                    },
                    Content::Text {
                        text: "answer".into(),
                    },
                    Content::ToolUse {
                        id: "original-valid-id".into(),
                        name: "read".into(),
                        input: json!({"path":"file"}),
                    },
                ],
            }],
            Some(source.clone()),
        );
        let repeated = project_request(original.clone(), &target, Some(&source));
        assert_eq!(repeated.messages, original.messages);
        assert_eq!(
            project_request(repeated.clone(), &target, Some(&source)).messages,
            original.messages
        );
        source.credential_identity = Some("account-b".into());
        let different = project_request(original.clone(), &target, Some(&source));
        assert!(
            matches!(&different.messages[0].content[0], Content::Text {text} if text == "readable thinking")
        );
        assert_eq!(
            different.messages[0].content[1..],
            original.messages[0].content[1..]
        );
        assert!(matches!(
            &original.messages[0].content[0],
            Content::Thinking {
                signature: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn foreign_opaque_state_is_removed_and_tool_identifiers_are_deterministic_and_paired() {
        let original_source = model(Protocol::Responses).private_history_source();
        let long_id = "illegal/identifier".repeat(10);
        let messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![
                    Content::ProtocolData {
                        protocol: Protocol::Responses,
                        value: json!({"type":"reasoning","summary":[{"type":"summary_text","text":"Public summary"}],"encrypted_content":"opaque-secret"}),
                    },
                    Content::ToolUse {
                        id: long_id.clone(),
                        name: "read".into(),
                        input: json!({}),
                    },
                    Content::ToolUse {
                        id: "illegal?identifier".into(),
                        name: "read".into(),
                        input: json!({}),
                    },
                ],
            },
            Message {
                role: Role::User,
                content: vec![
                    Content::ToolResult {
                        tool_use_id: long_id,
                        content: ToolResultContent::Text(
                            "pending; outcome unknown; do not repeat".into(),
                        ),
                        is_error: false,
                    },
                    Content::ToolResult {
                        tool_use_id: "illegal?identifier".into(),
                        content: ToolResultContent::Text("done".into()),
                        is_error: false,
                    },
                ],
            },
        ];
        let original = request(messages, original_source);
        let target = model(Protocol::Messages);
        let converted = project_request(
            original.clone(),
            &target,
            target.private_history_source().as_ref(),
        );
        assert_eq!(
            converted.messages,
            project_request(
                original.clone(),
                &target,
                target.private_history_source().as_ref()
            )
            .messages
        );
        assert!(
            !serde_json::to_string(&converted.messages)
                .unwrap()
                .contains("opaque-secret")
        );
        assert!(
            matches!(&converted.messages[0].content[0], Content::Text {text} if text == "Public summary")
        );
        let Content::ToolUse { id: first, .. } = &converted.messages[0].content[1] else {
            panic!()
        };
        let Content::ToolUse { id: second, .. } = &converted.messages[0].content[2] else {
            panic!()
        };
        assert_ne!(first, second);
        assert!(first.len() <= 64);
        assert!(
            matches!(&converted.messages[1].content[0], Content::ToolResult {tool_use_id, content:ToolResultContent::Text(text), is_error:false} if tool_use_id == first && text.contains("do not repeat"))
        );
        assert!(
            serde_json::to_string(&original.messages)
                .unwrap()
                .contains("opaque-secret")
        );
    }

    #[test]
    fn foreign_responses_readable_reasoning_survives_without_summary_or_opaque_state() {
        let source = model(Protocol::Responses).private_history_source();
        let original = request(
            vec![Message {
                role: Role::Assistant,
                content: vec![Content::ProtocolData {
                    protocol: Protocol::Responses,
                    value: json!({"type":"reasoning", "summary":[], "content":[
                    {"type":"reasoning_text", "text":"Read both files"},
                    {"type":"reasoning_text", "text":"Compare the contracts"},
                    {"type":"encrypted_content", "text":"opaque-secret"}
                ], "encrypted_content":"opaque-secret"}),
                }],
            }],
            source,
        );
        for protocol in [Protocol::Messages, Protocol::ChatCompletions] {
            let target = model(protocol);
            let projected = project_request(
                original.clone(),
                &target,
                target.private_history_source().as_ref(),
            );
            assert_eq!(
                projected.messages[0].content,
                vec![
                    Content::Text {
                        text: "Read both files".into()
                    },
                    Content::Text {
                        text: "Compare the contracts".into()
                    },
                ]
            );
            assert!(
                !serde_json::to_string(&projected.messages)
                    .unwrap()
                    .contains("opaque-secret")
            );
        }
        assert!(matches!(
            &original.messages[0].content[0],
            Content::ProtocolData { .. }
        ));
    }

    #[test]
    fn historical_images_and_tool_images_are_replaced_only_in_the_target_copy() {
        let image = Content::Image {
            source: ImageSource::Base64 {
                media_type: tickloop_harness_protocol::ImageMediaType::Png,
                data: "image-data".into(),
            },
            width: 1,
            height: 1,
        };
        let Content::Image { source, .. } = image.clone() else {
            panic!()
        };
        let original = request(
            vec![Message {
                role: Role::User,
                content: vec![
                    image,
                    Content::ToolResult {
                        tool_use_id: "call".into(),
                        content: ToolResultContent::Blocks(vec![ToolResultBlock::Image {
                            source,
                            width: 1,
                            height: 1,
                        }]),
                        is_error: false,
                    },
                ],
            }],
            None,
        );
        let target = model(Protocol::ChatCompletions);
        let projected = project_request(
            original.clone(),
            &target,
            target.private_history_source().as_ref(),
        );
        assert!(
            !serde_json::to_string(&projected.messages)
                .unwrap()
                .contains("image-data")
        );
        assert!(
            serde_json::to_string(&projected.messages)
                .unwrap()
                .contains(UNSUPPORTED_IMAGE_TEXT)
        );
        let mut vision = target;
        vision.input_modalities.push(InputModality::Image);
        assert_eq!(
            project_request(
                original.clone(),
                &vision,
                vision.private_history_source().as_ref()
            )
            .messages,
            original.messages
        );
    }
}
