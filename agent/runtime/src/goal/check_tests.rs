use super::*;
use serde_json::Value;
use tickloop_agent_protocol::{ToolResultContent, Usage};

const OBJECTIVE: &str = "Ship the export command with tests";
const REPORT: &str = "cargo test export passed";

fn payload(request: &ModelRequest) -> Value {
    let Content::Text { text } = &request.messages[1].content[0] else {
        panic!("check payload is text")
    };
    serde_json::from_str(text).expect("check payload is JSON")
}

fn system_prompt(request: &ModelRequest) -> &str {
    let Content::Text { text } = &request.messages[0].content[0] else {
        panic!("check system prompt is text")
    };
    text
}

fn output(text: &str) -> ModelOutput {
    ModelOutput {
        private_source: None,
        content: vec![Content::Text { text: text.into() }],
        usage: Usage::default(),
        stop_reason: "end_turn".into(),
    }
}

fn agent_text(text: impl Into<String>) -> EvidenceEntry {
    EvidenceEntry::Agent { text: text.into() }
}

#[test]
fn conversation_evidence_keeps_text_tool_calls_and_named_results() {
    let messages = vec![
        Message {
            role: Role::System,
            content: vec![Content::Text {
                text: "system".into(),
            }],
        },
        Message {
            role: Role::User,
            content: vec![Content::Text {
                text: "Add an export command".into(),
            }],
        },
        Message {
            role: Role::Assistant,
            content: vec![
                Content::Thinking {
                    thinking: "private".into(),
                    signature: None,
                },
                Content::Text { text: " ".into() },
                Content::ToolUse {
                    id: "call-1".into(),
                    name: "Bash".into(),
                    input: json!({"command": "cargo test export"}),
                },
            ],
        },
        Message {
            role: Role::User,
            content: vec![Content::ToolResult {
                tool_use_id: "call-1".into(),
                content: ToolResultContent::Text("test result: ok".into()),
                is_error: false,
            }],
        },
    ];

    assert_eq!(
        conversation_evidence(&messages),
        [
            EvidenceEntry::User {
                text: "Add an export command".into(),
            },
            EvidenceEntry::ToolCall {
                tool: "Bash".into(),
                input: r#"{"command":"cargo test export"}"#.into(),
            },
            EvidenceEntry::ToolResult {
                tool: Some("Bash".into()),
                is_error: false,
                output: "test result: ok".into(),
            },
        ]
    );
}

#[test]
fn request_carries_objective_report_and_conversation_without_tools() {
    let request = check_request(
        OBJECTIVE,
        REPORT,
        vec![agent_text("All export tests pass")],
        64 * 1024,
    );

    assert!(request.tools.is_empty());
    assert_eq!(request.messages[0].role, Role::System);
    assert!(system_prompt(&request).contains("Do not assume results that are not shown"));
    assert!(!system_prompt(&request).contains("conversation_truncation"));
    assert_eq!(
        request
            .structured_output
            .as_ref()
            .map(|output| &output.name),
        Some(&CHECK_RESULT_NAME.to_owned())
    );
    assert_eq!(
        payload(&request),
        json!({
            "objective": OBJECTIVE,
            "completion_report": REPORT,
            "conversation": [{"kind": "agent", "text": "All export tests pass"}],
        })
    );
}

#[test]
fn oversized_entries_lose_their_middle_and_are_marked() {
    let long = format!("{}{}{}", "head ", "x".repeat(40_000), " tail");
    let budget = 16 * 1024;
    let request = check_request(OBJECTIVE, REPORT, vec![agent_text(long)], budget);

    let payload = payload(&request);
    let text = payload["conversation"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("head "));
    assert!(text.ends_with(" tail"));
    assert!(text.contains("bytes omitted by the host to fit the completion check"));
    assert_eq!(payload["conversation_truncation"]["shortened"], 1);
    assert_eq!(payload["conversation_truncation"]["omitted_oldest"], 0);
    assert!(system_prompt(&request).contains("conversation_truncation"));
    let Content::Text { text } = &request.messages[1].content[0] else {
        unreachable!()
    };
    assert!(text.len() <= budget);
}

#[test]
fn oldest_entries_are_omitted_first_and_counted() {
    let entries = (0..40)
        .map(|index| agent_text(format!("{index:02} {}", "y".repeat(1000))))
        .collect::<Vec<_>>();
    let budget = 12 * 1024;
    let request = check_request(OBJECTIVE, REPORT, entries, budget);

    let payload = payload(&request);
    let conversation = payload["conversation"].as_array().unwrap();
    let omitted = payload["conversation_truncation"]["omitted_oldest"]
        .as_u64()
        .unwrap() as usize;
    assert!(omitted > 0);
    assert_eq!(conversation.len() + omitted, 40);
    assert!(
        conversation.last().unwrap()["text"]
            .as_str()
            .unwrap()
            .starts_with("39 ")
    );
    assert!(
        conversation[0]["text"]
            .as_str()
            .unwrap()
            .starts_with(&format!("{omitted:02} "))
    );
    assert_eq!(payload["conversation_truncation"]["shortened"], 0);
}

#[test]
fn cutting_respects_character_boundaries() {
    let mut text = "界".repeat(2000);
    let removed = cut_middle(&mut text, 3000);
    assert!(removed >= 3000);
    assert!(text.contains("bytes omitted"));
    assert!(text.starts_with('界') && text.ends_with('界'));
}

#[test]
fn reply_parses_into_the_three_verdicts() {
    assert_eq!(
        parse_check_reply(&output(r#"{"verdict":"met","reason":"tests pass"}"#)),
        Ok(CheckVerdict::Met)
    );
    assert_eq!(
        parse_check_reply(&output(
            r#"{"verdict":"not_met","reason":" No test output shown "}"#
        )),
        Ok(CheckVerdict::NotMet {
            reason: "No test output shown".into(),
        })
    );
    assert_eq!(
        parse_check_reply(&output(
            "```json\n{\"verdict\":\"impossible\",\"reason\":\"The API was removed\"}\n```"
        )),
        Ok(CheckVerdict::Impossible {
            reason: "The API was removed".into(),
        })
    );
}

#[test]
fn malformed_replies_are_errors() {
    for (text, error) in [
        ("met", "completion check returned an invalid verdict"),
        (
            r#"{"verdict":"done","reason":"x"}"#,
            "completion check returned an invalid verdict",
        ),
        (
            r#"{"verdict":"met"}"#,
            "completion check returned an invalid verdict",
        ),
        (
            r#"{"verdict":"met","reason":"x","extra":1}"#,
            "completion check returned an invalid verdict",
        ),
        (
            r#"{"verdict":"not_met","reason":"  "}"#,
            "completion check gave no reason",
        ),
        (
            "```yaml\nverdict: met\n```",
            "completion check returned an unsupported fence",
        ),
        (
            "```json\n{\"verdict\":\"met\",\"reason\":\"x\"}",
            "completion check returned an unfinished JSON fence",
        ),
    ] {
        assert_eq!(parse_check_reply(&output(text)), Err(error), "{text}");
    }

    let mut unfinished = output(r#"{"verdict":"met","reason":"x"}"#);
    unfinished.stop_reason = "max_tokens".into();
    assert_eq!(
        parse_check_reply(&unfinished),
        Err("completion check response did not finish")
    );

    let mut tool_call = output(r#"{"verdict":"met","reason":"x"}"#);
    tool_call.content.push(Content::ToolUse {
        id: "call".into(),
        name: "Bash".into(),
        input: json!({}),
    });
    assert_eq!(
        parse_check_reply(&tool_call),
        Err("completion check returned a tool operation")
    );
}
