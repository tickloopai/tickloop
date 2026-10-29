use super::{SkillContext, SkillDefinition, SkillError, SkillExpansion, SkillRegistry};
use regex::Regex;
use serde_json::{Map, Value};
use std::{
    path::{Path, PathBuf},
    sync::LazyLock,
};
use tickloop_harness_protocol::SkillSource;

const MAX_COMMAND_NAME_BYTES: usize = 256;
const MAX_ARGUMENT_NAMES: usize = 64;
const MAX_ARGUMENT_NAME_BYTES: usize = 64;
const MAX_EXPANSION_BYTES: usize = 4_000_000;
static FENCED_SHELL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)```!\s*\n?(.*?)\n?```").unwrap());
static INLINE_SHELL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|\s)(!`([^`]+)`)").unwrap());
static INLINE_CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`[^`\n]+`").unwrap());

pub struct CommandDocument {
    pub name: String,
    pub markdown: String,
    pub source: SkillSource,
    pub base_directory: PathBuf,
    pub metadata: Map<String, Value>,
    /// Public values and placeholders only; the host must never supply secrets.
    pub public_config: std::collections::BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CommandOptions {
    pub arguments: Vec<String>,
    pub base_directory: PathBuf,
    pub model_listed: bool,
    pub variables: Vec<(String, PathBuf)>,
    pub public_config: std::collections::BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SkillShellCommand {
    pub range: std::ops::Range<usize>,
    pub command: String,
}

struct CommandFields(Map<String, Value>);

impl<'de> serde::Deserialize<'de> for CommandFields {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct FieldsVisitor;
        impl<'de> serde::de::Visitor<'de> for FieldsVisitor {
            type Value = CommandFields;
            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("command frontmatter fields")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<Self::Value, M::Error> {
                let mut fields = Map::new();
                while let Some(key) = map.next_key::<String>()? {
                    if matches!(
                        key.as_str(),
                        "description"
                            | "argument-hint"
                            | "arguments"
                            | "disable-model-invocation"
                            | "user-invocable"
                            | "when_to_use"
                    ) {
                        fields.insert(key, map.next_value()?);
                    } else {
                        map.next_value::<serde::de::IgnoredAny>()?;
                        if key != "allowed-tools" {
                            fields.insert(key, Value::Null);
                        }
                    }
                }
                Ok(CommandFields(fields))
            }
        }
        deserializer.deserialize_map(FieldsVisitor)
    }
}

impl SkillExpansion {
    pub fn validate(&self) -> Result<(), SkillError> {
        if self.allowed_tools.is_empty() {
            Ok(())
        } else {
            Err(SkillError::AllowedToolsUnsupported(self.name.clone()))
        }
    }

    /// Resolves only the originally prepared blocks; output is never interpreted as another block.
    pub fn complete_shells(&mut self, outputs: &[String]) -> Result<(), SkillError> {
        if outputs.len() != self.shell_commands.len() {
            return Err(SkillError::Invalid(
                "Shell preprocessing did not finish every command".into(),
            ));
        }
        for (shell, output) in self.shell_commands.iter().zip(outputs).rev() {
            self.content.replace_range(shell.range.clone(), output);
        }
        self.shell_commands.clear();
        Ok(())
    }
}

impl SkillRegistry {
    pub fn register_command(&mut self, document: CommandDocument, variables: &[(&str, &Path)]) {
        let path = match &document.source {
            SkillSource::Plugin { path, .. } => path.clone(),
            _ => {
                self.warn("Commands must come from an enabled plugin".into());
                return;
            }
        };
        let parsed = self.parse_command(document, variables);
        self.register(parsed, Some(&path));
    }

    fn parse_command(
        &mut self,
        document: CommandDocument,
        variables: &[(&str, &Path)],
    ) -> Result<SkillDefinition, SkillError> {
        let invalid = |message: &str| SkillError::Invalid(message.into());
        let SkillSource::Plugin { plugin, .. } = &document.source else {
            return Err(invalid("expected plugin command"));
        };
        if document.name.is_empty()
            || document.name.len() > MAX_COMMAND_NAME_BYTES
            || document.name.split(':').any(|part| {
                part.is_empty()
                    || !part
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
            })
        {
            return Err(invalid("invalid plugin command name"));
        }
        let name = format!("{plugin}:{}", document.name);
        let markdown = document
            .markdown
            .strip_prefix('\u{feff}')
            .unwrap_or(&document.markdown);
        let (mut fields, body) = if let Some(rest) = markdown
            .strip_prefix("---\n")
            .or_else(|| markdown.strip_prefix("---\r\n"))
        {
            let (index, delimiter) =
                ["\n---\n", "\n---\r\n"]
                    .into_iter()
                    .filter_map(|delimiter| rest.find(delimiter).map(|index| (index, delimiter)))
                    .chain(["---\n", "---\r\n"].into_iter().filter_map(|delimiter| {
                        rest.starts_with(delimiter).then_some((0, delimiter))
                    }))
                    .min_by_key(|(index, _)| *index)
                    .ok_or_else(|| invalid("unterminated command frontmatter"))?;
            let fields: CommandFields = if rest[..index].trim().is_empty() {
                CommandFields(Map::new())
            } else {
                serde_saphyr::from_str_with_options(
                    &rest[..index],
                    serde_saphyr::options! { strict_booleans: true },
                )
                .map_err(|error| SkillError::Invalid(format!("invalid command YAML: {error}")))?
            };
            (fields.0, rest[index + delimiter.len()..].trim())
        } else {
            (Map::new(), markdown.trim())
        };
        for (metadata, field) in [
            ("description", "description"),
            ("argumentHint", "argument-hint"),
            ("model", "model"),
            ("allowedTools", "allowed-tools"),
        ] {
            if let Some(value) = document.metadata.get(metadata)
                && !value.is_null()
                && value != &Value::Bool(false)
                && value != &Value::String(String::new())
                && value != &Value::from(0)
            {
                fields.insert(field.into(), value.clone());
            }
        }
        for field in fields.keys() {
            if !matches!(
                field.as_str(),
                "description"
                    | "argument-hint"
                    | "arguments"
                    | "allowed-tools"
                    | "disable-model-invocation"
                    | "user-invocable"
                    | "when_to_use"
            ) {
                self.warn_compatibility(format!(
                    "Command {name}: {field} is not supported and is ignored"
                ));
            }
        }
        let text = |key: &str| -> Result<Option<String>, SkillError> {
            match fields.get(key) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(text)) => {
                    Ok(Some(text.split_whitespace().collect::<Vec<_>>().join(" ")))
                }
                _ => Err(SkillError::Invalid(format!("command {key} must be text"))),
            }
        };
        let boolean = |key: &str, default| -> Result<bool, SkillError> {
            match fields.get(key) {
                None => Ok(default),
                Some(Value::Bool(value)) => Ok(*value),
                _ => Err(SkillError::Invalid(format!(
                    "command {key} must be true or false"
                ))),
            }
        };
        let description = text("description")?.filter(|text| !text.is_empty());
        let when_to_use = text("when_to_use")?.filter(|text| !text.is_empty());
        let model_listed = description.is_some() || when_to_use.is_some();
        let description = description.or(when_to_use).unwrap_or_else(|| {
            body.lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or("Plugin command")
                .trim()
                .trim_start_matches('#')
                .trim()
                .chars()
                .take(100)
                .collect()
        });
        let arguments = match fields.get("arguments") {
            Some(Value::String(names)) => names
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>(),
            Some(Value::Array(names)) => names
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            None => Vec::new(),
            _ => return Err(invalid("command arguments must be text or a list")),
        }
        .into_iter()
        .filter(|name: &String| {
            !name.trim().is_empty() && !name.bytes().all(|byte| byte.is_ascii_digit())
        })
        .collect();
        if body.is_empty() {
            return Err(invalid("command body cannot be empty"));
        }
        let body = body.to_owned();
        Ok(SkillDefinition {
            name,
            description,
            argument_hint: fields
                .get("argument-hint")
                .filter(|value| !value.is_null())
                .map(command_hint),
            source: document.source,
            body,
            allowed_tools: Vec::new(),
            user_invocable: boolean("user-invocable", true)?,
            disable_model_invocation: boolean("disable-model-invocation", false)?,
            context: SkillContext::Inline,
            command: Some(CommandOptions {
                arguments,
                base_directory: document.base_directory,
                model_listed,
                variables: variables
                    .iter()
                    .map(|(name, path)| ((*name).into(), path.to_path_buf()))
                    .collect(),
                public_config: document.public_config,
            }),
        })
    }
}

