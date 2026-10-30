//! Read-only discovery of the Markdown commands selected by a plugin manifest.
use crate::{PluginManifest, manifest::contained_path};
use serde_json::{Map, Value};
use std::{
    collections::BTreeSet,
    fs,
    io::Read,
    path::{Path, PathBuf},
};

const MAX_COMMAND_BYTES: u64 = 1024 * 1024;
const MAX_DIRECTORY_DEPTH: usize = 64;

#[derive(Clone, Debug)]
pub struct PluginCommand {
    pub name: String,
    pub path: PathBuf,
    pub base_directory: PathBuf,
    pub markdown: String,
    pub metadata: Map<String, Value>,
}

#[derive(Default)]
pub struct CommandDiscovery {
    pub commands: Vec<PluginCommand>,
    pub watched: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

/// Reads command files without evaluating their frontmatter, references or shell blocks.
pub fn discover_commands(root: &Path, manifest: &PluginManifest) -> CommandDiscovery {
    let mut discovery = CommandDiscovery::default();
    let Ok(root) = root.canonicalize() else {
        return discovery;
    };
    let mut seen = BTreeSet::new();
    let mut declarations = Vec::new();
    let mut inline = Vec::new();
    match &manifest.commands {
        None => {
            discovery.watched.push(root.join("commands"));
            if fs::symlink_metadata(root.join("commands")).is_ok() {
                declarations.push(("./commands".to_owned(), None, Map::new()));
            }
        }
        Some(Value::String(path)) => declarations.push((path.clone(), None, Map::new())),
        Some(Value::Array(paths)) if paths.iter().all(Value::is_string) => {
            for path in paths.iter().filter_map(Value::as_str) {
                declarations.push((path.into(), None, Map::new()));
            }
        }
        Some(Value::Object(commands)) => {
            for (name, metadata) in commands {
                let Some(metadata) = metadata.as_object() else {
                    discovery
                        .warnings
                        .push(format!("command {name} is ignored: expected an object"));
                    continue;
                };
                if let Some(path) = metadata.get("source").and_then(Value::as_str) {
                    declarations.push((path.into(), Some(name.clone()), metadata.clone()));
                } else if !metadata.contains_key("source")
                    && let Some(content) = metadata.get("content").and_then(Value::as_str)
                {
                    if content.len() as u64 > MAX_COMMAND_BYTES {
                        discovery.warnings.push(format!(
                            "command {name} is ignored: content exceeds {MAX_COMMAND_BYTES} bytes"
                        ));
                        continue;
                    }
                    inline.push(PluginCommand {
                        name: name.clone(),
                        path: root.join(manifest.format.label()),
                        base_directory: root.clone(),
                        markdown: content.into(),
                        metadata: metadata.clone(),
                    });
                }
            }
        }
        _ => {}
    }
    for (declaration, name, metadata) in declarations {
        match contained_path(&root, &declaration) {
            Ok(path) => {
                let path = root.join(path);
                let base = if path.is_dir() {
                    path.clone()
                } else {
                    path.parent().unwrap_or(&root).to_path_buf()
                };
                discovery.visit(
                    &root,
                    &path,
                    &base,
                    (name.as_deref(), &metadata),
                    &mut seen,
                    0,
                );
            }
            Err(reason) => discovery
                .warnings
                .push(format!("command path {declaration:?} is ignored: {reason}")),
        }
    }
    discovery.commands.extend(inline);
    discovery
}

impl CommandDiscovery {
    fn visit(
        &mut self,
        root: &Path,
        path: &Path,
        base: &Path,
        override_metadata: (Option<&str>, &Map<String, Value>),
        seen: &mut BTreeSet<PathBuf>,
        depth: usize,
    ) {
        let (name, metadata) = override_metadata;
        self.watched.push(path.to_path_buf());
        let canonical = match path.canonicalize() {
            Ok(path) if path.starts_with(root) => path,
            _ => {
                self.warnings.push(format!(
                    "command {} is ignored: missing or outside plugin",
                    path.display()
                ));
                return;
            }
        };
        self.watched.push(canonical.clone());
        if depth > MAX_DIRECTORY_DEPTH {
            self.warnings.push(format!(
                "command directory {} is too deeply nested",
                path.display()
            ));
            return;
        }
        if !seen.insert(canonical.clone()) {
            return;
        }
        if canonical.is_dir() {
            let entries = match fs::read_dir(path).and_then(|entries| {
                entries
                    .map(|entry| entry.map(|entry| entry.path()))
                    .collect::<Result<Vec<_>, _>>()
            }) {
                Ok(mut entries) => {
                    entries.sort();
                    entries
                }
                Err(error) => {
                    self.warnings
                        .push(format!("cannot read commands {}: {error}", path.display()));
                    return;
                }
            };
            if let Some(skill) = entries.iter().find(|entry| {
                entry
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("SKILL.md"))
            }) {
                self.visit(root, skill, base, (None, &Map::new()), seen, depth + 1);
            } else {
                for entry in entries {
                    self.visit(root, &entry, base, (None, &Map::new()), seen, depth + 1);
                }
            }
            return;
        }
        let is_skill = path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("SKILL.md"));
        if !canonical.is_file()
            || (!is_skill && path.extension().is_none_or(|extension| extension != "md"))
        {
            return;
        }
        let markdown = (|| {
            let mut content = String::new();
            fs::File::open(&canonical)?
                .take(MAX_COMMAND_BYTES + 1)
                .read_to_string(&mut content)?;
            if content.len() as u64 > MAX_COMMAND_BYTES {
                return Err(std::io::Error::other("command exceeds size limit"));
            }
            Ok(content)
        })();
        let markdown = match markdown {
            Ok(markdown) => markdown,
            Err(error) => {
                self.warnings
                    .push(format!("cannot read command {}: {error}", path.display()));
                return;
            }
        };
        let relative = if is_skill && depth > 0 {
            path.parent().unwrap_or(path).to_path_buf()
        } else {
            path.with_extension("")
        };
        let relative = relative.strip_prefix(base).unwrap_or(&relative);
        let derived = relative
            .components()
            .map(|part| part.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join(":");
        let derived = if derived.is_empty() {
            base.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        } else {
            derived
        };
        self.commands.push(PluginCommand {
            name: name.unwrap_or(&derived).into(),
            base_directory: canonical.parent().unwrap_or(root).to_path_buf(),
            path: canonical,
            markdown,
            metadata: metadata.clone(),
        });
    }
}
