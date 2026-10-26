//! Finding and reading a plugin's manifest: `.tickloop-plugin/plugin.json`, then
//! `.claude-plugin/plugin.json`, then a Tick package's `tick-package.json`; a plugin without any
//! takes its fields from its marketplace entry or its directory. Every component a plugin can
//! carry is recognized, whether or not Tickloop supports it yet.

use std::{
    collections::BTreeSet,
    fs, io,
    os::unix::fs::PermissionsExt,
    path::{Component as PathComponent, Path, PathBuf},
};

use serde_json::{Map, Value, json};
use tick_packages::{MANIFEST_FILE_NAME as TICK_PACKAGE_MANIFEST, PackageAuth, PackageManifest};

use crate::{PluginError, printable};

pub const TICKLOOP_MANIFEST: &str = ".tickloop-plugin/plugin.json";
pub const CLAUDE_MANIFEST: &str = ".claude-plugin/plugin.json";
const SKILL_FILE_NAME: &str = "SKILL.md";
const DEFAULT_SKILLS_DIRECTORY: &str = "skills";
const MAX_VERSION_LENGTH: usize = 64;
const MAX_DESCRIPTION_LENGTH: usize = 2000;
const ANY_EXECUTE_PERMISSION: u32 = 0o111;

/// Manifest fields that declare a component, besides `skills` and the Tick fields.
const COMPONENT_FIELDS: [(&str, Component); 10] = [
    ("commands", Component::Commands),
    ("agents", Component::Agents),
    ("hooks", Component::Hooks),
    ("mcpServers", Component::McpServers),
    ("lspServers", Component::LspServers),
    ("outputStyles", Component::OutputStyles),
    ("userConfig", Component::UserConfig),
    ("channels", Component::Channels),
    ("settings", Component::Settings),
    ("dependencies", Component::Dependencies),
];
/// Where a plugin carries a component without declaring it.
const DEFAULT_COMPONENT_LOCATIONS: [(&str, Component); 7] = [
    ("commands", Component::Commands),
    ("agents", Component::Agents),
    ("output-styles", Component::OutputStyles),
    ("hooks/hooks.json", Component::Hooks),
    (".mcp.json", Component::McpServers),
    (".lsp.json", Component::LspServers),
    ("settings.json", Component::Settings),
];

/// Where a plugin's fields came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManifestFormat {
    Tickloop,
    Claude,
    TickPackage,
    /// The plugin has no manifest; its marketplace entry stands in for one.
    MarketplaceEntry,
    /// The plugin has no manifest and no marketplace entry; it is named after its directory.
    Directory,
}

impl ManifestFormat {
    pub fn label(self) -> &'static str {
        match self {
            Self::Tickloop => TICKLOOP_MANIFEST,
            Self::Claude => CLAUDE_MANIFEST,
            Self::TickPackage => TICK_PACKAGE_MANIFEST,
            Self::MarketplaceEntry => "its marketplace entry",
            Self::Directory => "its directory",
        }
    }
}

/// A kind of thing a plugin carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Component {
    Tick,
    Skills,
    Commands,
    Agents,
    Hooks,
    McpServers,
    LspServers,
    OutputStyles,
    UserConfig,
    Channels,
    Settings,
    Dependencies,
}

impl Component {
    /// Whether Tickloop uses this component of an installed plugin.
    pub fn is_supported(self) -> bool {
        matches!(self, Self::Tick | Self::Skills)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Tick => "tick",
            Self::Skills => "skills",
            Self::Commands => "commands",
            Self::Agents => "agents",
            Self::Hooks => "hooks",
            Self::McpServers => "MCP servers",
            Self::LspServers => "LSP servers",
            Self::OutputStyles => "output styles",
            Self::UserConfig => "user configuration",
            Self::Channels => "channels",
            Self::Settings => "settings",
            Self::Dependencies => "dependencies",
        }
    }
}

/// A plugin's Tick program.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TickProgram {
    /// The program, relative to the plugin root and named `tick-<name>`.
    pub executable: PathBuf,
    /// Whether the program speaks the Tick protocol.
    pub protocol: bool,
}