impl CommandOptions {
    pub(super) fn expand(&self, body: &str, arguments: &str) -> Result<String, SkillError> {
        let mut expanded = expand_arguments(body, arguments, &self.arguments)?;
        for (name, value) in &self.variables {
            expanded = expanded.replace(name, &value.to_string_lossy());
        }
        let mut result = String::new();
        let mut rest = expanded.as_str();
        while let Some(start) = rest.find("${user_config.") {
            result.push_str(&rest[..start]);
            let variable = &rest[start + "${user_config.".len()..];
            let Some(end) = variable.find('}') else {
                result.push_str(&rest[start..]);
                return Ok(result);
            };
            result.push_str(&neutralize_shell_markers(
                self.public_config
                    .get(&variable[..end])
                    .map_or("[configuration required]", String::as_str),
            ));
            rest = &variable[end + 1..];
        }
        result.push_str(rest);
        Ok(result)
    }
}

fn command_hint(value: &Value) -> String {
    match value {
        Value::String(text) => text.split_whitespace().collect::<Vec<_>>().join(" "),
        Value::Array(items) => items.iter().map(command_hint).collect::<Vec<_>>().join(","),
        Value::Object(_) => "[object Object]".into(),
        Value::Null => String::new(),
        value => value.to_string(),
    }
}

