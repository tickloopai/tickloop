use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const MAX_THIRD_PARTY_INPUT_BYTES: usize = 64 * 1024;
const MAX_EXCERPT_COLLECTION_ITEMS: usize = 64;
const MAX_EXCERPT_DEPTH: usize = 8;
const EXCERPT_METADATA_RESERVE_BYTES: usize = 4 * 1024;
const MIN_EXCERPT_TEXT_BYTES: usize = 128;
const FALLBACK_JSON_EXCERPT_BYTES: usize = 8 * 1024;
const EXCERPT_KIND: &str = "bounded_third_party_input";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputPurpose {
    Proposed,
    Historical,
}

/// Projects review evidence only; execution and deterministic rules use the original input.
pub(crate) fn project_tool_input(
    tool: &str,
    input: &Value,
    purpose: InputPurpose,
) -> Option<Value> {
    if !has_dedicated_projection(tool) {
        let encoded = serde_json::to_vec(input).expect("tool input serializes");
        return Some(if encoded.len() > MAX_THIRD_PARTY_INPUT_BYTES {
            bounded_third_party_input(input, &encoded)
        } else {
            input.clone()
        });
    }
    let Some(fields) = input.as_object() else {
        return Some(input.clone());
    };
    if matches!(tool, "ListSubagents" | "EnterPlanMode") && fields.is_empty() {
        return None;
    }
    if tool == "ArtifactEdit" && purpose == InputPurpose::Historical {
        return Some(artifact_history(input));
    }
    if tool == "TodoWrite"
        && fields.len() == 1
        && let Some(todos) = input["todos"].as_array()
    {
        return Some(json!({"items": todos.len()}));
    }
    if tool == "AskUserQuestion"
        && fields.len() == 1
        && let Some(questions) = input["questions"].as_array()
        && let Some(texts) = questions
            .iter()
            .map(|question| question.get("question")?.as_str())
            .collect::<Option<Vec<_>>>()
    {
        return Some(json!({"questions": texts}));
    }
    let required_text = match tool {
        "Bash" => Some("command"),
        "Read" => Some("file_path"),
        "Glob" | "Grep" => Some("pattern"),
        "Edit" => Some("new_string"),
        "NotebookEdit" => Some("new_source"),
        "WebSearch" => Some("query"),
        "Subagent" => Some("task"),
        "Agent" => Some("prompt"),
        "Skill" => Some("skill"),
        _ => None,
    };
    if required_text.is_some_and(|field| !input[field].is_string()) {
        return Some(input.clone());
    }
    let omitted: &[&str] = match tool {
        "Bash" => &["description", "timeout", "run_in_background"],
        "Read" => &["offset", "limit", "pages"],
        "Glob" => &["path"],
        "Grep" => &[
            "output_mode",
            "glob",
            "type",
            "-A",
            "-B",
            "-C",
            "-n",
            "-i",
            "multiline",
            "head_limit",
            "offset",
        ],
        "Edit" => &["old_string", "replace_all"],
        "NotebookEdit" => &["cell_id", "cell_type"],
        "WebSearch" => &["allowed_domains", "blocked_domains"],
        "Subagent" => &["description"],
        "Agent" => &["description", "name", "resume", "run_in_background"],
        "TaskOutput" => &["block", "timeout"],
        "Skill" => &["args"],
        _ => &[],
    };
    let mut projected = fields.clone();
    for field in omitted {
        projected.remove(*field);
    }
    Some(Value::Object(projected))
}

fn has_dedicated_projection(tool: &str) -> bool {
    matches!(
        tool,
        "Bash"
            | "Read"
            | "Glob"
            | "Grep"
            | "Write"
            | "Edit"
            | "NotebookEdit"
            | "WebFetch"
            | "WebSearch"
            | "Agent"
            | "SendMessage"
            | "TaskStop"
            | "TaskOutput"
            | "AskUserQuestion"
            | "Skill"
            | "TodoWrite"
            | "ReadWorkspace"
            | "TickRead"
            | "TickCall"
            | "TickManage"
            | "StartGoal"
            | "ListSubagents"
            | "Subagent"
            | "SubagentHandback"
            | "StopSubagent"
            | "EnterPlanMode"
            | "ExitPlanMode"
            | "ArtifactRead"
            | "ArtifactEdit"
            | "ArtifactComment"
            | "ArtifactLogin"
            | "ArtifactUpload"
    )
}

pub(crate) fn is_incomplete(input: &Value) -> bool {
    input["review_projection"]["kind"] == EXCERPT_KIND
        && input["review_projection"]["complete"] == false
}