/// What a plugin declares, wherever it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginManifest {
    pub name: String,
    /// The manifest's own name when a marketplace entry supplies a different identity.
    pub display_name: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
    pub format: ManifestFormat,
    pub tick: Option<TickProgram>,
    /// How the plugin's Tick program logs in to its data source.
    pub auth: PackageAuth,
    /// The directories that hold Skills, relative to the plugin root.
    pub skill_paths: Vec<PathBuf>,
    pub components: BTreeSet<Component>,
    /// What was ignored in the manifest, and why.
    pub warnings: Vec<String>,
}

impl PluginManifest {
    /// Reads the manifest of the plugin in `root`. `default_name` names a plugin that has no
    /// manifest and whose marketplace `entry`, if any, names none.
    pub fn load(
        root: &Path,
        default_name: &str,
        entry: Option<&Map<String, Value>>,
    ) -> Result<Self, PluginError> {
        Self::load_with_identity(root, default_name, entry, None)
    }

    pub(crate) fn load_from_marketplace(
        root: &Path,
        name: &str,
        entry: Option<&Map<String, Value>>,
    ) -> Result<Self, PluginError> {
        validate_plugin_name(name)?;
        Self::load_with_identity(root, name, entry, Some(name))
    }

    fn load_with_identity(
        root: &Path,
        default_name: &str,
        entry: Option<&Map<String, Value>>,
        marketplace_name: Option<&str>,
    ) -> Result<Self, PluginError> {
        let root = root
            .canonicalize()
            .map_err(PluginError::io(format!("cannot open {}", printable(root))))?;
        let mut warnings = Vec::new();
        let tick_package = root.join(TICK_PACKAGE_MANIFEST);
        for (path, format) in [
            (TICKLOOP_MANIFEST, ManifestFormat::Tickloop),
            (CLAUDE_MANIFEST, ManifestFormat::Claude),
        ] {
            if let Some(document) = read_document(&root.join(path))? {
                if tick_package.is_file() {
                    warnings.push(format!("{TICK_PACKAGE_MANIFEST} is ignored beside {path}"));
                }
                return from_document(&root, &document, format, None, marketplace_name, warnings);
            }
        }
        if tick_package.is_file() {
            return from_tick_package(&root, marketplace_name);
        }
        match entry {
            Some(entry) => from_document(
                &root,
                entry,
                ManifestFormat::MarketplaceEntry,
                Some(default_name),
                marketplace_name,
                warnings,
            ),
            None => from_document(
                &root,
                &Map::new(),
                ManifestFormat::Directory,
                Some(default_name),
                marketplace_name,
                warnings,
            ),
        }
    }

    /// The components the plugin carries that Tickloop does not use yet.
    pub fn unsupported(&self) -> Vec<Component> {
        self.components
            .iter()
            .copied()
            .filter(|component| !component.is_supported())
            .collect()
    }

    /// The manifest an installed copy of a plugin without one keeps, so it reads back the same.
    pub(crate) fn written_manifest(&self, entry: Option<&Map<String, Value>>) -> Option<Value> {
        let mut document = match self.format {
            ManifestFormat::MarketplaceEntry => entry.cloned().unwrap_or_default(),
            ManifestFormat::Directory => Map::new(),
            _ => return None,
        };
        for marketplace_field in ["source", "category", "tags", "strict"] {
            document.remove(marketplace_field);
        }
        document.insert("name".to_owned(), json!(self.name));
        Some(Value::Object(document))
    }
}

/// The directories of every Skill under `paths`: a path holding a `SKILL.md` is one Skill,
/// any other names a directory whose subdirectories with a `SKILL.md` are Skills.
pub fn skill_directories(root: &Path, paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut directories = Vec::new();
    for path in paths {
        let path = root.join(path);
        if path.join(SKILL_FILE_NAME).is_file() {
            directories.push(path);
            continue;
        }
        let Ok(entries) = fs::read_dir(&path) else {
            continue;
        };
        let mut found = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|child| child.join(SKILL_FILE_NAME).is_file())
            .collect::<Vec<_>>();
        found.sort();
        directories.extend(found);
    }
    directories
}

