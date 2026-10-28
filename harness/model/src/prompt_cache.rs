//! Anthropic prompt caching on the Messages wire: where a request's `cache_control` breakpoints
//! go.
//!
//! The service caches the request prefix in the order tools, system, messages, up to each
//! breakpoint; a later request reads the longest cached prefix it repeats byte for byte, looking
//! back at most 20 positions from each of its breakpoints, and pays the cache write only for what
//! lies past that read. A request carries at most four breakpoints. (Anthropic, "Prompt caching":
//! https://platform.claude.com/docs/en/build-with-claude/prompt-caching)
//!
//! The breakpoints, at most one each:
//!
//! 1. The last system block. It caches the listed tools and the system prompt, the part that
//!    survives a change of thinking or effort, which drops the cache of the messages only. Claude
//!    Code marks its system prompt blocks the same way (`buildSystemPromptBlocks`). Tools carry no
//!    marker of their own: they come before the system prompt, and the deferred ones
//!    (`defer_loading`) are left out of the cached prompt altogether, so a tool search loading
//!    them changes no cached byte (Claude Code leaves them out of its cache break detection for
//!    that reason).
//! 2. The last block of the first message: in a conversation request the user context message,
//!    which leads every request of a Chat. With the tools and system prompt it is the Chat's
//!    stable prefix, so the first request after a compaction, which rewrites everything after it,
//!    still reads it.
//! 3. The last block of the request: it moves forward with every request and writes the whole
//!    conversation, which the next request of the tool loop reads. Claude Code places exactly this
//!    one message marker (`addCacheBreakpoints`).
//! 4. The last block before the latest assistant message: where the previous request put its
//!    third breakpoint. Claude Code relies on the 20 position lookback alone; a turn that adds
//!    more positions than that (long runs of text and image blocks) would miss the previous
//!    write, and this marker reads it exactly.
//!
//! Requests without tools (titles, summaries, reviews) are one-shot: no later request repeats
//! their messages, so they mark only the system prompt and pay no write for the rest.
//!
//! A breakpoint goes only on a block the service accepts one on (text, image, document, tool use
//! and tool result); thinking, an empty text block and server tool blocks move it to the block
//! before, as Claude Code moves past a message that ends in thinking. Images need nothing more:
//! a request sends the same image bytes every time. Every breakpoint keeps the default lifetime
//! of five minutes: Claude Code gives API key users that lifetime, keeping the one-hour one for
//! subscription accounts' main thread, and a tool loop's requests follow each other well within
//! it, each read renewing it. Subagents are Chats of their own and place theirs the same way.

use std::{collections::BTreeSet, ops::Range};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The prompt caching a Provider's Messages endpoint accepts.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptCache {
    /// Anthropic's explicit `cache_control` breakpoints.
    Anthropic,
}

const MAX_BREAKPOINTS: usize = 4;

/// Marks the breakpoints on an encoded Messages request `body`. `continues` says whether later
/// requests repeat this one's messages, as a request offering tools does in the tool loop.
pub(crate) fn place_breakpoints(body: &mut Value, continues: bool) {
    let mut placed = 0;
    if let Some(system) = body["system"].as_array_mut()
        && let Some(last) = system.last_mut()
        && is_cacheable(last)
    {
        mark(last);
        placed += 1;
    }
    if !continues {
        return;
    }
    let Some(messages) = body["messages"].as_array_mut() else {
        return;
    };
    let latest_assistant = messages
        .iter()
        .rposition(|message| message["role"] == "assistant");
    let breakpoints: BTreeSet<(usize, usize)> = [
        (messages.len() > 1)
            .then(|| last_cacheable(messages, 0..1))
            .flatten(),
        last_cacheable(messages, 0..messages.len()),
        latest_assistant.and_then(|assistant| last_cacheable(messages, 0..assistant)),
    ]
    .into_iter()
    .flatten()
    .collect();
    debug_assert!(placed + breakpoints.len() <= MAX_BREAKPOINTS);
    for (message, block) in breakpoints {
        mark(&mut messages[message]["content"][block]);
    }
}

/// The last block in `messages[range]` that can carry a breakpoint, as (message, block).
fn last_cacheable(messages: &[Value], range: Range<usize>) -> Option<(usize, usize)> {
    range.rev().find_map(|message| {
        messages[message]["content"]
            .as_array()?
            .iter()
            .rposition(is_cacheable)
            .map(|block| (message, block))
    })
}

fn is_cacheable(block: &Value) -> bool {
    match block["type"].as_str() {
        Some("text") => block["text"]
            .as_str()
            .is_some_and(|text| !text.trim().is_empty()),
        Some("image" | "document" | "tool_use" | "tool_result") => true,
        _ => false,
    }
}

fn mark(block: &mut Value) {
    block["cache_control"] = json!({"type": "ephemeral"});
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(text: &str) -> Value {
        json!({"type": "text", "text": text})
    }

    fn marked(mut block: Value) -> Value {
        mark(&mut block);
        block
    }

    fn breakpoints(body: &Value) -> usize {
        body.to_string().matches("cache_control").count()
    }

    #[test]
    fn a_request_without_tools_marks_only_its_system_prompt() {
        let mut body = json!({
            "system": [text("Summarize.")],
            "messages": [{"role": "user", "content": [text("Long transcript")]}],
        });
        place_breakpoints(&mut body, false);
        assert_eq!(body["system"], json!([marked(text("Summarize."))]));
        assert_eq!(breakpoints(&body), 1);
    }

    #[test]
    fn thinking_empty_text_and_server_tool_blocks_move_the_breakpoint_back() {
        let mut body = json!({"messages": [
            {"role": "user", "content": [text("context")]},
            {"role": "user", "content": [text("prompt")]},
            {"role": "assistant", "content": [
                text("Searching"),
                {"type": "server_tool_use", "id": "s", "name": "tool_search_tool_bm25", "input": {}},
                {"type": "thinking", "thinking": "", "signature": "x"},
                text(" "),
            ]},
        ]});
        place_breakpoints(&mut body, true);
        assert_eq!(body["messages"][2]["content"][0], marked(text("Searching")));
        assert_eq!(body["messages"][1]["content"][0], marked(text("prompt")));
        assert_eq!(body["messages"][0]["content"][0], marked(text("context")));
        assert_eq!(breakpoints(&body), 3);
    }

    #[test]
    fn a_single_message_takes_one_breakpoint() {
        let mut body = json!({
            "system": [text("system")],
            "messages": [{"role": "user", "content": [text("only")]}],
        });
        place_breakpoints(&mut body, true);
        assert_eq!(body["messages"][0]["content"][0], marked(text("only")));
        assert_eq!(breakpoints(&body), 2);
    }
}
