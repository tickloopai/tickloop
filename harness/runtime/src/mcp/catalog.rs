use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use tickloop_harness_protocol::ToolDefinition;

#[derive(Clone)]
pub(super) struct Tool {
    pub original: String,
    pub schema: Value,
    pub definition: ToolDefinition,
}
pub(super) fn directory(
    server: &str,
    tools: Vec<rmcp::model::Tool>,
) -> (BTreeMap<String, Tool>, Vec<String>) {
    let mut names = BTreeMap::<String, usize>::new();
    for tool in &tools {
        *names.entry(normalize(server, &tool.name)).or_default() += 1;
    }
    let mut directory = BTreeMap::new();
    let mut warnings = Vec::new();
    for tool in tools {
        let original = tool.name.to_string();
        let normalized = normalize(server, &original);
        let name = if normalized.len() > 64 || names[&normalized] > 1 {
            hashed_name(&normalized, &format!("{server}\0{original}"))
        } else {
            normalized
        };
        let schema = Value::Object((*tool.input_schema).clone());
        let flattened = flatten(schema.clone()).filter(|schema| schema["type"] == "object");
        let Some(mut input_schema) = flattened else {
            warnings.push(format!("{name}: unsupported input schema; tool omitted"));
            continue;
        };
        input_schema
            .as_object_mut()
            .unwrap()
            .entry("properties")
            .or_insert(json!({}));
        if jsonschema::validator_for(&schema).is_err()
            || jsonschema::validator_for(&input_schema).is_err()
        {
            warnings.push(format!("{name}: invalid input schema; tool omitted"));
            continue;
        }
        let description = tool
            .description
            .map(|text| text.chars().take(2048).collect())
            .unwrap_or_default();
        directory.insert(
            name.clone(),
            Tool {
                original,
                schema,
                definition: ToolDefinition {
                    name,
                    description,
                    input_schema,
                },
            },
        );
    }
    (directory, warnings)
}
fn normalize(server: &str, tool: &str) -> String {
    format!("mcp__{server}__{tool}")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}
pub(super) fn hashed_name(normalized: &str, identity: &str) -> String {
    let digest = format!("{:x}", Sha256::digest(identity));
    format!(
        "{}_{}",
        &normalized[..normalized.len().min(55)],
        &digest[..8]
    )
}
fn flatten(value: Value) -> Option<Value> {
    match value {
        Value::Array(items) => items
            .into_iter()
            .map(flatten)
            .collect::<Option<Vec<_>>>()
            .map(Value::Array),
        Value::Object(mut object) => {
            if object.contains_key("$ref")
                || object.contains_key("not")
                || object.contains_key("if")
            {
                return None;
            }
            for key in ["oneOf", "anyOf", "allOf"] {
                if let Some(variants) = object.remove(key) {
                    let variants = variants
                        .as_array()?
                        .iter()
                        .cloned()
                        .map(flatten)
                        .collect::<Option<Vec<_>>>()?;
                    let mut merged = variants.first()?.clone();
                    for variant in variants.iter().skip(1) {
                        merged = merge(merged, variant.clone(), key == "allOf")?;
                    }
                    for (key, value) in merged.as_object()? {
                        if let Some(existing) = object.get(key) {
                            if existing != value {
                                return None;
                            }
                        } else {
                            object.insert(key.clone(), value.clone());
                        }
                    }
                }
            }
            object
                .into_iter()
                .map(|(key, value)| Some((key, flatten(value)?)))
                .collect::<Option<serde_json::Map<_, _>>>()
                .map(Value::Object)
        }
        other => Some(other),
    }
}
fn merge(left: Value, right: Value, intersection: bool) -> Option<Value> {
    if left == right {
        return Some(left);
    }
    if left["type"] != right["type"] {
        return None;
    }
    let mut left = left.as_object()?.clone();
    let mut right = right.as_object()?.clone();
    let left_required = left.remove("required");
    let right_required = right.remove("required");
    let required_set = |value: Option<Value>| -> Option<BTreeSet<String>> {
        value
            .map(|value| {
                value
                    .as_array()?
                    .iter()
                    .map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or(Some(BTreeSet::new()))
    };
    let left_required = required_set(left_required)?;
    let right_required = required_set(right_required)?;
    let required: Vec<_> = if intersection {
        left_required.union(&right_required).cloned().collect()
    } else {
        left_required
            .intersection(&right_required)
            .cloned()
            .collect()
    };
    let mut properties = left
        .remove("properties")
        .unwrap_or(json!({}))
        .as_object()?
        .clone();
    for (name, schema) in right
        .remove("properties")
        .unwrap_or(json!({}))
        .as_object()?
    {
        if properties
            .get(name)
            .is_some_and(|existing| existing != schema)
        {
            return None;
        }
        properties.insert(name.clone(), schema.clone());
    }
    if left != right {
        return None;
    }
    if left.get("type").is_some_and(|kind| kind == "object") {
        left.insert("properties".into(), Value::Object(properties));
    }
    if !required.is_empty() {
        left.insert("required".into(), json!(required));
    }
    Some(Value::Object(left))
}