/// A plugin name holds lowercase letters, digits and `-`, and does not start with `-`.
pub fn validate_plugin_name(name: &str) -> Result<(), PluginError> {
    tick_packages::validate_package_name(name).map_err(|_| {
        PluginError::Invalid(format!(
            "{:?} is not a plugin name: use lowercase letters, digits and '-', starting with a \
             letter or digit",
            printable(name)
        ))
    })
}

/// A version holds letters, digits, '.', '+' and '-', such as `0.2.0-rc.1+build.5`.
pub(crate) fn is_valid_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= MAX_VERSION_LENGTH
        && version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".+-".contains(&byte))
}

fn read_document(path: &Path) -> Result<Option<Map<String, Value>>, PluginError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(PluginError::io(format!("cannot read {}", printable(path)))(
                error,
            ));
        }
    };
    let invalid = |reason: String| PluginError::Invalid(format!("{} {reason}", printable(path)));
    match serde_json::from_slice(&bytes) {
        Ok(Value::Object(document)) => Ok(Some(document)),
        Ok(_) => Err(invalid("must hold a JSON object".to_owned())),
        Err(error) => Err(invalid(format!("is not valid JSON: {error}"))),
    }
}

fn from_document(
    root: &Path,
    document: &Map<String, Value>,
    format: ManifestFormat,
    default_name: Option<&str>,
    marketplace_name: Option<&str>,
    mut warnings: Vec<String>,
) -> Result<PluginManifest, PluginError> {
    let source = format.label();
    let name = match (document.get("name"), default_name) {
        (Some(Value::String(name)), _) => name.clone(),
        (Some(_), _) => {
            return Err(PluginError::Invalid(format!(
                "name in {source} must be a string"
            )));
        }
        (None, Some(default_name)) => default_name.to_owned(),
        (None, None) => return Err(PluginError::Invalid(format!("{source} has no name"))),
    };
    let display_name = marketplace_name
        .filter(|identity| *identity != name)
        .map(|_| name.clone());
    let name = marketplace_name.map_or(name, str::to_owned);
    validate_plugin_name(&name)?;
    let version = match document.get("version") {
        None | Some(Value::Null) => None,
        Some(Value::String(version)) if is_valid_version(version) => Some(version.clone()),
        Some(_) => {
            return Err(PluginError::Invalid(format!(
                "version in {source} may hold only letters, digits, '.', '+' and '-'"
            )));
        }
    };
    let description = match document.get("description") {
        Some(Value::String(text)) if is_showable(text) => Some(text.clone()),
        None | Some(Value::Null) => None,
        Some(_) => {
            warnings.push(format!(
                "description in {source} is ignored: it must be text of at most \
                 {MAX_DESCRIPTION_LENGTH} characters without control characters"
            ));
            None
        }
    };

    let mut components = BTreeSet::new();
    let skill_paths = match document.get("skills") {
        Some(paths) => contained_paths(root, "skills", paths, &mut warnings),
        None => default_skill_paths(root),
    };
    if !skill_directories(root, &skill_paths).is_empty() {
        components.insert(Component::Skills);
    }
    for (field, component) in COMPONENT_FIELDS {
        if let Some(value) = document.get(field)
            && declared_component(root, field, component, value, &mut warnings)
        {
            components.insert(component);
        }
    }
    insert_default_components(root, &mut components);

    let (tick, auth) = if format == ManifestFormat::Tickloop {
        let program_name = marketplace_name
            .and_then(|_| tick_program_name(document.get("tick")?.get("executable")));
        tick_fields(
            root,
            program_name.unwrap_or(&name),
            version.as_deref(),
            description.as_deref(),
            document,
            &mut warnings,
        )?
    } else {
        (None, PackageAuth::None)
    };
    if tick.is_some() {
        components.insert(Component::Tick);
    }
    Ok(PluginManifest {
        name,
        display_name,
        version,
        description,
        format,
        tick,
        auth,
        skill_paths,
        components,
        warnings,
    })
}