fn neutralize_shell_markers(value: &str) -> String {
    let value = value.replace("`!", "` !").replace("!`", "! `");
    let mut result = String::new();
    let mut boundary = true;
    for character in value.chars() {
        if boundary && character == '!' {
            result.push('\\');
        }
        result.push(character);
        boundary = character.is_whitespace();
    }
    result
}

pub(super) fn expand_arguments(
    body: &str,
    arguments: &str,
    names: &[String],
) -> Result<String, SkillError> {
    let mut matching_names = names
        .iter()
        .enumerate()
        .filter(|(_, name)| !name.is_empty() && name.len() <= MAX_ARGUMENT_NAME_BYTES)
        .take(MAX_ARGUMENT_NAMES)
        .collect::<Vec<_>>();
    matching_names.sort_by_key(|(_, name)| std::cmp::Reverse(name.len()));
    let tokens = shell_words::split(arguments)
        .unwrap_or_else(|_| arguments.split_whitespace().map(str::to_owned).collect());
    let mut result = String::new();
    let mut offset = 0;
    let mut substituted = false;
    while let Some(relative) = body[offset..].find('$') {
        let start = offset + relative;
        result.push_str(&body[offset..start]);
        let rest = &body[start + 1..];
        let Some((length, value)) = parameter(rest, arguments, &tokens, &matching_names) else {
            result.push('$');
            offset = start + 1;
            continue;
        };
        let end = start + 1 + length;
        let preceding = &body[..start];
        if preceding.ends_with('\\') && !preceding.ends_with("\\\\") {
            result.pop();
            result.push_str(&body[start..end]);
        } else if let Some(value) = value {
            result.push_str(&neutralize_shell_markers(value));
            substituted = true;
        } else {
            result.push_str(&body[start..end]);
        }
        offset = end;
        if result.len() > MAX_EXPANSION_BYTES {
            return Err(SkillError::Invalid(
                "command expansion exceeds size limit".into(),
            ));
        }
    }
    result.push_str(&body[offset..]);
    if !substituted && !arguments.is_empty() {
        result.push_str(&format!(
            "\n\nARGUMENTS: {}",
            neutralize_shell_markers(arguments)
        ));
    }
    if result.len() > MAX_EXPANSION_BYTES {
        return Err(SkillError::Invalid(
            "command expansion exceeds size limit".into(),
        ));
    }
    Ok(result)
}

