//! Context token counting with the open-source `o200k_base` BPE vocabulary, embedded in the
//! binary. One tokenizer stands in for every provider: counts are estimates that a service's own
//! usage calibrates.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Mutex;

use serde_json::Value;
use tickloop_harness_protocol::{
    Content, ContextTokenCounter, Message, Protocol, message_content_tokens,
};

use crate::{native_tool_search, responses};

pub const TOKENIZER_NAME: &str = "o200k_base";
/// Counted messages kept before the cache starts over; a long Chat stays well inside it.
const MESSAGE_CACHE_ENTRIES: usize = 16_384;
/// Fields Tickloop keeps on a protocol record for itself; adapters drop them before sending.
const PRIVATE_FIELD_PREFIX: &str = "_tickloop_";

/// Whether `content` is the model's reasoning, whose size in context is the reasoning its
/// service reported for the response rather than any text history holds: thinking, often empty
/// when the service omits it, or a sealed reasoning record.
pub fn is_reasoning(content: &Content) -> bool {
    match content {
        Content::Thinking { .. } => true,
        Content::ProtocolData { protocol, value } => is_reasoning_record(*protocol, value),
        _ => false,
    }
}

fn is_reasoning_record(protocol: Protocol, value: &Value) -> bool {
    match protocol {
        Protocol::Responses => value["type"] == "reasoning",
        Protocol::Google => value["part"].get("thoughtSignature").is_some(),
        Protocol::Messages => value["type"] == crate::MESSAGES_REDACTED_THINKING,
        Protocol::ChatCompletions | Protocol::SystemOne => false,
    }
}

/// The tokens a protocol record takes in a request as its adapter sends it. A sealed reasoning
/// record counts nothing here: its encrypted payload says nothing about its size.
pub fn protocol_record_tokens<C: ContextTokenCounter + ?Sized>(
    counter: &C,
    protocol: Protocol,
    value: &Value,
) -> usize {
    if is_reasoning_record(protocol, value) {
        return 0;
    }
    match (protocol, value["type"].as_str()) {
        // Sent as the `phase` of the assistant message that follows.
        (Protocol::Responses, Some(responses::ASSISTANT_PHASE_TYPE)) => {
            counter.text_tokens(value["phase"].as_str().unwrap_or_default())
        }
        // Sent as the `namespace` of the function call it names.
        (Protocol::Responses, Some(native_tool_search::RESPONSES_CALL_NAMESPACE)) => {
            counter.text_tokens(&value["namespace"].to_string())
        }
        // Sent in place of the readable block beside it, which counts on its own.
        (Protocol::Google, _) => 0,
        _ => match value.as_object() {
            Some(object)
                if object
                    .keys()
                    .any(|key| key.starts_with(PRIVATE_FIELD_PREFIX)) =>
            {
                let sent = object
                    .iter()
                    .filter(|(key, _)| !key.starts_with(PRIVATE_FIELD_PREFIX))
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect::<serde_json::Map<_, _>>();
                counter.text_tokens(&Value::Object(sent).to_string())
            }
            _ => counter.text_tokens(&value.to_string()),
        },
    }
}

pub struct O200kTokenCounter {
    /// Message counts by a hash of the serialized message: a message never changes once written,
    /// so a status read counts only what was appended since.
    messages: Mutex<HashMap<u64, usize>>,
}

pub static TOKEN_COUNTER: std::sync::LazyLock<O200kTokenCounter> =
    std::sync::LazyLock::new(|| O200kTokenCounter {
        messages: Mutex::new(HashMap::new()),
    });

/// Installs the tokenizer as the process's context token counter.
pub fn install() {
    tickloop_harness_protocol::set_context_token_counter(&*TOKEN_COUNTER);
}

