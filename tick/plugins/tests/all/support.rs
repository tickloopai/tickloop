use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use flate2::{Compression, write::GzEncoder};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tick_plugins::PluginStore;

/// A config root and a place for plugin sources, both in one temporary directory.
pub struct Fixture {
    directory: tempfile::TempDir,
    pub store: PluginStore,
}

impl Fixture {
    pub fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let store = PluginStore::in_config_root(&directory.path().join("config"));
        Self { directory, store }
    }

    pub fn root(&self) -> PathBuf {
        self.directory.path().canonicalize().unwrap()
    }

    pub fn config_root(&self) -> PathBuf {
        self.directory.path().join("config")
    }

    /// An empty directory for a plugin source called `label`.
    pub fn source(&self, label: &str) -> PathBuf {
        let directory = self.root().join("sources").join(label);
        fs::create_dir_all(&directory).unwrap();
        directory
    }

    /// A plugin in the Claude Code format with the given manifest and one Skill per name.
    pub fn claude_plugin(&self, label: &str, manifest: &Value, skills: &[&str]) -> PathBuf {
        let directory = self.source(label);
        write_json(&directory.join(".claude-plugin/plugin.json"), manifest);
        for skill in skills {
            write_skill(&directory.join("skills").join(skill), skill);
        }
        directory
    }

    /// A Tick package with a program and a Skill, as `tick-package.json` describes one.
    pub fn tick_package(&self, label: &str, name: &str, version: &str) -> PathBuf {
        let directory = self.source(label);
        write_json(
            &directory.join("tick-package.json"),
            &json!({
                "name": name,
                "version": version,
                "description": format!("Opens {name}"),
                "executable": format!("bin/tick-{name}"),
                "protocol": true,
                "auth": {"type": "api_key", "env": "SAMPLE_KEY", "label": "Sample key"},
            }),
        );
        write_program(&directory.join(format!("bin/tick-{name}")), label);
        write_skill(&directory.join("skills").join(name), name);
        directory
    }

    pub fn registry(&self) -> Value {
        serde_json::from_slice(&fs::read(self.store.directory().join("installed.json")).unwrap())
            .unwrap()
    }

    /// The relative paths of everything under the plugins directory.
    pub fn plugin_files(&self) -> Vec<PathBuf> {
        let root = self.store.directory();
        let mut files = Vec::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(directory) = pending.pop() {
            let Ok(children) = fs::read_dir(&directory) else {
                continue;
            };
            for child in children {
                let path = child.unwrap().path();
                if fs::symlink_metadata(&path).unwrap().is_dir() {
                    pending.push(path.clone());
                }
                files.push(path.strip_prefix(root).unwrap().to_path_buf());
            }
        }
        files.sort();
        files
    }
}

pub fn write_json(path: &Path, value: &Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

pub fn write_skill(directory: &Path, name: &str) {
    fs::create_dir_all(directory).unwrap();
    fs::write(
        directory.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: Uses {name}\n---\n"),
    )
    .unwrap();
}

pub fn write_program(path: &Path, label: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, format!("#!/bin/sh\necho {label}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Packs the contents of `directory` into a `.tar.gz` at `archive`; returns its SHA-256.
pub fn pack(directory: &Path, archive: &Path) -> String {
    let file = fs::File::create(archive).unwrap();
    let mut builder = tar::Builder::new(GzEncoder::new(file, Compression::fast()));
    builder.append_dir_all(".", directory).unwrap();
    builder.into_inner().unwrap().finish().unwrap();
    format!("{:x}", Sha256::digest(fs::read(archive).unwrap()))
}

pub fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}
