//! The records vendor tool search leaves in history and how each request replays them.
//!
//! The same model gets its own records back verbatim while its request still offers the search;
//! any other request reads them as plain text, so a conversation continues across models.

use serde_json::{Value, json};
use std::collections::BTreeSet;
use tickloop_harness_protocol::{
    Adapter, Content, ModelConfig, NativeToolSearch, Protocol, ToolDefinition,
};

use crate::ModelError;

pub(crate) const MESSAGES_SEARCH_TOOL: &str = "tool_search_tool_bm25";
const MESSAGES_SEARCH_TOOL_TYPE: &str = "tool_search_tool_bm25_20251119";
pub(crate) const MESSAGES_SEARCH_CALL: &str = "server_tool_use";
pub(crate) const MESSAGES_SEARCH_RESULT: &str = "tool_search_tool_result";
const RESPONSES_SEARCH_TOOL_TYPE: &str = "tool_search";
pub(crate) const RESPONSES_SEARCH_CALL: &str = "tool_search_call";
pub(crate) const RESPONSES_SEARCH_OUTPUT: &str = "tool_search_output";
/// Tickloop asks Responses for hosted search only, never for searches the client runs.
pub(crate) const RESPONSES_HOSTED_EXECUTION: &str = "server";
/// Responses loads each found function into a namespace its later calls must name again.
pub(crate) const RESPONSES_CALL_NAMESPACE: &str = "_tickloop_function_namespace";

/// The native tool search the model's card offers on the adapter it is used with.
pub fn offered(config: &ModelConfig) -> Option<NativeToolSearch> {
    crate::cards::model_card(config)
        .and_then(|card| card.native_tool_search.flatten())
        .filter(|search| supports_adapter(*search, config.adapter()))
}

pub(crate) fn supports_adapter(search: NativeToolSearch, adapter: Adapter) -> bool {
    search.adapter() == adapter
        || search == NativeToolSearch::OpenAi && adapter == Adapter::CodexResponses
}

pub(crate) fn check_request(
    config: &ModelConfig,
    request: &crate::ModelRequest,
) -> Result<(), ModelError> {
    if request.deferred_tools.is_empty() || offered(config).is_some() {
        Ok(())
    } else {
        Err(ModelError::Configuration(
            "this model offers no native tool search for deferred tools".into(),
        ))
    }
}

pub(crate) fn is_record(protocol: Protocol, value: &Value) -> bool {
    match protocol {
        Protocol::Messages => matches!(
            value["type"].as_str(),
            Some(MESSAGES_SEARCH_CALL | MESSAGES_SEARCH_RESULT)
        ),
        Protocol::Responses => matches!(
            value["type"].as_str(),
            Some(RESPONSES_SEARCH_CALL | RESPONSES_SEARCH_OUTPUT | RESPONSES_CALL_NAMESPACE)
        ),
        _ => false,
    }
}

/// A vendor tool search record in history, as `/context` counts it.
pub struct SearchRecord {
    pub label: String,
    /// Deferred tools the record names whose definitions the vendor loads beside it: a Messages
    /// search result carries references, not the definitions themselves.
    pub loaded_tools: Vec<String>,
}

pub fn search_record(protocol: Protocol, value: &Value) -> Option<SearchRecord> {
    if !is_record(protocol, value) {
        return None;
    }
    let loaded_tools = if value["type"] == MESSAGES_SEARCH_RESULT {
        value["content"]["tool_references"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|reference| reference["tool_name"].as_str().map(str::to_owned))
            .collect()
    } else {
        Vec::new()
    };
    Some(SearchRecord {
        label: readable_text(value).unwrap_or_else(|| "Tool search namespace.".into()),
        loaded_tools,
    })
}

/// What a record says, for a model that cannot replay it; namespaces say nothing.
pub(crate) fn readable_text(value: &Value) -> Option<String> {
    let found = |names: Vec<&str>| {
        if names.is_empty() {
            "Tool search found no tools.".to_owned()
        } else {
            format!("Tool search found: {}.", names.join(", "))
        }
    };
    match value["type"].as_str()? {
        MESSAGES_SEARCH_CALL => Some(format!(
            "Searched tools for {}.",
            value["input"]["query"].as_str().map_or_else(
                || value["input"].to_string(),
                |query| format!("\"{query}\"")
            )
        )),
        MESSAGES_SEARCH_RESULT => Some(match value["content"]["error_code"].as_str() {
            Some(code) => format!("Tool search failed: {code}."),
            None => found(
                value["content"]["tool_references"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|reference| reference["tool_name"].as_str())
                    .collect(),
            ),
        }),
        RESPONSES_SEARCH_CALL => Some(format!("Searched tools for {}.", value["arguments"])),
        RESPONSES_SEARCH_OUTPUT => Some(found(
            value["tools"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|tool| tool["name"].as_str())
                .collect(),
        )),
        _ => None,
    }
}

pub(crate) fn readable_content(value: &Value) -> Vec<Content> {
    readable_text(value)
        .map(|text| Content::Text { text })
        .into_iter()
        .collect()
}

pub(crate) fn messages_tools(tools: &[ToolDefinition], deferred: &[ToolDefinition]) -> Value {
    let definition = |tool: &ToolDefinition| json!({"name": tool.name, "description": tool.description, "input_schema": tool.input_schema});
    let mut encoded: Vec<_> = tools.iter().map(definition).collect();
    if !deferred.is_empty() {
        encoded.push(json!({"type": MESSAGES_SEARCH_TOOL_TYPE, "name": MESSAGES_SEARCH_TOOL}));
        encoded.extend(deferred.iter().map(|tool| {
            let mut tool = definition(tool);
            tool["defer_loading"] = json!(true);
            tool
        }));
    }
    json!(encoded)
}

/// A replayed search may name a tool the request no longer offers, which Messages refuses.
pub(crate) fn messages_replay(mut value: Value, deferred: &[ToolDefinition]) -> Value {
    let offered: BTreeSet<_> = deferred.iter().map(|tool| tool.name.as_str()).collect();
    if let Some(references) = value
        .pointer_mut("/content/tool_references")
        .and_then(Value::as_array_mut)
    {
        references.retain(|reference| {
            reference["tool_name"]
                .as_str()
                .is_some_and(|name| offered.contains(name))
        });
    }
    value
}

pub(crate) fn responses_tools(tools: &[ToolDefinition], deferred: &[ToolDefinition]) -> Value {
    let function = |tool: &ToolDefinition| json!({"type":"function","name":tool.name,"description":tool.description,"parameters":tool.input_schema});
    let mut encoded: Vec<_> = tools.iter().map(function).collect();
    if !deferred.is_empty() {
        encoded.push(json!({"type": RESPONSES_SEARCH_TOOL_TYPE}));
        encoded.extend(deferred.iter().map(|tool| {
            let mut tool = function(tool);
            tool["defer_loading"] = json!(true);
            tool
        }));
    }
    json!(encoded)
}
