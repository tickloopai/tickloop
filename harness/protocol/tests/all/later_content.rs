//! Content a later version wrote: a model reads a placeholder in its place, and a call it made
//! still pairs with its result.

use serde_json::json;
use tickloop_harness_protocol::{
    ChatSnapshot, Content, LATER_CONTENT_TEXT, Message, ToolResultBlock, ToolResultContent,
    projection::history_calls_in, without_later_content,
};

fn placeholder() -> Content {
    Content::Text {
        text: LATER_CONTENT_TEXT.into(),
    }
}

fn later_conversation() -> Vec<Message> {
    serde_json::from_value(json!([
        {"role": "user", "content": [
            {"type": "text", "text": "Look"},
            {"type": "image", "source": {"type": "later_source"}, "width": 1, "height": 1},
        ]},
        {"role": "assistant", "content": [
            {"type": "later_call", "id": "later-1", "name": "Map"},
            {"type": "tool_use", "id": "known-1", "name": "Read", "input": {}},
        ]},
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "later-1", "content": "map result", "is_error": false},
            {"type": "tool_result", "tool_use_id": "known-1", "content": [
                {"type": "text", "text": "file"},
                {"type": "later_result_block"},
            ], "is_error": false},
        ]},
    ]))
    .unwrap()
}

#[test]
fn a_model_reads_a_placeholder_for_later_content_and_its_calls_results() {
    let mut messages = later_conversation();
    without_later_content(&mut messages);
    assert_eq!(messages[0].content[1], placeholder());
    assert_eq!(messages[1].content[0], placeholder());
    assert!(matches!(messages[1].content[1], Content::ToolUse { .. }));
    assert_eq!(messages[2].content[0], placeholder());
    assert_eq!(
        messages[2].content[1],
        Content::ToolResult {
            tool_use_id: "known-1".into(),
            content: ToolResultContent::Blocks(vec![
                ToolResultBlock::Text {
                    text: "file".into()
                },
                ToolResultBlock::Text {
                    text: LATER_CONTENT_TEXT.into()
                },
            ]),
            is_error: false,
        }
    );
}

#[test]
fn the_result_of_a_later_call_leaves_the_conversation_valid() {
    let snapshot: ChatSnapshot = serde_json::from_value(json!({
        "id": uuid::Uuid::from_u128(1),
        "chat_loop": {"id": uuid::Uuid::from_u128(2), "state": "Idle"},
        "messages": later_conversation(),
    }))
    .unwrap();
    let calls = history_calls_in(&snapshot, 0..snapshot.messages.len()).unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].call_id, "known-1");
    assert!(calls[0].result.is_some());
}