fn from_tick_package(
    root: &Path,
    marketplace_name: Option<&str>,
) -> Result<PluginManifest, PluginError> {
    let (package, display_name) = if let Some(name) = marketplace_name {
        let mut document = read_document(&root.join(TICK_PACKAGE_MANIFEST))?.ok_or_else(|| {
            PluginError::Invalid(format!("plugin has no {TICK_PACKAGE_MANIFEST}"))
        })?;
        let declared_name = document
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                PluginError::Invalid(format!("name in {TICK_PACKAGE_MANIFEST} must be a string"))
            })?
            .to_owned();
        let program_name = tick_program_name(document.get("executable"))
            .unwrap_or(name)
            .to_owned();
        document.insert("name".to_owned(), Value::String(program_name));
        let package =
            PackageManifest::parse(&serde_json::to_vec(&document).expect("manifest serializes"))?;
        check_program(root, &package.executable).map_err(PluginError::Invalid)?;
        (package, (declared_name != name).then_some(declared_name))
    } else {
        (PackageManifest::load(root)?, None)
    };
    let skill_paths = default_skill_paths(root);
    let mut components = BTreeSet::from([Component::Tick]);
    if !skill_directories(root, &skill_paths).is_empty() {
        components.insert(Component::Skills);
    }
    insert_default_components(root, &mut components);
    Ok(PluginManifest {
        name: marketplace_name.map_or(package.name, str::to_owned),
        display_name,
        version: Some(package.version),
        description: Some(package.description),
        format: ManifestFormat::TickPackage,
        tick: Some(TickProgram {
            executable: package.executable,
            protocol: package.protocol,
        }),
        auth: package.auth,
        skill_paths,
        components,
        warnings: Vec::new(),
    })
}

fn tick_program_name(executable: Option<&Value>) -> Option<&str> {
    Path::new(executable?.as_str()?)
        .file_name()?
        .to_str()?
        .strip_prefix("tick-")
}

/// The `tick` and `auth` fields of a Tickloop manifest, checked by the rules of
/// `tick-package.json`.
fn tick_fields(
    root: &Path,
    name: &str,
    version: Option<&str>,
    description: Option<&str>,
    document: &Map<String, Value>,
    warnings: &mut Vec<String>,
) -> Result<(Option<TickProgram>, PackageAuth), PluginError> {
    let Some(tick) = document.get("tick") else {
        if document.get("auth").is_some_and(|auth| !auth.is_null()) {
            warnings.push(format!(
                "auth in {TICKLOOP_MANIFEST} is ignored: only a plugin with a tick logs in"
            ));
        }
        return Ok((None, PackageAuth::None));
    };
    let invalid = |reason: String| {
        PluginError::Invalid(format!(
            "tick in {TICKLOOP_MANIFEST} is not valid: {}",
            reason.replace(TICK_PACKAGE_MANIFEST, TICKLOOP_MANIFEST)
        ))
    };
    let (Some(version), Some(description)) = (version, description) else {
        return Err(invalid(
            "a plugin with a tick also needs a version and a description".to_owned(),
        ));
    };
    let package = json!({
        "name": name,
        "version": version,
        "description": description,
        "executable": tick.get("executable").cloned().unwrap_or(Value::Null),
        "protocol": tick.get("protocol").cloned().unwrap_or(Value::Null),
        "auth": document.get("auth").cloned().unwrap_or(Value::Null),
    });
    let package = PackageManifest::parse(package.to_string().as_bytes())
        .map_err(|error| invalid(error.to_string()))?;
    check_program(root, &package.executable).map_err(invalid)?;
    Ok((
        Some(TickProgram {
            executable: package.executable,
            protocol: package.protocol,
        }),
        package.auth,
    ))
}

