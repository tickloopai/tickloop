//! Tool input as models write it, read the way its schema means it. Many models fill every
//! property of a flat schema and put `null`, `""` or `[]` in the ones they do not need; an
//! optional property whose schema cannot take that empty value was simply not given.

use serde_json::Value;

/// The most characters of a schema refusal's reason the model is shown; a reason can quote the
/// whole refused value.
const MAX_REFUSAL_REASON_CHARACTERS: usize = 300;

/// `input` without the optional top-level properties whose empty value (`null`, `""` or `[]`)
/// their schema refuses. Required properties, empty values the schema accepts and properties the
/// schema cannot judge are kept, and the remaining fields keep their order.
pub(crate) fn without_unset_optional_fields(schema: &Value, mut input: Value) -> Value {
    let (Some(properties), Some(fields)) = (
        schema.get("properties").and_then(Value::as_object),
        input.as_object_mut(),
    ) else {
        return input;
    };
    let required = schema.get("required").and_then(Value::as_array);
    let is_required = |field: &str| {
        required.is_some_and(|required| required.iter().any(|name| name.as_str() == Some(field)))
    };
    fields.retain(|field, value| {
        is_required(field)
            || !properties
                .get(field)
                .is_some_and(|property| refuses_empty_value(property, value))
    });
    input
}

/// Whether `value` is empty and the property schema refuses it by its `const`, `enum`, `type`,
/// `minLength`, `format` or `minItems`. A schema built from alternatives is never judged here.
fn refuses_empty_value(schema: &Value, value: &Value) -> bool {
    let type_name = match value {
        Value::Null => "null",
        Value::String(text) if text.is_empty() => "string",
        Value::Array(items) if items.is_empty() => "array",
        _ => return false,
    };
    if schema
        .get("const")
        .is_some_and(|expected| expected != value)
        || schema
            .get("enum")
            .and_then(Value::as_array)
            .is_some_and(|choices| !choices.contains(value))
    {
        return true;
    }
    if let Some(types) = schema.get("type") {
        let accepts_type = types.as_str() == Some(type_name)
            || types
                .as_array()
                .is_some_and(|types| types.iter().any(|name| name.as_str() == Some(type_name)));
        if !accepts_type {
            return true;
        }
    }
    let is_positive = |keyword: &str| {
        schema
            .get(keyword)
            .and_then(Value::as_u64)
            .is_some_and(|minimum| minimum > 0)
    };
    match value {
        Value::String(_) => is_positive("minLength") || schema.get("format").is_some(),
        Value::Array(_) => is_positive("minItems"),
        _ => false,
    }
}

/// A JSON Schema refusal of a tool's input as the model reads it: where in the input the first
/// error is and why.
pub(crate) fn schema_refusal(name: &str, error: &jsonschema::ValidationError<'_>) -> String {
    let mut reason = error.to_string();
    if let Some((cut, _)) = reason.char_indices().nth(MAX_REFUSAL_REASON_CHARACTERS) {
        reason.truncate(cut);
        reason.push('…');
    }
    match error.instance_path().as_str() {
        "" => format!("invalid {name} input: {reason}"),
        location => format!("invalid {name} input at {location}: {reason}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({"type":"object","properties":{
            "action":{"type":"string","enum":["open","close"]},
            "name":{"type":"string","minLength":1},
            "note":{"type":"string"},
            "nullable":{"type":["string","null"]},
            "mode":{"type":"string","enum":["fast","","slow"]},
            "id":{"type":"string","format":"uuid"},
            "count":{"type":"integer"},
            "tags":{"type":"array","minItems":1,"items":{"type":"string"}},
            "list":{"type":"array","items":{"type":"string"}},
            "kind":{"const":"tick"},
            "free":{"description":"Any value."},
            "choice":{"anyOf":[{"type":"string"},{"type":"integer"}]}
        },"required":["action","name"],"additionalProperties":false})
    }

    /// Each optional property left empty where its schema cannot take that value is dropped.
    #[test]
    fn optional_properties_left_empty_against_their_schema_are_dropped() {
        let input = json!({
            "action":"open","name":"n",
            "id":"","count":null,"count2":null,"tags":[],"kind":"","mode":null,"note":null
        });
        assert_eq!(
            without_unset_optional_fields(&schema(), input),
            json!({"action":"open","name":"n","count2":null})
        );
        assert_eq!(
            without_unset_optional_fields(
                &schema(),
                json!({"action":"open","name":"n","count":""})
            ),
            json!({"action":"open","name":"n"})
        );
    }

    /// Required properties, empty values the schema accepts, non-empty values and properties the
    /// schema cannot judge stay as the model wrote them, so the schema check still judges them.
    #[test]
    fn required_valid_and_unjudged_values_are_kept() {
        for unchanged in [
            json!({"action":"","name":""}),
            json!({"action":null,"name":null}),
            json!({"action":"open","name":"n","note":"","nullable":null,"mode":"","list":[]}),
            json!({"action":"open","name":"n","free":null,"choice":""}),
            json!({"action":"open","name":"n","count":0,"tags":["a"],"id":"x","kind":"other"}),
            json!(["not", "an", "object"]),
        ] {
            assert_eq!(
                without_unset_optional_fields(&schema(), unchanged.clone()),
                unchanged
            );
        }
    }

    #[test]
    fn a_schema_refusal_names_where_and_why() {
        let schema = json!({"type":"object","properties":{
            "query":{"type":"string"},
            "limits":{"type":"array","items":{"type":"integer"}}
        },"required":["query"]});
        let validator = jsonschema::validator_for(&schema).unwrap();
        let refusal =
            |input: Value| schema_refusal("search", &validator.validate(&input).unwrap_err());
        assert_eq!(
            refusal(json!({})),
            "invalid search input: \"query\" is a required property"
        );
        assert_eq!(
            refusal(json!({"query":"q","limits":[1,"two"]})),
            "invalid search input at /limits/1: \"two\" is not of type \"integer\""
        );
        let long = refusal(json!({"query":["x".repeat(1000)]}));
        assert!(
            long.starts_with("invalid search input at /query: "),
            "{long}"
        );
        assert!(long.ends_with('…'), "{long}");
        assert!(long.chars().count() < 400, "{long}");
    }
}