fn bounded_third_party_input(input: &Value, encoded: &[u8]) -> Value {
    let metadata = json!({
        "kind": EXCERPT_KIND,
        "complete": false,
        "original_json_bytes": encoded.len(),
        "original_json_sha256": format!("{:x}", Sha256::digest(encoded)),
        "notice": "Excerpts retain field structure and text boundaries, not all semantics. Omitted text is unavailable evidence, never proof of safety or authorization."
    });
    let mut text_budget = MAX_THIRD_PARTY_INPUT_BYTES - EXCERPT_METADATA_RESERVE_BYTES;
    loop {
        let projected = json!({
            "review_projection": metadata,
            "input": excerpt_value(input, text_budget, 0),
        });
        if serde_json::to_vec(&projected)
            .expect("projection serializes")
            .len()
            <= MAX_THIRD_PARTY_INPUT_BYTES
        {
            return projected;
        }
        if text_budget <= MIN_EXCERPT_TEXT_BYTES {
            return json!({
                "review_projection": metadata,
                "input_json_excerpt": text_excerpt(std::str::from_utf8(encoded).expect("JSON is UTF-8"), FALLBACK_JSON_EXCERPT_BYTES),
            });
        }
        text_budget /= 2;
    }
}

fn excerpt_value(input: &Value, text_budget: usize, depth: usize) -> Value {
    if depth >= MAX_EXCERPT_DEPTH {
        return json!({"omitted_nested_value": true});
    }
    match input {
        Value::String(text) if text.len() > text_budget => text_excerpt(text, text_budget),
        Value::Object(fields) => {
            let excerpt = fields
                .iter()
                .take(MAX_EXCERPT_COLLECTION_ITEMS)
                .map(|(key, value)| (key.clone(), excerpt_value(value, text_budget, depth + 1)))
                .collect::<serde_json::Map<_, _>>();
            if fields.len() > MAX_EXCERPT_COLLECTION_ITEMS {
                json!({"fields": excerpt, "omitted_field_count": fields.len() - MAX_EXCERPT_COLLECTION_ITEMS})
            } else {
                Value::Object(excerpt)
            }
        }
        Value::Array(items) => {
            let excerpt = items
                .iter()
                .take(MAX_EXCERPT_COLLECTION_ITEMS)
                .map(|value| excerpt_value(value, text_budget, depth + 1))
                .collect::<Vec<_>>();
            if items.len() > MAX_EXCERPT_COLLECTION_ITEMS {
                json!({"items": excerpt, "omitted_item_count": items.len() - MAX_EXCERPT_COLLECTION_ITEMS})
            } else {
                Value::Array(excerpt)
            }
        }
        value => value.clone(),
    }
}

fn text_excerpt(text: &str, budget: usize) -> Value {
    let mut prefix_end = text.floor_char_boundary((budget / 2).min(text.len()));
    let mut suffix_start = text.ceil_char_boundary(text.len().saturating_sub(budget / 2));
    // Prefer complete lines or paragraphs so an excerpt does not splice sentences together.
    if let Some(boundary) = text[..prefix_end].rfind('\n')
        && boundary > prefix_end / 2
    {
        prefix_end = boundary + 1;
    }
    if let Some(boundary) = text[suffix_start..].find('\n')
        && boundary < budget / 4
    {
        suffix_start += boundary + 1;
    }
    json!({
        "prefix": &text[..prefix_end],
        "suffix": &text[suffix_start..],
        "omitted_utf8_bytes": suffix_start.saturating_sub(prefix_end),
        "original_utf8_bytes": text.len(),
        "sha256": format!("{:x}", Sha256::digest(text.as_bytes())),
    })
}

