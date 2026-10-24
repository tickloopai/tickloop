//! Records written into model text with their Tickloop ids in the Loop's model form. Records
//! serialize their ids as stored, as hyphenated UUIDs; a record that reaches the model passes
//! through [`model_json`] (or [`model_json_text`]) first. Only fields that name ids change:
//! free text such as a task's description keeps whatever it says.

use serde_json::Value;
use tick_protocol::InstanceRef;
use tickloop_ids::IdContext;

const HYPHENATED_UUID_LENGTH: usize = 36;

/// Keys that hold an id or a list of ids besides `id`, `*_id` and `*_ids`.
const ID_KEYS: &[&str] = &[
    "dependencies",
    "blocked_by",
    "affected_members",
    "subscription",
    "watch",
    "instance",
    "tick_instance",
    "replaces",
];

/// Id keys whose values this conversion leaves as they are: provider tool call ids, and the ids
/// of the family message relay and task mutation records.
const KEPT_KEYS: &[&str] = &[
    "call_id",
    "tool_use_id",
    "mutation_id",
    "relay_user_message_id",
    "original_message_id",
];

/// Parts of keys that name ids of other services (Artifacts, Herdr), never rewritten.
const KEPT_KEY_PARTS: &[&str] = &["artifact", "asset", "thread", "pane", "workspace", "herdr"];

/// Subtrees kept as stored: the host's Auto policy, whose rule ids the reviewer cites back.
const KEPT_SUBTREES: &[&str] = &["auto_policy"];

/// `value` with each id it holds under an id key written as the model reads ids, at any depth.
pub fn model_json(ids: &IdContext, value: Value) -> Value {
    convert(ids, value, None)
}

fn convert(ids: &IdContext, value: Value, key: Option<&str>) -> Value {
    match value {
        Value::String(text) if key.is_some_and(is_id_key) => Value::String(model_string(ids, text)),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| convert(ids, item, key))
                .collect(),
        ),
        Value::Object(fields) => Value::Object(
            fields
                .into_iter()
                .map(|(field, value)| {
                    let value = if KEPT_SUBTREES.contains(&field.as_str()) {
                        value
                    } else {
                        convert(ids, value, Some(&field))
                    };
                    (field, value)
                })
                .collect(),
        ),
        other => other,
    }
}

fn is_id_key(key: &str) -> bool {
    let names_id =
        key == "id" || key.ends_with("_id") || key.ends_with("_ids") || ID_KEYS.contains(&key);
    names_id && !KEPT_KEYS.contains(&key) && !KEPT_KEY_PARTS.iter().any(|part| key.contains(part))
}

fn model_string(ids: &IdContext, text: String) -> String {
    if text.len() == HYPHENATED_UUID_LENGTH
        && let Ok(uuid) = uuid::Uuid::try_parse(&text)
    {
        return ids.model_text_of(uuid);
    }
    match text.parse::<InstanceRef>() {
        Ok(instance) if instance.hosted_run().is_some() || instance.shell_task().is_some() => {
            instance.model_text(ids)
        }
        _ => text,
    }
}

/// Serialized JSON converted as [`model_json`] converts its value, keeping the text's field
/// order, for a payload whose order matters to the model reading it.
pub fn model_json_text(ids: &IdContext, json: &str) -> String {
    let mut rewritten = String::with_capacity(json.len());
    // The key each open object or array belongs to, and whether it is an array.
    let mut containers: Vec<(Option<String>, bool)> = Vec::new();
    let mut pending_key: Option<String> = None;
    let mut kept_depth: Option<usize> = None;
    let mut rest = json;
    while let Some(character) = rest.chars().next() {
        match character {
            '"' => {
                let end = string_end(rest);
                let token = &rest[..end];
                rest = &rest[end..];
                let after = rest.trim_start();
                if after.starts_with(':') {
                    pending_key = Some(token[1..token.len() - 1].to_owned());
                    rewritten.push_str(token);
                    continue;
                }
                let (owner, in_array) = containers.last().cloned().unwrap_or((None, false));
                let key = if in_array { owner } else { pending_key.take() };
                let text = &token[1..token.len() - 1];
                if kept_depth.is_none()
                    && key.as_deref().is_some_and(is_id_key)
                    && !text.contains('\\')
                {
                    rewritten.push('"');
                    rewritten.push_str(&model_string(ids, text.to_owned()));
                    rewritten.push('"');
                } else {
                    rewritten.push_str(token);
                }
            }
            '{' | '[' => {
                let key = pending_key.take();
                if kept_depth.is_none()
                    && key
                        .as_deref()
                        .is_some_and(|key| KEPT_SUBTREES.contains(&key))
                {
                    kept_depth = Some(containers.len());
                }
                containers.push((key, character == '['));
                rewritten.push(character);
                rest = &rest[1..];
            }
            '}' | ']' => {
                containers.pop();
                if kept_depth == Some(containers.len()) {
                    kept_depth = None;
                }
                rewritten.push(character);
                rest = &rest[1..];
            }
            ',' => {
                pending_key = None;
                rewritten.push(character);
                rest = &rest[1..];
            }
            other => {
                rewritten.push(other);
                rest = &rest[other.len_utf8()..];
            }
        }
    }
    rewritten
}

/// The length of the JSON string token at the start of `text`, quotes included.
fn string_end(text: &str) -> usize {
    let mut escaped = false;
    for (index, character) in text.char_indices().skip(1) {
        match character {
            '\\' if !escaped => escaped = true,
            '"' if !escaped => return index + 1,
            _ => escaped = false,
        }
    }
    text.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record(ids: &IdContext, uuid: uuid::Uuid) -> (Value, Value) {
        let stored = uuid.to_string();
        let model = ids.model_text_of(uuid);
        let instance = format!("hosted:{stored}");
        let model_instance = InstanceRef::hosted(uuid).model_text(ids);
        (
            json!({"id":stored,"child_chat_id":stored,"task_ids":[stored],"dependencies":[stored],
                "instance":instance,"description":stored,"result":stored,"mutation_id":stored,
                "call_id":stored,"artifact_id":stored,"auto_policy":{"rules":[{"id":stored}]},
                "nested":{"run_id":stored,"text":stored}}),
            json!({"id":model,"child_chat_id":model,"task_ids":[model],"dependencies":[model],
                "instance":model_instance,"description":stored,"result":stored,"mutation_id":stored,
                "call_id":stored,"artifact_id":stored,"auto_policy":{"rules":[{"id":stored}]},
                "nested":{"run_id":model,"text":stored}}),
        )
    }

    #[test]
    fn only_id_fields_take_the_model_form() {
        let ids = IdContext::new();
        let (stored, model) = record(&ids, uuid::Uuid::now_v7());
        assert_eq!(model_json(&ids, stored), model);
    }

    #[test]
    fn the_text_form_converts_the_same_fields_and_keeps_the_field_order() {
        let ids = IdContext::new();
        let (stored, model) = record(&ids, uuid::Uuid::now_v7());
        let text = model_json_text(&ids, &stored.to_string());
        assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), model);
        let ordered = r#"{"z_id":"x","a":"y"}"#;
        assert_eq!(model_json_text(&ids, ordered), ordered);
        let escaped = r#"{"id":"a\"b","text":"say \"hi\""}"#;
        assert_eq!(model_json_text(&ids, escaped), escaped);
    }
}
