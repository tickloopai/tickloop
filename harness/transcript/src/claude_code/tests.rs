use serde_json::json;
use tickloop_harness_protocol::{Content, Message, Role, ToolResultBlock};

use super::*;
use crate::{CustomTitleEntry, UnknownLine};

const CHAT: Uuid = Uuid::from_u128(0x0198_0a1b_2c3d_7000_8000_0000_0000_0001);

fn entry(parent: Option<Uuid>, role: Role, content: Vec<Content>) -> ChainEntry {
    serde_json::from_value(json!({
        "uuid": Uuid::new_v4(), "parentUuid": parent, "chatId": CHAT,
        "timestamp": "2026-10-29T00:00:00.000Z", "cwd": "/work", "version": "test",
        "message": Message { role, content }, "humanFamilyOrigin": null,
    }))
    .unwrap()
}

fn text(text: &str) -> Content {
    Content::Text { text: text.into() }
}

fn tool_use(id: &str) -> Content {
    Content::ToolUse {
        id: id.into(),
        name: "Bash".into(),
        input: json!({"command": "ls"}),
    }
}

fn tool_result(id: &str, output: &str) -> Content {
    Content::ToolResult {
        tool_use_id: id.into(),
        content: output.into(),
        is_error: false,
    }
}

fn thinking(text: &str) -> Content {
    Content::Thinking {
        thinking: text.into(),
        signature: Some("signed".into()),
    }
}

fn jsonl(lines: &[ClaudeCodeLine]) -> String {
    lines
        .iter()
        .map(|line| serde_json::to_string(line).unwrap() + "\n")
        .collect()
}