fn artifact_history(input: &Value) -> Value {
    let body = match input["action"].as_str() {
        Some("create" | "replace") => {
            let Some(content) = input["content"].as_str() else {
                return input.clone();
            };
            content.as_bytes().to_vec()
        }
        Some("patch") => {
            let Some(patches) = input["patches"].as_array() else {
                return input.clone();
            };
            serde_json::to_vec(patches).expect("artifact patches serialize")
        }
        _ => return input.clone(),
    };
    let mut projected = input.as_object().expect("object checked by caller").clone();
    projected.remove("content");
    projected.remove("patches");
    projected.remove("title");
    projected.insert("content_bytes".into(), json!(body.len()));
    projected.insert(
        "content_sha256".into(),
        json!(format!("{:x}", Sha256::digest(&body))),
    );
    Value::Object(projected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_named_tools_retain_classifier_relevant_fields() {
        let cases = [
            (
                "Bash",
                json!({"command":"echo hi", "description":"friendly", "timeout":100, "run_in_background":true}),
                json!({"command":"echo hi"}),
            ),
            (
                "Read",
                json!({"file_path":"a", "offset":20,"limit":1,"pages":"1"}),
                json!({"file_path":"a"}),
            ),
            (
                "Glob",
                json!({"pattern":"*.rs", "path":"src"}),
                json!({"pattern":"*.rs"}),
            ),
            (
                "Grep",
                json!({"pattern":"needle","path":"src","output_mode":"content","-i":true,"glob":"*.rs","head_limit":2}),
                json!({"pattern":"needle","path":"src"}),
            ),
            (
                "Write",
                json!({"file_path":"a","content":"all code"}),
                json!({"file_path":"a","content":"all code"}),
            ),
            (
                "Edit",
                json!({"file_path":"a","old_string":"old","new_string":"new","replace_all":true}),
                json!({"file_path":"a","new_string":"new"}),
            ),
            (
                "NotebookEdit",
                json!({"notebook_path":"a.ipynb","edit_mode":"replace","new_source":"all code","cell_id":"1","cell_type":"code"}),
                json!({"notebook_path":"a.ipynb","edit_mode":"replace","new_source":"all code"}),
            ),
            (
                "WebFetch",
                json!({"url":"https://example.com","prompt":"find it"}),
                json!({"url":"https://example.com","prompt":"find it"}),
            ),
            (
                "WebSearch",
                json!({"query":"query","allowed_domains":["example.com"]}),
                json!({"query":"query"}),
            ),
            (
                "SendMessage",
                json!({"to":"worker","message":"do this"}),
                json!({"to":"worker","message":"do this"}),
            ),
            (
                "TaskStop",
                json!({"task_id":"task"}),
                json!({"task_id":"task"}),
            ),
            (
                "TaskOutput",
                json!({"task_id":"task"}),
                json!({"task_id":"task"}),
            ),
            (
                "AskUserQuestion",
                json!({"questions":[{"question":"Proceed?","header":"Confirm","options":[]}]}),
                json!({"questions":["Proceed?"]}),
            ),
            (
                "Skill",
                json!({"skill":"tickloop","args":"details"}),
                json!({"skill":"tickloop"}),
            ),
            (
                "TodoWrite",
                json!({"todos":[{"content":"task","status":"pending"}]}),
                json!({"items":1}),
            ),
        ];
        for (tool, input, expected) in cases {
            for purpose in [InputPurpose::Proposed, InputPurpose::Historical] {
                assert_eq!(
                    project_tool_input(tool, &input, purpose),
                    Some(expected.clone()),
                    "{tool}"
                );
            }
        }
    }

    #[test]
    fn tickloop_tools_keep_targets_execution_and_delegation_content() {
        let cases = [
            ("ReadWorkspace", json!({"workspace_id":"w"})),
            ("TickRead", json!({"tick_instance_id":"hosted:1"})),
            (
                "TickCall",
                json!({"tick_instance_id":"hosted:1","tool":"trade","args":{"size":100}}),
            ),
            (
                "TickManage",
                json!({"action":"open","command":["python","a.py"],"cwd":"/project"}),
            ),
            (
                "TickManage",
                json!({"action":"close","tick_instance_id":"hosted:1"}),
            ),
            (
                "TickManage",
                json!({"action":"send_input","tick_instance_id":"herdr:1","text":"all code","keys":["Enter"]}),
            ),
            ("StartGoal", json!({"goal":"keep watching"})),
            (
                "Subagent",
                json!({"task":"all instructions","run_in_background":true,"name":"worker"}),
            ),
            ("SubagentHandback", json!({"message":"all report"})),
            ("StopSubagent", json!({"to":"worker"})),
            (
                "ExitPlanMode",
                json!({"allowedPrompts":[{"tool":"Bash","prompt":"run tests"}]}),
            ),
            ("ArtifactRead", json!({"action":"read","artifact_id":"doc"})),
            (
                "ArtifactComment",
                json!({"action":"reply","artifact_id":"doc","thread_id":"thread","text":"all comment"}),
            ),
            (
                "ArtifactComment",
                json!({"action":"done","artifact_id":"doc","outcome":"done","summary":"all summary"}),
            ),
            (
                "ArtifactUpload",
                json!({"artifact_id":"slides","file_path":"/project/chart.png","content_type":"image/png","bytes":12,"sha256":"ab"}),
            ),
            (
                "mcp__unknown",
                json!({"nested":{"code":"all code"},"other":42}),
            ),
        ];
        for (tool, input) in cases {
            for purpose in [InputPurpose::Proposed, InputPurpose::Historical] {
                assert_eq!(
                    project_tool_input(tool, &input, purpose),
                    Some(input.clone()),
                    "{tool}"
                );
            }
        }
        for tool in ["ListSubagents", "EnterPlanMode"] {
            assert_eq!(
                project_tool_input(tool, &json!({}), InputPurpose::Historical),
                None
            );
            let invalid = json!({"unexpected":"keep evidence"});
            assert_eq!(
                project_tool_input(tool, &invalid, InputPurpose::Historical),
                Some(invalid)
            );
        }
    }

    #[test]
    fn artifact_history_hashes_utf8_but_proposed_content_is_complete() {
        for action in ["create", "replace", "patch"] {
            let input = match action {
                "create" => {
                    json!({"action":action,"format":"doc","title":"周报","content":"# 周报\n收入：100 元。"})
                }
                "replace" => {
                    json!({"action":action,"artifact_id":"doc","expected_revision":7,"content":"界".repeat(70 * 1024)})
                }
                _ => {
                    json!({"action":action,"artifact_id":"doc","patches":[{"type":"replace","target":"block","expected_hash":"hash","text":"新内容"}]})
                }
            };
            assert_eq!(
                project_tool_input("ArtifactEdit", &input, InputPurpose::Proposed),
                Some(input.clone())
            );
            let projected =
                project_tool_input("ArtifactEdit", &input, InputPurpose::Historical).unwrap();
            let body = input.get("content").map_or_else(
                || serde_json::to_vec(&input["patches"]).unwrap(),
                |content| content.as_str().unwrap().as_bytes().to_vec(),
            );
            assert_eq!(projected["content_bytes"], body.len());
            assert_eq!(
                projected["content_sha256"],
                format!("{:x}", Sha256::digest(body))
            );
            assert_eq!(projected["action"], input["action"]);
            assert_eq!(projected["artifact_id"], input["artifact_id"]);
            assert_eq!(projected["expected_revision"], input["expected_revision"]);
            assert!(projected.get("content").is_none());
            assert!(projected.get("patches").is_none());
        }
    }

    #[test]
    fn malformed_and_unknown_tools_do_not_become_empty_or_lose_evidence() {
        for (tool, input) in [
            ("UnknownTool", json!({"content":"all content"})),
            (
                "ArtifactEdit",
                json!({"action":"future","content":"all content"}),
            ),
            (
                "ArtifactEdit",
                json!({"action":"replace","content":["invalid"]}),
            ),
            ("AskUserQuestion", json!({"questions":[{}]})),
            ("TodoWrite", json!({"todos":"invalid"})),
            ("Bash", json!(["invalid"])),
            ("Edit", json!({"old_string":"evidence", "new_string":null})),
            ("Bash", json!({"description":"evidence"})),
        ] {
            assert_eq!(
                project_tool_input(tool, &input, InputPurpose::Historical),
                Some(input.clone())
            );
        }
    }

    #[test]
    fn third_party_excerpts_are_bounded_and_explicitly_incomplete() {
        for input in [
            json!({"destination":"https://example.com", "body":format!("# Heading\n{}\nFinal instructions", "内容\n".repeat(20_000))}),
            json!({"content": "\u{0000}".repeat(80 * 1024)}),
            json!((0..100_000).collect::<Vec<_>>()),
            json!({"enormous-key".repeat(10_000): "value"}),
        ] {
            let projected =
                project_tool_input("mcp__external", &input, InputPurpose::Proposed).unwrap();
            assert!(is_incomplete(&projected));
            assert!(serde_json::to_vec(&projected).unwrap().len() <= MAX_THIRD_PARTY_INPUT_BYTES);
            assert_eq!(
                projected["review_projection"]["original_json_bytes"],
                serde_json::to_vec(&input).unwrap().len()
            );
        }
        for (tool, field) in [
            ("Write", "content"),
            ("Edit", "new_string"),
            ("Bash", "command"),
            ("NotebookEdit", "new_source"),
        ] {
            let input = json!({field: "all executable content\n".repeat(10_000)});
            assert_eq!(
                project_tool_input(tool, &input, InputPurpose::Historical),
                Some(input)
            );
        }
    }
}
