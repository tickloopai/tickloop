use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use tickloop_agent_protocol::{InvokedSkill, SkillSource, ToolDefinition};

pub const TICKLOOP_SKILL: &str = include_str!("../skills/tickloop/SKILL.md");
pub const SKILL_TOOL_NAME: &str = "Skill";

const SKILL_FILE_NAME: &str = "SKILL.md";
const PROJECT_SKILLS_DIRECTORY: &str = ".tickloop/skills";
const MAX_NAME_BYTES: usize = 64;
const MAX_DESCRIPTION_CHARS: usize = 250;
const DEFAULT_LISTING_BYTES: usize = 8 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SkillContext {
    #[default]
    Inline,
    Fork,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillDefinition {
    pub name: String,
    pub description: String,
    pub source: SkillSource,
    body: String,
    allowed_tools: Vec<String>,
    user_invocable: bool,
    disable_model_invocation: bool,
    context: SkillContext,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillExpansion {
    pub name: String,
    pub content: String,
    pub source: SkillSource,
    pub allowed_tools: Vec<String>,
}

impl TryFrom<SkillExpansion> for InvokedSkill {
    type Error = SkillError;

    fn try_from(expansion: SkillExpansion) -> Result<Self, Self::Error> {
        if !expansion.allowed_tools.is_empty() {
            return Err(SkillError::AllowedToolsUnsupported(expansion.name));
        }
        Ok(Self {
            name: expansion.name,
            content: expansion.content,
            source: expansion.source,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkillCaller {
    Model,
    User,
}

#[derive(Clone, Debug, Default)]
pub struct SkillRegistry {
    skills: BTreeMap<String, SkillDefinition>,
    warnings: Vec<String>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SkillError {
    #[error("unknown skill: {0}")]
    Unknown(String),
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
}

struct Frontmatter {
    name: String,
    description: String,
    allowed_tools: Vec<String>,
    user_invocable: bool,
    disable_model_invocation: bool,
    context: SkillContext,
}

impl SkillRegistry {
    pub fn discover(workdir: &Path, user_skills_directory: Option<&Path>) -> Self {
        Self::discover_with_builtins(workdir, user_skills_directory, [TICKLOOP_SKILL])
    }

    pub fn discover_with_builtins<'a>(
        workdir: &Path,
        user_skills_directory: Option<&Path>,
        builtins: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        let mut registry = Self::default();
        for markdown in builtins {
            registry.load_markdown(markdown, SkillSource::BuiltIn, None);
        }
        let mut seen_files = HashSet::new();
        if let Some(directory) = user_skills_directory {
            registry.load_directory(directory, SkillOrigin::User, &mut seen_files);
        }
        for directory in project_skill_directories(workdir) {
            registry.load_directory(&directory, SkillOrigin::Project, &mut seen_files);
        }
        registry
    }

    pub fn definitions(&self) -> impl Iterator<Item = &SkillDefinition> {
        self.skills.values()
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn contains(&self, name: &str) -> bool {
        self.skills
            .contains_key(name.trim().trim_start_matches('/'))
    }

    pub fn model_listing(&self) -> String {
        self.model_listing_with_budget(DEFAULT_LISTING_BYTES)
    }

    pub fn model_listing_with_budget(&self, budget: usize) -> String {
        let mut listing = String::new();
        for skill in self
            .skills
            .values()
            .filter(|skill| !skill.disable_model_invocation)
        {
            let description = skill
                .description
                .chars()
                .take(MAX_DESCRIPTION_CHARS)
                .collect::<String>();
            let entry = format!("- {}: {}\n", skill.name, description);
            if listing.len().saturating_add(entry.len()) > budget {
                break;
            }
            listing.push_str(&entry);
        }
        listing
    }

    /// Claude Code's `SkillTool` prompt and input schema. Claude Code lists the skills in
    /// system reminders; here the listing follows the prompt, and a loaded skill stays active for
    /// the Chat instead of arriving as a command message.
    pub fn tool_definition(&self) -> Option<ToolDefinition> {
        let listing = self.model_listing();
        (!listing.is_empty()).then(|| ToolDefinition {
            name: SKILL_TOOL_NAME.into(),
            description: format!(
                r#"Execute a skill within the main conversation

When users ask you to perform tasks, check if any of the available skills match. Skills provide specialized capabilities and domain knowledge.

When users reference a "slash command" or "/<something>" (e.g., "/tickloop"), they are referring to a skill. Use this tool to invoke it.

How to invoke:
- Use this tool with the skill name and optional arguments
- Examples:
  - `skill: "pdf"` - invoke the pdf skill
  - `skill: "commit", args: "-m 'Fix bug'"` - invoke with arguments
  - `skill: "review-pr", args: "123"` - invoke with arguments

Important:
- Available skills are listed below; only their metadata is listed, and the full instructions load after invocation
- When a skill matches the user's request, this is a BLOCKING REQUIREMENT: invoke the relevant {SKILL_TOOL_NAME} tool BEFORE generating any other response about the task
- NEVER mention a skill without actually calling this tool
- Do not invoke a skill that is already running
- Do not use this tool for built-in CLI commands (like /help, /clear, etc.)
- If a system message says a skill is active for this Chat, the skill has ALREADY been loaded - follow the instructions directly instead of calling this tool again

Available Skills:
{listing}"#
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "skill": {
                        "type": "string",
                        "description": "The skill name. E.g., \"commit\", \"review-pr\", or \"pdf\""
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
        let name = name.trim().trim_start_matches('/');
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
        let content = if skill.body.contains("$ARGUMENTS") {
            skill.body.replace("$ARGUMENTS", arguments)
        } else if arguments.is_empty() {
            skill.body.clone()
        } else {
            format!("{}\n\nArguments:\n{arguments}", skill.body)
        };
        Ok(SkillExpansion {
            name: skill.name.clone(),
            content,
            source: skill.source.clone(),
            allowed_tools: skill.allowed_tools.clone(),
        })
    }

    fn load_directory(
        &mut self,
        directory: &Path,
        origin: SkillOrigin,
        seen_files: &mut HashSet<PathBuf>,
    ) {
        let entries = match fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => {
                self.warnings.push(format!(
                    "cannot read Skill directory {}: {error}",
                    directory.display()
                ));
                return;
            }
        };
        let mut paths = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path().join(SKILL_FILE_NAME))
            .filter(|path| path.is_file())
            .collect::<Vec<_>>();
        paths.sort();
        for path in paths {
            let canonical = match path.canonicalize() {
                Ok(path) => path,
                Err(error) => {
                    self.warnings
                        .push(format!("cannot resolve Skill {}: {error}", path.display()));
                    continue;
                }
            };
            if !seen_files.insert(canonical.clone()) {
                continue;
            }
            let markdown = match fs::read_to_string(&canonical) {
                Ok(markdown) => markdown,
                Err(error) => {
                    self.warnings.push(format!(
                        "cannot read Skill {}: {error}",
                        canonical.display()
                    ));
                    continue;
                }
            };
            let source = match origin {
                SkillOrigin::User => SkillSource::User {
                    path: canonical.clone(),
                },
                SkillOrigin::Project => SkillSource::Project {
                    path: canonical.clone(),
                },
            };
            self.load_markdown(&markdown, source, Some(&canonical));
        }
    }

    fn load_markdown(&mut self, markdown: &str, source: SkillSource, path: Option<&Path>) {
        match parse_skill(markdown, source) {
            Ok(skill) if self.skills.contains_key(&skill.name) => self.warnings.push(format!(
                "Skill `{}` from {} is ignored because that name is already registered",
                skill.name,
                display_source(path)
            )),
            Ok(skill) => {
                self.skills.insert(skill.name.clone(), skill);
            }
            Err(error) => self
                .warnings
                .push(format!("{} is ignored: {error}", display_source(path))),
        }
    }
}

#[derive(Clone, Copy)]
enum SkillOrigin {
    User,
    Project,
}

fn display_source(path: Option<&Path>) -> String {
    path.map_or_else(
        || "built-in Skill".into(),
        |path| path.display().to_string(),
    )
}

fn project_skill_directories(workdir: &Path) -> Vec<PathBuf> {
    let workdir = workdir
        .canonicalize()
        .unwrap_or_else(|_| workdir.to_path_buf());
    let mut directories = Vec::new();
    for ancestor in workdir.ancestors() {
        directories.push(ancestor.join(PROJECT_SKILLS_DIRECTORY));
        if ancestor.join(".git").exists() {
            break;
        }
        if std::env::var_os("HOME").is_some_and(|home| ancestor == Path::new(&home)) {
            break;
        }
    }
    directories
}

fn parse_skill(markdown: &str, source: SkillSource) -> Result<SkillDefinition, SkillError> {
    let markdown = markdown.strip_prefix('\u{feff}').unwrap_or(markdown);
    let rest = markdown
        .strip_prefix("---\n")
        .ok_or_else(|| SkillError::Invalid("missing YAML frontmatter".into()))?;
    let (frontmatter, body) = rest
        .split_once("\n---\n")
        .ok_or_else(|| SkillError::Invalid("unterminated YAML frontmatter".into()))?;
    let frontmatter = parse_frontmatter(frontmatter)?;
    validate_name(&frontmatter.name)?;
    if frontmatter.description.trim().is_empty() {
        return Err(SkillError::Invalid("description cannot be empty".into()));
    }
    let body = body.trim().to_owned();
    if body.is_empty() {
        return Err(SkillError::Invalid("body cannot be empty".into()));
    }
    Ok(SkillDefinition {
        name: frontmatter.name,
        description: frontmatter.description,
        source,
        body,
        allowed_tools: frontmatter.allowed_tools,
        user_invocable: frontmatter.user_invocable,
        disable_model_invocation: frontmatter.disable_model_invocation,
        context: frontmatter.context,
    })
}

fn parse_frontmatter(source: &str) -> Result<Frontmatter, SkillError> {
    let mut name = None;
    let mut description = None;
    let mut allowed_tools = Vec::new();
    let mut user_invocable = true;
    let mut disable_model_invocation = false;
    let mut context = SkillContext::Inline;
    let mut list_key = None;

    for raw_line in source.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(value) = line.strip_prefix("- ") {
            if list_key != Some("allowed-tools") {
                return Err(SkillError::Invalid(
                    "only allowed-tools accepts a YAML list".into(),
                ));
            }
            allowed_tools.push(parse_scalar(value)?);
            continue;
        }
        let (key, value) = line
            .split_once(':')
            .ok_or_else(|| SkillError::Invalid(format!("invalid frontmatter line `{line}`")))?;
        let key = key.trim();
        let value = value.trim();
        list_key = None;
        match key {
            "name" => set_once(&mut name, parse_scalar(value)?, key)?,
            "description" => set_once(&mut description, parse_scalar(value)?, key)?,
            "allowed-tools" if value.is_empty() => list_key = Some("allowed-tools"),
            "allowed-tools" => allowed_tools.extend(parse_string_list(value)?),
            "user-invocable" => user_invocable = parse_bool(value, key)?,
            "disable-model-invocation" => {
                disable_model_invocation = parse_bool(value, key)?;
            }
            "context" => {
                context = match parse_scalar(value)?.as_str() {
                    "inline" => SkillContext::Inline,
                    "fork" => SkillContext::Fork,
                    _ => {
                        return Err(SkillError::Invalid("context must be inline or fork".into()));
                    }
                }
            }
            _ => {}
        }
    }
    Ok(Frontmatter {
        name: name.ok_or_else(|| SkillError::Invalid("name is required".into()))?,
        description: description
            .ok_or_else(|| SkillError::Invalid("description is required".into()))?,
        allowed_tools,
        user_invocable,
        disable_model_invocation,
        context,
    })
}

fn set_once(target: &mut Option<String>, value: String, key: &str) -> Result<(), SkillError> {
    if target.replace(value).is_some() {
        return Err(SkillError::Invalid(format!("duplicate `{key}` field")));
    }
    Ok(())
}

fn parse_bool(value: &str, key: &str) -> Result<bool, SkillError> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(SkillError::Invalid(format!(
            "`{key}` must be true or false"
        ))),
    }
}

fn parse_string_list(value: &str) -> Result<Vec<String>, SkillError> {
    if let Some(value) = value
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
    {
        return value
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(parse_scalar)
            .collect();
    }
    Ok(parse_scalar(value)?
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect())
}

fn parse_scalar(value: &str) -> Result<String, SkillError> {
    if value.is_empty() || matches!(value, "|" | ">") {
        return Err(SkillError::Invalid(
            "frontmatter values must be non-empty single-line scalars".into(),
        ));
    }
    if value.starts_with('"') {
        return serde_json::from_str(value)
            .map_err(|_| SkillError::Invalid(format!("invalid quoted scalar `{value}`")));
    }
    if let Some(value) = value
        .strip_prefix('\'')
        .and_then(|value| value.strip_suffix('\''))
    {
        return Ok(value.replace("''", "'"));
    }
    if value.starts_with('\'') || value.ends_with('\'') {
        return Err(SkillError::Invalid(format!(
            "invalid quoted scalar `{value}`"
        )));
    }
    Ok(value.to_owned())
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