fn parameter<'a>(
    rest: &str,
    arguments: &'a str,
    tokens: &'a [String],
    names: &[(usize, &String)],
) -> Option<(usize, Option<&'a str>)> {
    for (index, name) in names {
        if let Some(after) = rest.strip_prefix(name.as_str())
            && !after
                .bytes()
                .next()
                .is_some_and(|byte| byte.is_ascii_alphanumeric() || b"_[".contains(&byte))
        {
            return Some((
                name.len(),
                Some(tokens.get(*index).map_or("", String::as_str)),
            ));
        }
    }
    if let Some(indexed) = rest.strip_prefix("ARGUMENTS[") {
        let length = indexed.bytes().take_while(u8::is_ascii_digit).count();
        if length > 0 && indexed[length..].starts_with(']') {
            let value = indexed[..length]
                .parse::<usize>()
                .ok()
                .and_then(|index| tokens.get(index))
                .map(String::as_str);
            return Some(("ARGUMENTS[".len() + length + 1, value));
        }
    }
    if rest.starts_with("ARGUMENTS") {
        return Some(("ARGUMENTS".len(), Some(arguments)));
    }
    let length = rest.bytes().take_while(u8::is_ascii_digit).count();
    if length == 0
        || rest[length..]
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return None;
    }
    Some((
        length,
        rest[..length]
            .parse::<usize>()
            .ok()
            .and_then(|index| tokens.get(index))
            .map(String::as_str),
    ))
}

pub(super) fn shell_commands(content: &str) -> Result<Vec<SkillShellCommand>, SkillError> {
    let mut commands = FENCED_SHELL
        .captures_iter(content)
        .filter_map(|captures| {
            let command = captures[1].trim().replace("\r\n", "\n");
            (!command.is_empty()).then(|| SkillShellCommand {
                range: captures.get(0).unwrap().range(),
                command,
            })
        })
        .collect::<Vec<_>>();
    // A documented example inside ordinary inline code is not an execution block.
    let mut masked = content.as_bytes().to_vec();
    for matched in INLINE_CODE.find_iter(content) {
        if matched.start() == 0 || !matches!(content.as_bytes()[matched.start() - 1], b'!' | b'`') {
            masked[matched.start() + 1..matched.end() - 1].fill(b' ');
        }
    }
    let masked = String::from_utf8(masked).expect("masking entire UTF-8 spans preserves UTF-8");
    for captures in INLINE_SHELL.captures_iter(&masked) {
        let range = captures.get(1).unwrap().range();
        if commands
            .iter()
            .any(|shell| shell.range.start <= range.start && shell.range.end >= range.end)
        {
            continue;
        }
        let command_range = captures.get(2).unwrap().range();
        let command = content[command_range].trim().replace("\r\n", "\n");
        if !command.is_empty() {
            commands.push(SkillShellCommand { range, command });
        }
    }
    commands.sort_by_key(|shell| shell.range.start);
    if commands
        .windows(2)
        .any(|pair| pair[0].range.end > pair[1].range.start)
    {
        return Err(SkillError::Invalid(
            "overlapping Shell blocks cannot be prepared".into(),
        ));
    }
    Ok(commands)
}
