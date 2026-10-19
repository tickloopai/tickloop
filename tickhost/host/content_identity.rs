use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use toml::Value;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub struct Identity {
    pub value: String,
    pub inputs: BTreeSet<PathBuf>,
}

pub fn build_settings() -> BTreeMap<String, String> {
    let mut settings = std::env::vars()
        .filter(|(name, _)| {
            matches!(
                name.as_str(),
                "HOST" | "TARGET" | "PROFILE" | "OPT_LEVEL" | "DEBUG" | "CARGO_ENCODED_RUSTFLAGS"
            ) || name.starts_with("CARGO_CFG_")
                || name.starts_with("CARGO_FEATURE_")
        })
        .collect::<BTreeMap<_, _>>();
    let compiler = std::process::Command::new(std::env::var_os("RUSTC").unwrap())
        .arg("--version")
        .output()
        .expect("cannot identify Rust compiler");
    assert!(compiler.status.success(), "cannot identify Rust compiler");
    settings.insert("rustc".into(), String::from_utf8(compiler.stdout).unwrap());
    settings
}

pub fn fingerprint(
    workspace: &Path,
    package: &Path,
    settings: &BTreeMap<String, String>,
) -> Result<Identity> {
    let workspace = workspace.canonicalize()?;
    let root_manifest = read_toml(&workspace.join("Cargo.toml"))?;
    let lock = read_toml(&workspace.join("Cargo.lock"))?;
    let locked = lock
        .get("package")
        .and_then(Value::as_array)
        .ok_or("Cargo.lock has no packages")?;
    let mut graph = Inputs {
        workspace: &workspace,
        root_manifest: &root_manifest,
        locked,
        visited: BTreeSet::new(),
        values: BTreeMap::new(),
        paths: BTreeSet::from([workspace.join("Cargo.toml"), workspace.join("Cargo.lock")]),
    };
    graph.local_package(package)?;
    graph.shared_dependency_features()?;
    for key in [
        "profile",
        "workspace.package.edition",
        "workspace.package.rust-version",
        "workspace.resolver",
    ] {
        if let Some(value) = key
            .split('.')
            .try_fold(&root_manifest, |value, key| value.get(key))
        {
            graph
                .values
                .insert(format!("workspace:{key}"), value.to_string().into_bytes());
        }
    }
    for file in [
        "rust-toolchain",
        "rust-toolchain.toml",
        ".cargo/config",
        ".cargo/config.toml",
    ] {
        let path = workspace.join(file);
        if path.is_file() {
            graph.file(&path)?;
        }
    }
    for (name, value) in settings {
        graph
            .values
            .insert(format!("build:{name}"), value.as_bytes().to_vec());
    }
    let mut hash = Sha256::new();
    for (name, value) in graph.values {
        hash.update((name.len() as u64).to_le_bytes());
        hash.update(name.as_bytes());
        hash.update((value.len() as u64).to_le_bytes());
        hash.update(value);
    }
    Ok(Identity {
        value: format!("tickhost:{:x}", hash.finalize()),
        inputs: graph.paths,
    })
}

struct Inputs<'a> {
    workspace: &'a Path,
    root_manifest: &'a Value,
    locked: &'a [Value],
    visited: BTreeSet<String>,
    values: BTreeMap<String, Vec<u8>>,
    paths: BTreeSet<PathBuf>,
}

