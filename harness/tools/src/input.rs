//! How the built-in tools read their input as models write it. Claude Code's `semanticNumber`
//! and `semanticBoolean` also take quoted numbers and booleans; beyond them, an optional field
//! the model filled with `null` or `""` counts as not given, as many models fill every field of
//! the schema.

use serde::{Deserialize, Deserializer, de::DeserializeOwned, de::Error as _};
use serde_json::Value;

use super::ToolError;

/// Parses a tool's input, naming the field whose value was refused.
pub(super) fn parse_input<T: DeserializeOwned>(input: &Value) -> Result<T, ToolError> {
    serde_path_to_error::deserialize(input)
        .map_err(|error| ToolError::InvalidInput(error.to_string()))
}

/// An optional text field, where `""` means not given.
pub(super) fn given_text<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    Ok(Option::<String>::deserialize(deserializer)?.filter(|text| !text.is_empty()))
}

/// An optional non-negative integer that may also be written as a digit string; `null` and blank
/// text mean not given.
pub(super) fn semantic_integer<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<usize>, D::Error> {
    let refused = |found: &dyn std::fmt::Display| {
        D::Error::custom(format!("expected a non-negative integer, found {found}"))
    };
    match Value::deserialize(deserializer)? {
        Value::Null => Ok(None),
        Value::String(text) if text.trim().is_empty() => Ok(None),
        Value::String(text) => text
            .parse()
            .map(Some)
            .map_err(|_| refused(&format_args!("{text:?}"))),
        Value::Number(number) => number
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .map(Some)
            .ok_or_else(|| refused(&number)),
        other => Err(refused(&other)),
    }
}

/// A boolean flag that defaults to false; see [`optional_semantic_boolean`].
pub(super) fn semantic_boolean<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<bool, D::Error> {
    optional_semantic_boolean(deserializer).map(Option::unwrap_or_default)
}

/// An optional boolean that may also be written as `"true"` or `"false"`; `null` and blank text
/// mean not given.
pub(super) fn optional_semantic_boolean<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<bool>, D::Error> {
    match Value::deserialize(deserializer)? {
        Value::Null => Ok(None),
        Value::Bool(value) => Ok(Some(value)),
        Value::String(text) => match text.trim() {
            "" => Ok(None),
            "true" => Ok(Some(true)),
            "false" => Ok(Some(false)),
            _ => Err(D::Error::custom(format!(
                "expected a boolean, found {text:?}"
            ))),
        },
        other => Err(D::Error::custom(format!(
            "expected a boolean, found {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[derive(Debug, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct Input {
        #[serde(default, deserialize_with = "semantic_integer")]
        limit: Option<usize>,
        #[serde(default, deserialize_with = "semantic_boolean")]
        flag: bool,
        #[serde(default, deserialize_with = "optional_semantic_boolean")]
        choice: Option<bool>,
        #[serde(default, deserialize_with = "given_text")]
        path: Option<String>,
    }

    const UNSET: Input = Input {
        limit: None,
        flag: false,
        choice: None,
        path: None,
    };

    #[test]
    fn null_and_empty_optional_values_mean_not_given() {
        for input in [
            json!({}),
            json!({"limit":null,"flag":null,"choice":null,"path":null}),
            json!({"limit":"","flag":"","choice":"","path":""}),
            json!({"limit":" ","flag":" ","choice":" "}),
        ] {
            assert_eq!(parse_input::<Input>(&input).unwrap(), UNSET, "{input}");
        }
    }

    #[test]
    fn quoted_numbers_and_booleans_count_as_written() {
        assert_eq!(
            parse_input::<Input>(&json!({"limit":"30","flag":"true","choice":"false","path":" "}))
                .unwrap(),
            Input {
                limit: Some(30),
                flag: true,
                choice: Some(false),
                path: Some(" ".into()),
            }
        );
        assert_eq!(
            parse_input::<Input>(&json!({"limit":0,"flag":false,"choice":true})).unwrap(),
            Input {
                limit: Some(0),
                choice: Some(true),
                ..UNSET
            }
        );
    }

    #[test]
    fn a_refused_value_names_its_field() {
        for (input, reason) in [
            (
                json!({"limit":"many"}),
                "limit: expected a non-negative integer, found \"many\"",
            ),
            (
                json!({"limit":-1}),
                "limit: expected a non-negative integer, found -1",
            ),
            (
                json!({"limit":true}),
                "limit: expected a non-negative integer, found true",
            ),
            (
                json!({"flag":"yes"}),
                "flag: expected a boolean, found \"yes\"",
            ),
            (json!({"choice":1}), "choice: expected a boolean, found 1"),
            (
                json!({"path":7}),
                "path: invalid type: integer `7`, expected a string",
            ),
        ] {
            let ToolError::InvalidInput(error) = parse_input::<Input>(&input).unwrap_err() else {
                panic!("{input} is not invalid input");
            };
            assert_eq!(error, reason, "{input}");
        }
    }
}