/// Why the program at `relative` cannot run as the plugin's Tick, if it can.
fn check_program(root: &Path, relative: &Path) -> Result<(), String> {
    let shown = printable(relative);
    let program = match root.join(relative).canonicalize() {
        Ok(program) => program,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(format!("executable {shown} does not exist in the plugin"));
        }
        Err(error) => return Err(format!("cannot open executable {shown}: {error}")),
    };
    if !program.starts_with(root) {
        return Err(format!("executable {shown} leads outside the plugin"));
    }
    let metadata = fs::metadata(&program)
        .map_err(|error| format!("cannot open executable {shown}: {error}"))?;
    if !metadata.is_file() {
        return Err(format!("executable {shown} is not a file"));
    }
    if metadata.permissions().mode() & ANY_EXECUTE_PERMISSION == 0 {
        return Err(format!("executable {shown} is not executable"));
    }
    Ok(())
}

fn default_skill_paths(root: &Path) -> Vec<PathBuf> {
    if root.join(DEFAULT_SKILLS_DIRECTORY).is_dir() {
        vec![PathBuf::from(DEFAULT_SKILLS_DIRECTORY)]
    } else {
        Vec::new()
    }
}

fn declared_component(
    root: &Path,
    field: &str,
    component: Component,
    value: &Value,
    warnings: &mut Vec<String>,
) -> bool {
    if value.is_null() {
        return false;
    }
    match component {
        Component::Commands if value.is_object() => {
            let mut found = false;
            for (name, metadata) in value.as_object().expect("an object").iter() {
                let field = format!("{field}.{name}.source");
                found |= !contained_paths(
                    root,
                    &field,
                    metadata.get("source").unwrap_or(&Value::Null),
                    warnings,
                )
                .is_empty();
            }
            found
        }
        Component::Commands | Component::Agents | Component::OutputStyles => {
            !contained_paths(root, field, value, warnings).is_empty()
        }
        Component::Hooks | Component::McpServers | Component::LspServers | Component::Settings => {
            match value {
                Value::Null => false,
                Value::Object(_) => true,
                Value::Array(items) => {
                    let mut found = false;
                    for item in items {
                        found |= declared_component(root, field, component, item, warnings);
                    }
                    found
                }
                _ => !contained_paths(root, field, value, warnings).is_empty(),
            }
        }
        _ => !value.is_null(),
    }
}

fn insert_default_components(root: &Path, components: &mut BTreeSet<Component>) {
    for (location, component) in DEFAULT_COMPONENT_LOCATIONS {
        if fs::symlink_metadata(root.join(location)).is_ok() {
            components.insert(component);
        }
    }
}

/// The paths a manifest field lists that start with `./`, stay inside the plugin and exist,
/// relative to the plugin root; the others are dropped with a warning.
fn contained_paths(
    root: &Path,
    field: &str,
    value: &Value,
    warnings: &mut Vec<String>,
) -> Vec<PathBuf> {
    let listed = match value {
        Value::String(path) => vec![path.as_str()],
        Value::Array(paths) if paths.iter().all(Value::is_string) => {
            paths.iter().filter_map(Value::as_str).collect()
        }
        _ => {
            warnings.push(format!(
                "{field} is ignored: it must be a path or a list of paths"
            ));
            return Vec::new();
        }
    };
    let mut paths = Vec::new();
    for text in listed {
        match contained_path(root, text) {
            Ok(path) => paths.push(path),
            Err(reason) => warnings.push(format!(
                "{field} path {:?} is ignored: {reason}",
                printable(text)
            )),
        }
    }
    paths
}

fn contained_path(root: &Path, text: &str) -> Result<PathBuf, &'static str> {
    let Some(relative) = text.strip_prefix("./") else {
        return Err("it must start with ./");
    };
    let mut path = PathBuf::new();
    for component in Path::new(relative).components() {
        match component {
            PathComponent::Normal(part) => path.push(part),
            PathComponent::CurDir => {}
            _ => return Err("it must stay inside the plugin"),
        }
    }
    match root.join(&path).canonicalize() {
        Ok(resolved) if resolved.starts_with(root) => Ok(path),
        Ok(_) => Err("it leads outside the plugin"),
        Err(_) => Err("it does not exist"),
    }
}

fn is_showable(text: &str) -> bool {
    !text.trim().is_empty()
        && text.chars().count() <= MAX_DESCRIPTION_LENGTH
        && !text
            .chars()
            .any(|character| character.is_control() && character != '\n')
}