impl Inputs<'_> {
    fn shared_dependency_features(&mut self) -> Result<()> {
        let shared = self
            .visited
            .iter()
            .filter_map(|identity| {
                identity
                    .strip_prefix("locked:")
                    .and_then(|identity| identity.split(':').next())
            })
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        let members = self
            .root_manifest
            .get("workspace")
            .and_then(|workspace| workspace.get("members"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str);
        let mut manifests = Vec::new();
        for member in members {
            if let Some(parent) = member.strip_suffix("/*") {
                for entry in std::fs::read_dir(self.workspace.join(parent))? {
                    let path = entry?.path().join("Cargo.toml");
                    if path.is_file() {
                        manifests.push(path);
                    }
                }
            } else {
                manifests.push(self.workspace.join(member).join("Cargo.toml"));
            }
        }
        for path in manifests {
            let manifest = read_toml(&path)?;
            self.paths.insert(path.clone());
            let mut features: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
            for (alias, dependency) in production_dependencies(&manifest) {
                let declaration = &dependency;
                let dependency =
                    if dependency.get("workspace").and_then(Value::as_bool) == Some(true) {
                        self.root_manifest
                            .get("workspace")
                            .and_then(|workspace| workspace.get("dependencies"))
                            .and_then(|dependencies| dependencies.get(&alias))
                            .ok_or("missing workspace dependency")?
                    } else {
                        &dependency
                    };
                let name = dependency
                    .get("package")
                    .and_then(Value::as_str)
                    .unwrap_or(&alias);
                if !shared.contains(name) {
                    continue;
                }
                for key in ["features", "default-features"] {
                    if let Some(value) = dependency.get(key) {
                        features
                            .entry(format!("{alias}:{key}"))
                            .or_default()
                            .insert(value.to_string());
                    }
                    if let Some(value) = declaration.get(key) {
                        features
                            .entry(format!("{alias}:declared:{key}"))
                            .or_default()
                            .insert(value.to_string());
                    }
                }
                if let Some(table) = manifest.get("features").and_then(Value::as_table) {
                    for values in table.values() {
                        if values.as_array().is_some_and(|values| {
                            values.iter().filter_map(Value::as_str).any(|value| {
                                value
                                    .strip_prefix("dep:")
                                    .unwrap_or(value)
                                    .split(['/', '?'])
                                    .next()
                                    == Some(&alias)
                            })
                        }) {
                            features
                                .entry("feature-selection".into())
                                .or_default()
                                .insert(format!("{table:?}"));
                        }
                    }
                }
            }
            if !features.is_empty() {
                self.values.insert(
                    format!("unified-features:{}", self.relative(&path)?),
                    format!("{features:?}").into_bytes(),
                );
            }
        }
        Ok(())
    }

    fn local_package(&mut self, directory: &Path) -> Result<()> {
        let directory = directory.canonicalize()?;
        let manifest_path = directory.join("Cargo.toml");
        let mut manifest = read_toml(&manifest_path)?;
        let name = manifest["package"]["name"]
            .as_str()
            .ok_or("package has no name")?
            .to_owned();
        if !self.visited.insert(format!("local:{name}")) {
            return Ok(());
        }
        self.paths.insert(manifest_path.clone());
        let lock = self
            .locked
            .iter()
            .find(|package| {
                package["name"].as_str() == Some(&name) && package.get("source").is_none()
            })
            .ok_or_else(|| format!("{name} is missing from Cargo.lock"))?
            .clone();
        let dependencies = production_dependencies(&manifest);
        for (alias, mut dependency) in dependencies {
            let inherited = dependency.get("workspace").and_then(Value::as_bool) == Some(true);
            if inherited {
                dependency = self
                    .root_manifest
                    .get("workspace")
                    .and_then(|value| value.get("dependencies"))
                    .and_then(|value| value.get(&alias))
                    .ok_or("missing workspace dependency")?
                    .clone();
                self.values.insert(
                    format!("workspace-dependency:{alias}"),
                    dependency.to_string().into_bytes(),
                );
            }
            if let Some(path) = dependency.get("path").and_then(Value::as_str) {
                let base = if inherited {
                    self.workspace
                } else {
                    &directory
                };
                self.local_package(&base.join(path))?;
            } else {
                let dependency_name = dependency
                    .get("package")
                    .and_then(Value::as_str)
                    .unwrap_or(&alias);
                let references = lock
                    .get("dependencies")
                    .and_then(Value::as_array)
                    .map(|dependencies| {
                        dependencies
                            .iter()
                            .filter_map(Value::as_str)
                            .filter(|entry| {
                                entry.split_whitespace().next() == Some(dependency_name)
                            })
                            .collect::<Vec<_>>()
                    })
                    .ok_or_else(|| {
                        format!("{name} dependency {dependency_name} is missing from Cargo.lock")
                    })?;
                if references.is_empty() {
                    return Err(format!(
                        "{name} dependency {dependency_name} is missing from Cargo.lock"
                    )
                    .into());
                }
                for reference in references {
                    self.locked_package(reference)?;
                }
            }
        }
        normalize_manifest(&mut manifest);
        self.values.insert(
            self.relative(&manifest_path)?,
            manifest.to_string().into_bytes(),
        );
        let source = directory.join("src");
        if source.is_dir() {
            self.directory(&source)?;
        }
        // Build scripts and sibling Rust modules can generate production code.
        for entry in std::fs::read_dir(&directory)? {
            let entry = entry?;
            if entry.file_type()?.is_file()
                && entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "rs")
            {
                self.file(&entry.path())?;
            }
        }
        if let Some(path) = manifest
            .get("package")
            .and_then(|package| package.get("build"))
            .and_then(Value::as_str)
        {
            self.file(&directory.join(path))?;
        }
        Ok(())
    }

    fn locked_package(&mut self, reference: &str) -> Result<()> {
        let mut parts = reference.split_whitespace();
        let name = parts.next().ok_or("empty dependency reference")?;
        let version = parts.next();
        let source = parts.next().map(|source| source.trim_matches(['(', ')']));
        let mut candidates = self.locked.iter().filter(|package| {
            package["name"].as_str() == Some(name)
                && version.is_none_or(|version| package["version"].as_str() == Some(version))
                && source.is_none_or(|source| {
                    package.get("source").and_then(Value::as_str) == Some(source)
                })
        });
        let package = candidates
            .next()
            .ok_or_else(|| format!("unresolved dependency {reference}"))?
            .clone();
        if candidates.next().is_some() {
            return Err(format!("ambiguous dependency {reference}").into());
        }
        let identity = format!(
            "locked:{name}:{}:{}",
            package["version"],
            package
                .get("source")
                .map(Value::to_string)
                .unwrap_or_default()
        );
        if !self.visited.insert(identity.clone()) {
            return Ok(());
        }
        if package.get("source").is_none() {
            let patched = self
                .root_manifest
                .get("patch")
                .and_then(Value::as_table)
                .into_iter()
                .flat_map(|sources| sources.values())
                .filter_map(Value::as_table)
                .flat_map(|packages| packages.iter())
                .find(|(alias, value)| {
                    value
                        .get("package")
                        .and_then(Value::as_str)
                        .unwrap_or(alias)
                        == name
                })
                .and_then(|(_, value)| value.get("path"))
                .and_then(Value::as_str)
                .ok_or_else(|| format!("local dependency {name} needs a path"))?;
            return self.local_package(&self.workspace.join(patched));
        }
        self.values
            .insert(identity, package.to_string().into_bytes());
        for dependency in package
            .get("dependencies")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            self.locked_package(dependency.as_str().ok_or("invalid locked dependency")?)?;
        }
        Ok(())
    }

    fn relative(&self, path: &Path) -> Result<String> {
        Ok(path
            .strip_prefix(self.workspace)?
            .to_string_lossy()
            .into_owned())
    }

    fn file(&mut self, path: &Path) -> Result<()> {
        self.values
            .insert(self.relative(path)?, std::fs::read(path)?);
        self.paths.insert(path.to_owned());
        Ok(())
    }

    fn directory(&mut self, path: &Path) -> Result<()> {
        self.paths.insert(path.to_owned());
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                self.directory(&entry.path())?;
            } else if kind.is_file() {
                self.file(&entry.path())?;
            } else {
                return Err(format!("unsupported source input {}", entry.path().display()).into());
            }
        }
        Ok(())
    }
}

