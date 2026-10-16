use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use tick_packages::{InstalledPackage, PackageStore};
use tickloop_agent_protocol::{InvokedSkill, RemoteSkillDefinition, SkillSource, ToolDefinition};

pub const TICKLOOP_SKILL: &str = include_str!("../skills/tickloop/SKILL.md");
pub const BUILTIN_SKILLS: [&str; 1] = [TICKLOOP_SKILL];
pub const SKILL_TOOL_NAME: &str = "Skill";
/// In the body of a Tick package's Skill, the directory the package is installed in.
pub const PACKAGE_ROOT_VARIABLE: &str = "${TICK_PACKAGE_ROOT}";

const SKILL_FILE_NAME: &str = "SKILL.md";
/// The user's Skills in the config root, and a Tick package's in its install directory.
const SKILLS_DIRECTORY: &str = "skills";
const PROJECT_SKILLS_DIRECTORY: &str = ".tickloop/skills";
const MAX_NAME_BYTES: usize = 64;
const MAX_DESCRIPTION_CHARS: usize = 1536;
const DEFAULT_CONTEXT_WINDOW_TOKENS: usize = 200_000;
const ESTIMATED_LISTING_BYTES_PER_TOKEN: usize = 4;
const LISTING_CONTEXT_PERCENT: usize = 1;

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

impl SkillDefinition {
    /// The Skill as its Tick package brings it: named `<package>:<skill>`, which no other Skill
    /// can be named, with the package's install directory in place of `${TICK_PACKAGE_ROOT}`.
    fn in_package(mut self, package: &InstalledPackage) -> Self {
        self.name = format!("{}:{}", package.manifest.name, self.name);
        self.body = self
            .body
            .replace(PACKAGE_ROOT_VARIABLE, &package.root.to_string_lossy());
        self
    }
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
            run_id: None,
            references: Vec::new(),
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
    remote: BTreeMap<String, SkillDefinition>,
    warnings: Vec<String>,
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
    /// The Skills a Chat working in `workdir` finds: the built-in ones, the user's and those of
    /// the installed Tick packages in `config_root`, and the project's.
    pub fn discover(workdir: &Path, config_root: Option<&Path>) -> Self {
        Self::discover_with_builtins(workdir, config_root, BUILTIN_SKILLS)
    }

    pub fn discover_with_builtins<'a>(
        workdir: &Path,
        config_root: Option<&Path>,
        builtins: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        let mut registry = Self::default();
        for markdown in builtins {
            registry.load_markdown(markdown, SkillSource::BuiltIn, None);
        }
        let mut seen_files = HashSet::new();
        if let Some(root) = config_root {
            registry.load_directory(
                &root.join(SKILLS_DIRECTORY),
                SkillOrigin::User,
                &mut seen_files,
            );
            registry.load_tick_packages(root, &mut seen_files);
        }
        for directory in project_skill_directories(workdir) {
            registry.load_directory(&directory, SkillOrigin::Project, &mut seen_files);
        }
        registry
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
            .filter(|skill| !skill.disable_model_invocation)
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
                let name_only = format!("- {}\n", skill.name);
                let full = format!("- {}: {description}\n", skill.name);
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
                        source: definition.source,
                        body: String::new(),
                        allowed_tools: Vec::new(),
                        user_invocable: false,
                        disable_model_invocation: false,
                        context: SkillContext::Inline,
                    },
                )
            })
            .collect();
    }

    /// Exposes discoverable guidance and on-demand reference loading for the current context.
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
- For an already activated remote Skill, use resource to read a relative reference path in its manifest. Resource and args are mutually exclusive.
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
                    },
                    "resource": {
                        "type": "string",
                        "description": "Read a listed relative reference file from an already activated remote Skill; mutually exclusive with args"
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

    fn load_tick_packages(&mut self, config_root: &Path, seen_files: &mut HashSet<PathBuf>) {
        let installed = match PackageStore::in_config_root(config_root).installed() {
            Ok(installed) => installed,
            Err(error) => {
                self.warnings.push(format!(
                    "the Skills of installed Tick packages are not loaded: {error}"
                ));
                return;
            }
        };
        for broken in installed.broken {
            self.warnings.push(format!(
                "the Skills of Tick package {} are not loaded: it cannot be used ({}); install it again",
                broken.name, broken.reason
            ));
        }
        for package in &installed.packages {
            self.load_directory(
                &package.root.join(SKILLS_DIRECTORY),
                SkillOrigin::TickPackage(package),
                seen_files,
            );
        }
    }

    fn load_directory(
        &mut self,
        directory: &Path,
        origin: SkillOrigin<'_>,
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
            let path = canonical.clone();
            let parsed = match origin {
                SkillOrigin::User => parse_skill(&markdown, SkillSource::User { path }),
                SkillOrigin::Project => parse_skill(&markdown, SkillSource::Project { path }),
                SkillOrigin::TickPackage(package) => parse_skill(
                    &markdown,
                    SkillSource::TickPackage {
                        package: package.manifest.name.clone(),
                        path,
                    },
                )
                .map(|skill| skill.in_package(package)),
            };
            self.register(parsed, Some(&canonical));
        }
    }

    fn load_markdown(&mut self, markdown: &str, source: SkillSource, path: Option<&Path>) {
        self.register(parse_skill(markdown, source), path);
    }

    fn register(&mut self, parsed: Result<SkillDefinition, SkillError>, path: Option<&Path>) {
        match parsed {
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

pub fn expand_remote(
    markdown: &str,
    definition: &RemoteSkillDefinition,
    arguments: &str,
) -> Result<InvokedSkill, SkillError> {
    let skill = parse_skill(markdown, definition.source.clone())?;
    if format!("tickloop-docs:{}", skill.name) != definition.name
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
    Ok(InvokedSkill {
        name: definition.name.clone(),
        content,
        source: definition.source.clone(),
        run_id: None,
        references: Vec::new(),
    })
}

#[derive(Clone, Copy)]
enum SkillOrigin<'a> {
    User,
    Project,
    TickPackage(&'a InstalledPackage),
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
