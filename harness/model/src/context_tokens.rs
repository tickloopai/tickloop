//! Context token counting with the open-source `o200k_base` BPE vocabulary, embedded in the
//! binary. One tokenizer stands in for every provider: counts are estimates that a service's own
//! usage calibrates.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Mutex;

use tickloop_harness_protocol::{ContextTokenCounter, Message, message_content_tokens};

pub const TOKENIZER_NAME: &str = "o200k_base";
/// Counted messages kept before the cache starts over; a long Chat stays well inside it.
const MESSAGE_CACHE_ENTRIES: usize = 16_384;

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
    use tickloop_harness_protocol::{Content, Role, ToolDefinition};

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