impl ContextTokenCounter for O200kTokenCounter {
    fn name(&self) -> &'static str {
        TOKENIZER_NAME
    }

    fn text_tokens(&self, text: &str) -> usize {
        if text.is_empty() {
            return 0;
        }
        tiktoken_rs::o200k_base_singleton().count_ordinary(text)
    }

    fn protocol_data_tokens(&self, protocol: Protocol, value: &Value) -> usize {
        protocol_record_tokens(self, protocol, value)
    }

    fn message_tokens(&self, message: &Message) -> usize {
        let Ok(serialized) = serde_json::to_vec(message) else {
            return message_content_tokens(self, message);
        };
        let mut hasher = DefaultHasher::new();
        serialized.hash(&mut hasher);
        let key = hasher.finish();
        if let Some(tokens) = self.messages.lock().unwrap().get(&key) {
            return *tokens;
        }
        let tokens = message_content_tokens(self, message);
        let mut messages = self.messages.lock().unwrap();
        if messages.len() >= MESSAGE_CACHE_ENTRIES {
            messages.clear();
        }
        messages.insert(key, tokens);
        tokens
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickloop_harness_protocol::{Role, ToolDefinition};

    fn user(text: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![Content::Text { text: text.into() }],
        }
    }

    #[test]
    fn counts_are_deterministic_and_plausible_for_english_and_cjk() {
        let english = "The quick brown fox jumps over the lazy dog. ".repeat(20);
        let tokens = TOKEN_COUNTER.text_tokens(&english);
        assert_eq!(tokens, TOKEN_COUNTER.text_tokens(&english));
        assert!((150..=250).contains(&tokens), "{tokens}");
        let chinese = "上下文用量按类别估算，工具搜索候选池单独显示。".repeat(10);
        let tokens = TOKEN_COUNTER.text_tokens(&chinese);
        let characters = chinese.chars().count();
        assert!(
            tokens >= characters / 3 && tokens <= characters,
            "{tokens} tokens for {characters} characters"
        );
        assert_eq!(TOKEN_COUNTER.text_tokens(""), 0);
    }

    #[test]
    fn records_count_as_their_adapter_sends_them() {
        let origin = "9f".repeat(32);
        let sealed = serde_json::json!({
            "type": "reasoning",
            "id": "rs_1",
            "encrypted_content": "gAAAAB".repeat(400),
            "summary": [],
            "_tickloop_codex_origin": origin,
        });
        assert_eq!(
            TOKEN_COUNTER.protocol_data_tokens(Protocol::Responses, &sealed),
            0
        );
        assert!(is_reasoning(&Content::ProtocolData {
            protocol: Protocol::Responses,
            value: sealed,
        }));
        let redacted = serde_json::json!({"type": "redacted_thinking", "data": "EmwK".repeat(400)});
        assert_eq!(
            TOKEN_COUNTER.protocol_data_tokens(Protocol::Messages, &redacted),
            0
        );
        assert!(is_reasoning(&Content::ProtocolData {
            protocol: Protocol::Messages,
            value: redacted,
        }));
        let phase = serde_json::json!({
            "type": responses::ASSISTANT_PHASE_TYPE,
            "phase": "final_answer",
            "_tickloop_codex_origin": origin,
        });
        assert_eq!(
            TOKEN_COUNTER.protocol_data_tokens(Protocol::Responses, &phase),
            TOKEN_COUNTER.text_tokens("final_answer")
        );
        let search =
            serde_json::json!({"type": "tool_search_call", "arguments": {"query": "quote"}});
        let mut tagged = search.clone();
        tagged["_tickloop_codex_origin"] = serde_json::json!(origin);
        assert_eq!(
            TOKEN_COUNTER.protocol_data_tokens(Protocol::Responses, &tagged),
            TOKEN_COUNTER.text_tokens(&search.to_string())
        );
        let google = serde_json::json!({"part": {"text": "done", "thoughtSignature": "opaque"}, "paired": true});
        assert_eq!(
            TOKEN_COUNTER.protocol_data_tokens(Protocol::Google, &google),
            0
        );
        assert!(is_reasoning(&Content::Thinking {
            thinking: String::new(),
            signature: Some("sealed".into()),
        }));
        assert!(!is_reasoning(&Content::Text {
            text: "done".into()
        }));
    }

    #[test]
    fn a_message_counts_its_content_and_framing_and_repeats_from_the_cache() {
        let message = user("hello world");
        let counted = TOKEN_COUNTER.message_tokens(&message);
        assert_eq!(counted, TOKEN_COUNTER.text_tokens("hello world") + 3);
        assert_eq!(TOKEN_COUNTER.message_tokens(&message), counted);
        let definition = ToolDefinition {
            name: "Read".into(),
            description: "Reads a file".into(),
            input_schema: serde_json::json!({"type": "object"}),
        };
        assert!(TOKEN_COUNTER.definition_tokens(&definition) >= 5);
    }
}