/// What ralph-loop's Stop hook reads from `transcript_path`: the lines containing
/// `"role":"assistant"` (`grep`), the last 100 of them (`tail`), and the last text block among
/// them (`jq -rs 'map(.message.content[]? | select(.type == "text") | .text) | last // ""'`).
fn ralph_loop_last_assistant_text(file: &str) -> String {
    let assistant: Vec<&str> = file
        .lines()
        .filter(|line| line.contains(r#""role":"assistant""#))
        .collect();
    assistant[assistant.len().saturating_sub(100)..]
        .iter()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .flat_map(|line| {
            line["message"]["content"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str().map(str::to_owned))
        .last()
        .unwrap_or_default()
}

/// A prompt, an answer that thinks, calls a tool and gets its result, then a final answer.
fn conversation() -> (Vec<Line>, ChainEntry) {
    let prompt = entry(None, Role::User, vec![text("Fix the bug")]);
    let call = entry(
        Some(prompt.uuid),
        Role::Assistant,
        vec![
            thinking("Look first."),
            text("Let me look."),
            tool_use("call-1"),
        ],
    );
    let result = entry(
        Some(call.uuid),
        Role::User,
        vec![tool_result("call-1", "main.rs")],
    );
    let reply = entry(
        Some(result.uuid),
        Role::Assistant,
        vec![text("Fixed. <promise>DONE</promise>")],
    );
    (
        vec![
            Line::User(prompt),
            Line::Assistant(call),
            Line::User(result),
            Line::Assistant(reply.clone()),
        ],
        reply,
    )
}

#[test]
fn each_content_block_is_one_claude_code_line() {
    let (lines, _) = conversation();
    let projected = project(&lines);
    let shapes: Vec<serde_json::Value> = projected
        .iter()
        .map(|line| {
            let value = serde_json::to_value(line).unwrap();
            json!([
                value["type"],
                value["message"]["role"],
                value["message"]["content"]
            ])
        })
        .collect();
    assert_eq!(
        shapes,
        [
            json!(["user", "user", [{"type": "text", "text": "Fix the bug"}]]),
            json!(["assistant", "assistant", [{"type": "text", "text": "Let me look."}]]),
            json!(["assistant", "assistant",
                [{"type": "tool_use", "id": "call-1", "name": "Bash", "input": {"command": "ls"}}]]),
            json!(["user", "user",
                [{"type": "tool_result", "tool_use_id": "call-1", "content": "main.rs", "is_error": false}]]),
            json!(["assistant", "assistant",
                [{"type": "text", "text": "Fixed. <promise>DONE</promise>"}]]),
        ]
    );
    let first = serde_json::to_value(&projected[0]).unwrap();
    assert_eq!(first["sessionId"], json!(CHAT));
    assert_eq!(first["cwd"], "/work");
    assert_eq!(first["isSidechain"], false);
    assert_eq!(first["timestamp"], "2026-10-29T00:00:00.000Z");
}

#[test]
fn lines_chain_through_their_blocks_and_have_their_own_ids() {
    let (lines, _) = conversation();
    let projected = project(&lines);
    let Line::User(prompt) = &lines[0] else {
        unreachable!()
    };
    assert_eq!(projected[0].uuid, prompt.uuid);
    assert_eq!(projected[0].parent_uuid, None);
    for pair in projected.windows(2) {
        assert_eq!(pair[1].parent_uuid, Some(pair[0].uuid));
    }
    let mut ids: Vec<Uuid> = projected.iter().map(|line| line.uuid).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), projected.len());
    assert_eq!(
        project(&lines),
        projected,
        "the projection is deterministic"
    );
}

#[test]
fn ralph_loop_reads_the_last_assistant_text() {
    let (lines, reply) = conversation();
    let file = jsonl(&project(&lines));
    assert_eq!(
        ralph_loop_last_assistant_text(&file),
        "Fixed. <promise>DONE</promise>"
    );

    // A last answer that only calls tools leaves the text before it as the last one.
    let calls_only = entry(Some(reply.uuid), Role::Assistant, vec![tool_use("call-2")]);
    let mut more = lines.clone();
    more.push(Line::Assistant(calls_only));
    assert_eq!(
        ralph_loop_last_assistant_text(&jsonl(&project(&more))),
        "Fixed. <promise>DONE</promise>"
    );
}

#[test]
fn only_what_the_conversation_shows_the_model_is_projected() {
    let prompt = entry(None, Role::User, vec![text("hello")]);
    let mut note = entry(
        Some(prompt.uuid),
        Role::User,
        vec![text("<system-reminder>host state</system-reminder>")],
    );
    note.is_meta = true;
    let mut boundary = entry(
        Some(note.uuid),
        Role::System,
        vec![text("Conversation compacted")],
    );
    boundary.subtype = Some(crate::COMPACT_BOUNDARY_SUBTYPE.into());
    let mut transcript_only = entry(Some(boundary.uuid), Role::User, vec![text("shown only")]);
    transcript_only.is_visible_in_transcript_only = true;
    let image = Content::ToolResult {
        tool_use_id: "call-1".into(),
        content: tickloop_harness_protocol::ToolResultContent::Blocks(vec![
            ToolResultBlock::Text {
                text: "screenshot taken".into(),
            },
            ToolResultBlock::Image {
                source: tickloop_harness_protocol::ImageSource::Base64 {
                    media_type: tickloop_harness_protocol::ImageMediaType::Png,
                    data: "iVBORw0KGgo=".into(),
                },
                width: 1,
                height: 1,
            },
        ]),
        is_error: true,
    };
    let answer = entry(
        Some(transcript_only.uuid),
        Role::Assistant,
        vec![
            thinking("private reasoning"),
            Content::ProtocolData {
                protocol: tickloop_harness_protocol::Protocol::Responses,
                value: json!({"type": "reasoning", "summary": [{"text": "hidden"}]}),
            },
            text("answer"),
        ],
    );
    let result = entry(Some(answer.uuid), Role::User, vec![image]);
    let lines = vec![
        Line::CustomTitle(CustomTitleEntry {
            chat_id: CHAT,
            custom_title: "a title".into(),
        }),
        Line::User(prompt.clone()),
        Line::User(note),
        Line::System(boundary),
        Line::User(transcript_only),
        Line::Assistant(answer),
        Line::User(result),
        Line::Unknown(UnknownLine {
            kind: "future-record".into(),
            value: json!({"type": "future-record", "secret": "host only"}),
        }),
    ];
    let projected = project(&lines);
    let blocks: Vec<&ClaudeCodeBlock> = projected
        .iter()
        .map(|line| &line.message.content[0])
        .collect();
    assert_eq!(
        blocks,
        [
            &ClaudeCodeBlock::Text {
                text: "hello".into()
            },
            &ClaudeCodeBlock::Text {
                text: "answer".into()
            },
            &ClaudeCodeBlock::ToolResult {
                tool_use_id: "call-1".into(),
                content: "screenshot taken".into(),
                is_error: true,
            },
        ]
    );
    // A line skipped in between leaves the next one attached to the last projected line.
    assert_eq!(projected[1].parent_uuid, Some(prompt.uuid));
    let file = jsonl(&projected);
    for private in [
        "host state",
        "private reasoning",
        "hidden",
        "a title",
        "host only",
    ] {
        assert!(!file.contains(private), "{private} leaked into {file}");
    }
}

#[test]
fn applying_lines_again_adds_nothing_and_new_lines_add_only_themselves() {
    let (lines, reply) = conversation();
    let mut projector = ClaudeCodeProjector::new();
    let first: Vec<ClaudeCodeLine> = lines
        .iter()
        .flat_map(|line| projector.apply(line))
        .collect();
    assert_eq!(first.len(), 5);
    let next = entry(Some(reply.uuid), Role::User, vec![text("and the tests?")]);
    let mut grown = lines.clone();
    grown.push(Line::User(next));
    let added: Vec<ClaudeCodeLine> = grown
        .iter()
        .flat_map(|line| projector.apply(line))
        .collect();
    assert_eq!(
        added
            .iter()
            .map(|line| &line.message.content[0])
            .collect::<Vec<_>>(),
        [&ClaudeCodeBlock::Text {
            text: "and the tests?".into()
        }]
    );
    assert_eq!(added[0].parent_uuid, first.last().map(|line| line.uuid));
}

/// A compaction writes a boundary, a summary and copies of the messages it keeps; the copies
/// stand for lines already projected, while fragments appended to a copy later are new.
#[test]
fn a_compaction_adds_its_summary_but_not_the_copies_of_kept_messages() {
    let (lines, reply) = conversation();
    let mut boundary = entry(None, Role::System, vec![text("Conversation compacted")]);
    boundary.subtype = Some(crate::COMPACT_BOUNDARY_SUBTYPE.into());
    boundary.logical_parent_uuid = Some(reply.uuid);
    let mut summary = entry(
        Some(boundary.uuid),
        Role::User,
        vec![text(
            "This session is being continued. Summary: fixed the bug.",
        )],
    );
    summary.is_compact_summary = true;
    let mut kept = entry(
        Some(summary.uuid),
        Role::Assistant,
        reply.message.content.clone(),
    );
    kept.original_message_id = Some(reply.uuid);
    let mut appended = entry(Some(kept.uuid), Role::Assistant, vec![tool_use("call-9")]);
    appended.original_message_id = Some(reply.uuid);
    appended.continues_message = true;
    let mut compacted = lines.clone();
    compacted.extend([
        Line::System(boundary),
        Line::User(summary),
        Line::Assistant(kept),
        Line::Assistant(appended),
    ]);
    let before = project(&lines);
    let after = project(&compacted);
    assert_eq!(after[..before.len()], before[..]);
    assert_eq!(
        after[before.len()..]
            .iter()
            .map(|line| &line.message.content[0])
            .collect::<Vec<_>>(),
        [
            &ClaudeCodeBlock::Text {
                text: "This session is being continued. Summary: fixed the bug.".into()
            },
            &ClaudeCodeBlock::ToolUse {
                id: "call-9".into(),
                name: "Bash".into(),
                input: json!({"command": "ls"}),
            },
        ]
    );
    assert_eq!(
        ralph_loop_last_assistant_text(&jsonl(&after)),
        "Fixed. <promise>DONE</promise>"
    );
}

/// After a reload the store may give a compaction's copy without its source: the copy is then
/// the first time the message is seen.
#[test]
fn a_copy_whose_source_was_never_seen_is_projected_once() {
    let source = Uuid::new_v4();
    let mut first = entry(None, Role::Assistant, vec![text("kept answer")]);
    first.original_message_id = Some(source);
    let mut second = entry(None, Role::Assistant, vec![text("kept answer")]);
    second.original_message_id = Some(source);
    let projected = project(&[Line::Assistant(first), Line::Assistant(second)]);
    assert_eq!(projected.len(), 1);
}
