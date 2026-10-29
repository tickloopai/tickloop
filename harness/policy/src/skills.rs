mod commands;
pub use commands::{CommandDocument, SkillShellCommand};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Deserializer, de};
use serde_json::Value;

use tickloop_harness_protocol::{InvokedSkill, RemoteSkillDefinition, SkillSource, ToolDefinition};

use crate::loop_skill::{self, LoopInstructionSource, MaintenanceInstructions};

pub const TICKLOOP_SKILL: &str = include_str!("../skills/tickloop/SKILL.md");
/// `/slides`: only the user invokes it, and its body guides only the run it starts.
pub const SLIDES_SKILL: &str = include_str!("../skills/slides/SKILL.md");
/// `/loop`: its expansion carries the host's reading of the arguments, see [`loop_skill`].
pub const LOOP_SKILL: &str = include_str!("../skills/loop/SKILL.md");
pub const LOOP_SKILL_NAME: &str = "loop";
pub const BUILTIN_SKILLS: [&str; 3] = [TICKLOOP_SKILL, SLIDES_SKILL, LOOP_SKILL];
pub const SKILL_TOOL_NAME: &str = "Skill";
/// In the body of a Tick package's Skill, the directory the package is installed in.
pub const PACKAGE_ROOT_VARIABLE: &str = "${TICK_PACKAGE_ROOT}";
/// In the body of a plugin's Skill, the directory the plugin is installed in, in Tickloop's and
/// Claude Code's naming.
pub const PLUGIN_ROOT_VARIABLES: [&str; 2] = ["${TICKLOOP_PLUGIN_ROOT}", "${CLAUDE_PLUGIN_ROOT}"];
/// In the body of a plugin's Skill, the directory the plugin keeps its data in.
pub const PLUGIN_DATA_VARIABLES: [&str; 2] = ["${TICKLOOP_PLUGIN_DATA}", "${CLAUDE_PLUGIN_DATA}"];
/// In the body of a plugin's Skill, the directory of the Skill itself.
pub const SKILL_DIRECTORY_VARIABLES: [&str; 2] = ["${TICKLOOP_SKILL_DIR}", "${CLAUDE_SKILL_DIR}"];