fn read_toml(path: &Path) -> Result<Value> {
    Ok(std::fs::read_to_string(path)?.parse()?)
}

fn production_dependencies(manifest: &Value) -> Vec<(String, Value)> {
    let mut dependencies = Vec::new();
    for table in std::iter::once(manifest).chain(
        manifest
            .get("target")
            .and_then(Value::as_table)
            .into_iter()
            .flat_map(|target| target.values()),
    ) {
        for key in ["dependencies", "build-dependencies"] {
            if let Some(table) = table.get(key).and_then(Value::as_table) {
                dependencies.extend(
                    table
                        .iter()
                        .map(|(name, value)| (name.clone(), value.clone())),
                );
            }
        }
    }
    dependencies
}

fn normalize_manifest(manifest: &mut Value) {
    let table = manifest.as_table_mut().unwrap();
    for key in ["dev-dependencies", "test", "example", "bench"] {
        table.remove(key);
    }
    if let Some(package) = table.get_mut("package").and_then(Value::as_table_mut) {
        if package
            .get("version")
            .and_then(|version| version.get("workspace"))
            .and_then(Value::as_bool)
            == Some(true)
        {
            package.remove("version");
        }
        for key in [
            "description",
            "authors",
            "license",
            "license-file",
            "readme",
            "repository",
            "homepage",
            "documentation",
            "keywords",
            "categories",
            "publish",
            "metadata",
        ] {
            package.remove(key);
        }
    }
    if let Some(targets) = table.get_mut("target").and_then(Value::as_table_mut) {
        for target in targets
            .iter_mut()
            .filter_map(|(_, value)| value.as_table_mut())
        {
            target.remove("dev-dependencies");
        }
    }
}
