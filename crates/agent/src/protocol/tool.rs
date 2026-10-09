use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A tool as offered to the model: its name, what it does and the JSON Schema of its input.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}