const MAX_NAME_BYTES: usize = 64;
const MAX_DESCRIPTION_CHARS: usize = 1536;
const DEFAULT_CONTEXT_WINDOW_TOKENS: usize = 200_000;
const ESTIMATED_LISTING_BYTES_PER_TOKEN: usize = 4;
const LISTING_CONTEXT_PERCENT: usize = 1;

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum SkillContext {
    #[default]
    Inline,
    Fork,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillDefinition {
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
    pub source: SkillSource,
    body: String,
    allowed_tools: Vec<String>,
    user_invocable: bool,
    disable_model_invocation: bool,
    context: SkillContext,
    command: Option<commands::CommandOptions>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SkillExpansion {
    pub name: String,
    pub content: String,
    pub source: SkillSource,
    pub allowed_tools: Vec<String>,
    pub shell_commands: Vec<SkillShellCommand>,
}

impl TryFrom<SkillExpansion> for InvokedSkill {
    type Error = SkillError;

    fn try_from(expansion: SkillExpansion) -> Result<Self, Self::Error> {
        if !expansion.shell_commands.is_empty() {
            return Err(SkillError::ShellExecutionRequired(expansion.name));
        }
        if !expansion.allowed_tools.is_empty() {
            return Err(SkillError::AllowedToolsUnsupported(expansion.name));
        }
        Ok(Self {
            name: expansion.name,
            content: expansion.content,
            source: expansion.source,
            run_id: None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkillCaller {
    Model,
    User,
}

#[derive(Clone, Debug)]
pub struct SkillRegistry {
    skills: BTreeMap<String, SkillDefinition>,
    remote: BTreeMap<String, SkillDefinition>,
    warnings: Vec<String>,
    discovery_incomplete: bool,
    loop_files: Arc<dyn LoopInstructionSource>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillCatalog {
    pub content: String,
    pub budget_bytes: usize,
    pub capped_descriptions: Vec<String>,
    pub omitted_descriptions: Vec<String>,
    pub over_budget: bool,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SkillError {
    #[error("unknown skill: {0}")]
    Unknown(String),
    #[error("skill `{0}` requires Shell preprocessing through the runtime")]
    ShellExecutionRequired(String),
    #[error("ambiguous skill `{name}`; use one of: {candidates}")]
    Ambiguous { name: String, candidates: String },
    #[error("skill `{0}` cannot be invoked by the model")]
    ModelInvocationDisabled(String),
    #[error("skill `{0}` cannot be invoked directly by the user")]
    UserInvocationDisabled(String),
    #[error("skill `{0}` requires fork context, which is not supported")]
    ForkUnsupported(String),
    #[error("skill `{0}` requests allowed-tools, which is not supported yet")]
    AllowedToolsUnsupported(String),
    #[error("invalid skill: {0}")]
    Invalid(String),
    #[error("the loop instructions are unavailable: {0}")]
    LoopInstructionsUnavailable(String),
    #[error("{0}")]
    InvalidLoopInterval(tickloop_harness_cron::IntervalError),
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct Frontmatter {
    #[serde(deserialize_with = "deserialize_text")]
    name: String,
    #[serde(deserialize_with = "deserialize_text")]
    description: String,
    #[serde(default)]
    allowed_tools: RawAllowedTools,
    #[serde(
        default = "default_user_invocable",
        deserialize_with = "deserialize_bool"
    )]
    user_invocable: bool,
    #[serde(default, deserialize_with = "deserialize_bool")]
    disable_model_invocation: bool,
    #[serde(default)]
    context: SkillContext,
}

impl SkillRegistry {
    pub fn builtin() -> Self {
        Self::with_builtins(BUILTIN_SKILLS)
    }

    pub fn builtin_without_ticks() -> Self {
        Self::with_builtins([SLIDES_SKILL, LOOP_SKILL])
    }

    pub fn with_builtins<'a>(builtins: impl IntoIterator<Item = &'a str>) -> Self {
        let mut registry = Self::default();
        for markdown in builtins {
            registry.load_markdown(markdown, SkillSource::BuiltIn, None);
        }
        registry
    }

    pub fn set_loop_instructions(&mut self, source: Arc<dyn LoopInstructionSource>) {
        self.loop_files = source;
    }

    pub fn warn(&mut self, warning: String) {
        self.discovery_incomplete = true;
        self.warnings.push(warning);
    }

    fn warn_compatibility(&mut self, warning: String) {
        self.warnings.push(warning);
    }

    pub fn needs_discovery_retry(&self) -> bool {
        self.discovery_incomplete
    }

    /// Registers the Skill in `markdown`. A Tick package's or a plugin's Skill is named
    /// `<package>:<skill>` and has each placeholder of `variables` in its body replaced by its
    /// path; a plugin's Skill gets no tools from its `allowed-tools`.
    pub fn register_markdown(
        &mut self,
        markdown: &str,
        source: SkillSource,
        variables: &[(&str, &Path)],
    ) {
        let path = match &source {
            SkillSource::User { path }
            | SkillSource::Project { path }
            | SkillSource::TickPackage { path, .. }
            | SkillSource::Plugin { path, .. } => Some(path.clone()),
            _ => None,
        };
        let parsed = parse_skill(markdown, source).map(|mut skill| {
            let namespace = match &skill.source {
                SkillSource::TickPackage { package, .. } => Some(package.clone()),
                SkillSource::Plugin { plugin, .. } => Some(plugin.clone()),
                _ => None,
            };
            if let Some(namespace) = namespace {
                skill.name = format!("{namespace}:{}", skill.name);
                for (placeholder, value) in variables {
                    skill.body = skill.body.replace(placeholder, &value.to_string_lossy());
                }
            }
            skill
        });
        self.register(parsed, path.as_deref());
    }

    pub fn definitions(&self) -> impl Iterator<Item = &SkillDefinition> {
        self.skills.values().chain(self.remote.values())
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn contains(&self, name: &str) -> bool {
        self.skills
            .contains_key(name.trim().trim_start_matches('/'))
    }

    /// Resolves a slash invocation without widening the model tool's exact-name contract.
    pub fn resolve_user_name(&self, name: &str) -> Result<&str, SkillError> {
        let name = name.trim().trim_start_matches('/');
        if let Some(skill) = self.skills.get(name) {
            return Ok(&skill.name);
        }
        let matches = self
            .skills
            .values()
            .filter(|skill| {
                skill.user_invocable
                    && skill
                        .name
                        .split_once(':')
                        .is_some_and(|(_, bare)| bare == name)
            })
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [skill] => Ok(&skill.name),
            [] => Err(SkillError::Unknown(name.to_owned())),
            _ => Err(SkillError::Ambiguous {
                name: name.to_owned(),
                candidates: matches
                    .iter()
                    .map(|skill| format!("/{}", skill.name))
                    .collect::<Vec<_>>()
                    .join(", "),
            }),
        }
    }

    pub fn user_definitions(&self) -> impl Iterator<Item = &SkillDefinition> {
        self.skills.values().filter(|skill| skill.user_invocable)
    }

    pub fn is_plugin_command(&self, name: &str) -> Option<bool> {
        self.skills.get(name).map(|skill| skill.command.is_some())
    }

    /// Whether the Skill named `name` disables model invocation, so only the user invokes it.
    pub fn is_user_only(&self, name: &str) -> bool {
        self.skills
            .get(name)
            .is_some_and(|skill| skill.disable_model_invocation)
    }

    pub fn model_listing(&self) -> String {
        self.model_catalog_for_context(None).content
    }

    pub fn model_listing_with_budget(&self, budget: usize) -> String {
        self.catalog_with_budget(budget).content
    }

    pub fn model_catalog_for_context(&self, context_window_tokens: Option<usize>) -> SkillCatalog {
        let budget = context_window_tokens
            .filter(|tokens| *tokens > 0)
            .unwrap_or(DEFAULT_CONTEXT_WINDOW_TOKENS)
            .saturating_mul(ESTIMATED_LISTING_BYTES_PER_TOKEN)
            .saturating_mul(LISTING_CONTEXT_PERCENT)
            / 100;
        self.catalog_with_budget(budget.max(1))
    }

    fn catalog_with_budget(&self, budget: usize) -> SkillCatalog {
        let mut capped_descriptions = Vec::new();
        let mut entries = self
            .definitions()
            .filter(|skill| {
                !skill.disable_model_invocation
                    && skill
                        .command
                        .as_ref()
                        .is_none_or(|command| command.model_listed)
            })
            .map(|skill| {
                let mut description = skill
                    .description
                    .chars()
                    .take(MAX_DESCRIPTION_CHARS)
                    .collect::<String>();
                if skill
                    .description
                    .chars()
                    .nth(MAX_DESCRIPTION_CHARS)
                    .is_some()
                {
                    description.pop();
                    description.push('\u{2026}');
                    capped_descriptions.push(skill.name.clone());
                }
                let name_only = format!("- {} [{}]\n", skill.name, skill.source.display_label());
                let full = format!(
                    "- {} [{}]: {description}\n",
                    skill.name,
                    skill.source.display_label()
                );
                (skill, name_only, full)
            })
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.0.name.cmp(&right.0.name));
        // Names and built-in guidance remain discoverable even when they exceed the soft budget.
        let mut used = entries
            .iter()
            .map(|(skill, name, full)| {
                if skill.source == SkillSource::BuiltIn {
                    full.len()
                } else {
                    name.len()
                }
            })
            .sum::<usize>();
        let mut content = String::new();
        let mut omitted_descriptions = Vec::new();
        for (skill, name, full) in entries {
            if skill.source == SkillSource::BuiltIn {
                content.push_str(&full);
            } else if used.saturating_add(full.len() - name.len()) <= budget {
                used += full.len() - name.len();
                content.push_str(&full);
            } else {
                omitted_descriptions.push(skill.name.clone());
                content.push_str(&name);
            }
        }
        SkillCatalog {
            over_budget: content.len() > budget,
            content,
            budget_bytes: budget,
            capped_descriptions,
            omitted_descriptions,
        }
    }

    pub fn replace_remote(&mut self, definitions: Vec<RemoteSkillDefinition>) {
        self.remote = definitions
            .into_iter()
            .map(|definition| {
                (
                    definition.name.clone(),
                    SkillDefinition {
                        name: definition.name,
                        description: definition.description,
                        argument_hint: None,
                        source: definition.source,
                        body: String::new(),
                        allowed_tools: Vec::new(),
                        user_invocable: false,
                        disable_model_invocation: false,
                        context: SkillContext::Inline,
                        command: None,
                    },
                )
            })
            .collect();
    }

    /// Exposes discoverable guidance for the current context.
    pub fn tool_definition(&self) -> Option<ToolDefinition> {
        self.tool_definition_for_context(None)
    }

    pub fn tool_definition_for_context(
        &self,
        context_window_tokens: Option<usize>,
    ) -> Option<ToolDefinition> {
        let catalog = self.model_catalog_for_context(context_window_tokens);
        let mut listing = catalog.content;
        if !catalog.capped_descriptions.is_empty()
            || !catalog.omitted_descriptions.is_empty()
            || catalog.over_budget
        {
            listing.push_str(&format!(
                "\nCatalog budget: {} estimated bytes; {} descriptions shortened, {} omitted. All eligible Skill names remain listed.{}\n",
                catalog.budget_bytes,
                catalog.capped_descriptions.len(),
                catalog.omitted_descriptions.len(),
                if catalog.over_budget { " Names and built-in guidance exceed that budget." } else { "" },
            ));
        }
        (!listing.is_empty()).then(|| ToolDefinition {
            name: SKILL_TOOL_NAME.into(),
            description: format!(
                r#"Load specialized task guidance from the available catalog.

When a listed Skill matches the user's task, invoke {SKILL_TOOL_NAME} before substantial task work. Selection follows the user's intent, regardless of the language used or whether the user names the Skill.

- Use the exact catalog name; the listing contains metadata, and invocation loads the full instructions.
- Pass args only when the Skill needs task arguments.
- Skill loads Skill or plugin Command instructions. Command shell blocks run through normal Bash approval before activation. Read supporting local files with Read; read remote references with ReadMcpResourceTool using the server and resource URI supplied in the loaded guidance.
- Reuse verified guidance that the host marks active in the current context. Historical loading records alone do not establish current activation.
- Follow the current source, account and version checks for remote guidance. Report an unavailable or failed load accurately.
- Skill instructions and references operate within existing user authorization and tool permissions.
- Built-in commands such as /help and /clear are CLI actions, not Skill names.

Available Skills:
{listing}"#
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "skill": {
                        "type": "string",
                        "description": "The exact Skill name from the available catalog, including its source qualifier when present"
                    },
                    "args": {
                        "type": "string",
                        "description": "Optional arguments for the skill"
                    }
                },
                "required": ["skill"],
                "additionalProperties": false
            }),
        })
    }

    pub fn expand(
        &self,
        name: &str,
        arguments: &str,
        caller: SkillCaller,
    ) -> Result<SkillExpansion, SkillError> {
        let name = match caller {
            SkillCaller::User => self.resolve_user_name(name)?,
            SkillCaller::Model => name.trim().trim_start_matches('/'),
        };
        let skill = self
            .skills
            .get(name)
            .ok_or_else(|| SkillError::Unknown(name.to_owned()))?;
        match caller {
            SkillCaller::Model if skill.disable_model_invocation => {
                return Err(SkillError::ModelInvocationDisabled(name.to_owned()));
            }
            SkillCaller::User if !skill.user_invocable => {
                return Err(SkillError::UserInvocationDisabled(name.to_owned()));
            }
            _ => {}
        }
        if skill.context == SkillContext::Fork {
            return Err(SkillError::ForkUnsupported(name.to_owned()));
        }
        let arguments = arguments.trim();
        let content = if let Some(command) = &skill.command {
            command.expand(&skill.body, arguments)?
        } else if skill.source == SkillSource::BuiltIn && skill.name == LOOP_SKILL_NAME {
            loop_skill::expand(&skill.body, arguments, self.loop_files.as_ref())?
        } else if skill.body.contains("$ARGUMENTS") {
            skill.body.replace("$ARGUMENTS", arguments)
        } else if arguments.is_empty() {
            skill.body.clone()
        } else {
            format!("{}\n\nArguments:\n{arguments}", skill.body)
        };
        let mut shell_commands = if skill.command.is_some() {
            commands::shell_commands(&content)
        } else {
            Vec::new()
        };
        let body_length = content.len();
        let content = if let Some(command) = &skill.command {
            format!(
                "Base directory for this skill: {}\nResolve supporting-file paths against this directory. @file references are text, not attachments; read them using Read with normal permissions.\n\n{content}",
                command.base_directory.display()
            )
        } else {
            match &skill.source {
                SkillSource::User { path }
                | SkillSource::Project { path }
                | SkillSource::TickPackage { path, .. }
                | SkillSource::Plugin { path, .. } => format!(
                    "Base directory for this skill: {}\nResolve relative supporting-file paths against this directory and read their absolute paths with Read.\n\n{content}",
                    path.parent().unwrap_or(path).display()
                ),
                _ => content,
            }
        };
        let prefix_length = content.len() - body_length;
        for shell in &mut shell_commands {
            shell.range.start += prefix_length;
            shell.range.end += prefix_length;
        }
        Ok(SkillExpansion {
            name: skill.name.clone(),
            content,
            source: skill.source.clone(),
            allowed_tools: skill.allowed_tools.clone(),
            shell_commands,
        })
    }

    fn load_markdown(&mut self, markdown: &str, source: SkillSource, path: Option<&Path>) {
        self.register(parse_skill(markdown, source), path);
    }

    fn register(&mut self, parsed: Result<SkillDefinition, SkillError>, path: Option<&Path>) {
        match parsed {
            Ok(skill) if self.skills.contains_key(&skill.name) => self.warn(format!(
                "Skill `{}` from {} is ignored because that name is already registered",
                skill.name,
                display_source(path)
            )),
            Ok(skill) => {
                self.skills.insert(skill.name.clone(), skill);
            }
            Err(error) => self.warn(format!("{} is ignored: {error}", display_source(path))),
        }
    }
}

pub fn expand_remote(
    markdown: &str,
    definition: &RemoteSkillDefinition,
    arguments: &str,
) -> Result<InvokedSkill, SkillError> {
    let skill = parse_skill(markdown, definition.source.clone())?;
    if format!("tickloop-artifacts:{}", skill.name) != definition.name
        || skill.description != definition.description
    {
        return Err(SkillError::Invalid(
            "remote frontmatter does not match its directory entry".into(),
        ));
    }
    if skill.disable_model_invocation
        || skill.context != SkillContext::Inline
        || !skill.allowed_tools.is_empty()
    {
        return Err(SkillError::Invalid(
            "unsupported remote invocation or execution metadata".into(),
        ));
    }
    let content = if arguments.trim().is_empty() {
        skill.body
    } else {
        format!("{}\n\nTask arguments:\n{}", skill.body, arguments.trim())
    };
    let SkillSource::Remote { uri, .. } = &definition.source else {
        return Err(SkillError::Invalid("expected a remote Skill source".into()));
    };
    let root = uri
        .strip_suffix("SKILL.md")
        .ok_or_else(|| SkillError::Invalid("remote Skill URI must end in SKILL.md".into()))?;
    let content = format!(
        "Resource root for this skill: {root}\nRead supporting files with ReadMcpResourceTool using server \"tickloop-artifacts\" and the full URI formed by appending the relative path to this root. Resource reads do not activate Skills.\n\n{content}"
    );
    Ok(InvokedSkill {
        name: definition.name.clone(),
        content,
        source: definition.source.clone(),
        run_id: None,
    })
}

fn display_source(path: Option<&Path>) -> String {
    path.map_or_else(
        || "built-in Skill".into(),
        |path| path.display().to_string(),
    )
}

fn parse_skill(markdown: &str, source: SkillSource) -> Result<SkillDefinition, SkillError> {
    let markdown = markdown.strip_prefix('\u{feff}').unwrap_or(markdown);
    let rest = markdown
        .strip_prefix("---\n")
        .or_else(|| markdown.strip_prefix("---\r\n"))
        .ok_or_else(|| SkillError::Invalid("missing YAML frontmatter".into()))?;
    let (frontmatter, body) = ["\n---\n", "\n---\r\n"]
        .into_iter()
        .filter_map(|delimiter| rest.find(delimiter).map(|index| (index, delimiter.len())))
        .min_by_key(|(index, _)| *index)
        .map(|(index, length)| (&rest[..index], &rest[index + length..]))
        .ok_or_else(|| SkillError::Invalid("unterminated YAML frontmatter".into()))?;
    let frontmatter = parse_frontmatter(frontmatter)?;
    validate_name(&frontmatter.name)?;
    if frontmatter.description.trim().is_empty() {
        return Err(SkillError::Invalid("description cannot be empty".into()));
    }
    let allowed_tools = if matches!(source, SkillSource::Plugin { .. }) {
        Vec::new()
    } else {
        parse_allowed_tools(frontmatter.allowed_tools)?
    };
    let body = body.trim().to_owned();
    if body.is_empty() {
        return Err(SkillError::Invalid("body cannot be empty".into()));
    }
    Ok(SkillDefinition {
        name: frontmatter.name,
        description: frontmatter.description,
        argument_hint: None,
        source,
        body,
        allowed_tools,
        user_invocable: frontmatter.user_invocable,
        disable_model_invocation: frontmatter.disable_model_invocation,
        context: frontmatter.context,
        command: None,
    })
}

fn parse_frontmatter(source: &str) -> Result<Frontmatter, SkillError> {
    let options = serde_saphyr::options! { strict_booleans: true };
    serde_saphyr::from_str_with_options(source, options)
        .map_err(|error| SkillError::Invalid(format!("invalid YAML frontmatter: {error}")))
}

fn deserialize_text<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    match Value::deserialize(deserializer)? {
        Value::String(value) => Ok(value.split_whitespace().collect::<Vec<_>>().join(" ")),
        _ => Err(de::Error::custom("expected a string")),
    }
}

fn default_user_invocable() -> bool {
    true
}

fn deserialize_bool<'de, D: Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    match Value::deserialize(deserializer)? {
        Value::Bool(value) => Ok(value),
        _ => Err(de::Error::custom("expected true or false")),
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawAllowedTools {
    Value(Value),
    Unsupported(de::IgnoredAny),
}

impl Default for RawAllowedTools {
    fn default() -> Self {
        Self::Value(Value::Array(Vec::new()))
    }
}

fn parse_allowed_tools(value: RawAllowedTools) -> Result<Vec<String>, SkillError> {
    let RawAllowedTools::Value(value) = value else {
        return Err(SkillError::Invalid(
            "allowed-tools must be a string or list of strings".into(),
        ));
    };
    match value {
        Value::String(value) if !value.is_empty() => Ok(value
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .collect()),
        Value::Array(values) => values
            .into_iter()
            .map(|value| match value {
                Value::String(value) if !value.is_empty() => Ok(value),
                _ => Err(SkillError::Invalid(
                    "allowed-tools entries must be strings".into(),
                )),
            })
            .collect(),
        _ => Err(SkillError::Invalid(
            "allowed-tools must be a string or list of strings".into(),
        )),
    }
}

fn validate_name(name: &str) -> Result<(), SkillError> {
    if name.is_empty()
        || name.len() > MAX_NAME_BYTES
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        || name.starts_with('-')
        || name.ends_with('-')
        || name.contains("--")
    {
        return Err(SkillError::Invalid(format!(
            "`{name}` is not a lowercase hyphenated Skill name"
        )));
    }
    Ok(())
}

impl Default for SkillRegistry {
    fn default() -> Self {
        Self {
            skills: BTreeMap::new(),
            remote: BTreeMap::new(),
            warnings: Vec::new(),
            discovery_incomplete: false,
            loop_files: Arc::new(MaintenanceInstructions),
        }
    }
}
