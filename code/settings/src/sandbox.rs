//! Code-owned execution sandbox settings, read only from the user's settings file.
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::LoadedSettings;

#[derive(Clone, Debug, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SandboxSettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub fail_if_unavailable: bool,
    #[serde(default)]
    pub filesystem: SandboxFilesystem,
    #[serde(default)]
    pub network: SandboxNetwork,
    pub auto_allow_bash_if_sandboxed: bool,
    pub allow_unsandboxed_commands: bool,
    pub excluded_commands: Vec<String>,
}

impl SandboxSettings {
    fn deduplicate(&mut self) {
        for entries in [
            &mut self.excluded_commands,
            &mut self.filesystem.allow_write,
            &mut self.filesystem.deny_write,
            &mut self.network.allowed_domains,
        ] {
            let mut seen = std::collections::HashSet::new();
            entries.retain(|entry| seen.insert(entry.clone()));
        }
    }
}

impl Default for SandboxSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            fail_if_unavailable: false,
            filesystem: SandboxFilesystem::default(),
            network: SandboxNetwork::default(),
            auto_allow_bash_if_sandboxed: true,
            allow_unsandboxed_commands: true,
            excluded_commands: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxNetwork {
    #[serde(default)]
    pub allowed_domains: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxFilesystem {
    #[serde(default)]
    pub allow_write: Vec<String>,
    #[serde(default)]
    pub deny_write: Vec<String>,
}

impl LoadedSettings {
    pub fn sandbox(&mut self) -> Result<SandboxSettings, String> {
        if let Some(error) = &self.read_error {
            return Err(error.clone());
        }
        let Some(value) = self.settings.get("sandbox").cloned() else {
            return Ok(SandboxSettings::default());
        };
        if let Some(object) = value.as_object() {
            self.warn_unsupported_sandbox_fields(
                "sandbox",
                object,
                &[
                    "enabled",
                    "failIfUnavailable",
                    "filesystem",
                    "network",
                    "autoAllowBashIfSandboxed",
                    "allowUnsandboxedCommands",
                    "excludedCommands",
                ],
            );
            if let Some(filesystem) = object.get("filesystem").and_then(Value::as_object) {
                self.warn_unsupported_sandbox_fields(
                    "sandbox.filesystem",
                    filesystem,
                    &["allowWrite", "denyWrite"],
                );
            }
            if let Some(network) = object.get("network").and_then(Value::as_object) {
                self.warn_unsupported_sandbox_fields(
                    "sandbox.network",
                    network,
                    &["allowedDomains"],
                );
            }
        }
        let mut settings: SandboxSettings =
            serde_json::from_value(value).map_err(|error| format!("sandbox: {error}"))?;
        settings.deduplicate();
        Ok(settings)
    }

    pub fn sandbox_for(&mut self, folder: &std::path::Path) -> Result<SandboxSettings, String> {
        self.sandbox()?;
        let project = self
            .settings
            .get("projects")
            .and_then(|value| value.get(super::project_key(folder)))
            .and_then(|value| value.get("sandbox"))
            .cloned();
        let Some(project) = project else {
            return self.sandbox();
        };
        let mut project_settings = self.clone();
        project_settings
            .settings
            .insert("sandbox".into(), project.clone());
        project_settings.sandbox()?;
        for warning in project_settings.warnings {
            if !self.warnings.contains(&warning) {
                self.warnings.push(warning);
            }
        }
        let mut merged = self
            .settings
            .get("sandbox")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        merge_sandbox(&mut merged, project);
        let mut settings: SandboxSettings =
            serde_json::from_value(merged).map_err(|error| format!("sandbox: {error}"))?;
        settings.deduplicate();
        Ok(settings)
    }

    fn warn_unsupported_sandbox_fields(
        &mut self,
        prefix: &str,
        fields: &Map<String, Value>,
        supported: &[&str],
    ) {
        for name in fields
            .keys()
            .filter(|name| !supported.contains(&name.as_str()))
        {
            self.warn(format!("{prefix}.{name}: 暂不支持"));
        }
    }
}

fn merge_sandbox(global: &mut Value, project: Value) {
    match (global, project) {
        (Value::Object(global), Value::Object(project)) => {
            for (key, value) in project {
                merge_sandbox(global.entry(key).or_insert(Value::Null), value);
            }
        }
        (Value::Array(global), Value::Array(project)) => {
            for value in project {
                if !global.contains(&value) {
                    global.push(value);
                }
            }
            let mut unique = Vec::new();
            for value in global.drain(..) {
                if !unique.contains(&value) {
                    unique.push(value);
                }
            }
            *global = unique;
        }
        (global, project) => *global = project,
    }
}

#[derive(Clone, Copy, Debug)]
pub enum SandboxMode {
    AutoAllow,
    Regular,
    Disabled,
}

pub enum SandboxChange {
    Mode(SandboxMode),
    AllowUnsandboxed(bool),
    Exclude(String),
}

#[derive(Clone, Debug)]
pub struct SandboxSnapshot {
    pub settings: SandboxSettings,
    global: Value,
    project: Value,
}

impl SandboxSnapshot {
    pub fn source(&self, pointer: &str, item: Option<&str>) -> &'static str {
        let contains = |value: &Value| {
            value.pointer(pointer).is_some_and(|value| {
                item.is_none_or(|item| {
                    value
                        .as_array()
                        .is_some_and(|items| items.iter().any(|value| value.as_str() == Some(item)))
                })
            })
        };
        match (contains(&self.global), contains(&self.project)) {
            (true, true) if item.is_some() => "global + this Folder",
            (_, true) => "this Folder",
            (true, false) => "global",
            _ => "default",
        }
    }
}

impl LoadedSettings {
    pub fn sandbox_snapshot(
        &mut self,
        folder: &std::path::Path,
    ) -> Result<SandboxSnapshot, String> {
        Ok(SandboxSnapshot {
            settings: self.sandbox_for(folder)?,
            global: self.settings.get("sandbox").cloned().unwrap_or(Value::Null),
            project: self
                .settings
                .get("projects")
                .and_then(|projects| projects.get(super::project_key(folder)))
                .and_then(|project| project.get("sandbox"))
                .cloned()
                .unwrap_or(Value::Null),
        })
    }
}

impl crate::UserSettings {
    pub fn update_sandbox(
        &self,
        folder: &std::path::Path,
        expected: &SandboxSnapshot,
        change: SandboxChange,
    ) -> Result<(), crate::SettingsError> {
        use crate::{
            SettingsError, create_private_directory, lock_exclusively, object_entry,
            parent_directory, project_key,
        };
        let write_error = |source| SettingsError::Write {
            path: self.path.clone(),
            source,
        };
        create_private_directory(parent_directory(&self.path)).map_err(write_error)?;
        let lock_path = self.lock_path();
        let _lock = lock_exclusively(&lock_path).map_err(|source| SettingsError::Lock {
            path: lock_path,
            source,
        })?;
        let mut settings = self.read_object()?;
        let current = LoadedSettings {
            path: self.path.clone(),
            settings: settings.clone(),
            warnings: Vec::new(),
            read_error: None,
        }
        .sandbox_snapshot(folder)
        .map_err(|error| SettingsError::InvalidEntry {
            path: self.path.clone(),
            key: "sandbox".into(),
            expected: error,
        })?;
        if current.global != expected.global || current.project != expected.project {
            return Err(SettingsError::Conflict);
        }
        let invalid = |entry: crate::InvalidEntry| SettingsError::InvalidEntry {
            path: self.path.clone(),
            key: entry.key,
            expected: entry.expected,
        };
        let projects =
            object_entry(&mut settings, "projects", "projects".into()).map_err(invalid)?;
        let project = object_entry(projects, &project_key(folder), "projects.<Folder>".into())
            .map_err(invalid)?;
        let sandbox = object_entry(project, "sandbox", "projects.<Folder>.sandbox".into())
            .map_err(invalid)?;
        match change {
            SandboxChange::Mode(mode) => {
                sandbox.insert(
                    "enabled".into(),
                    Value::Bool(!matches!(mode, SandboxMode::Disabled)),
                );
                if !matches!(mode, SandboxMode::Disabled) {
                    sandbox.insert(
                        "autoAllowBashIfSandboxed".into(),
                        Value::Bool(matches!(mode, SandboxMode::AutoAllow)),
                    );
                }
            }
            SandboxChange::AllowUnsandboxed(allow) => {
                sandbox.insert("allowUnsandboxedCommands".into(), Value::Bool(allow));
            }
            SandboxChange::Exclude(pattern) => {
                if pattern.trim().is_empty() {
                    return Err(SettingsError::InvalidEntry {
                        path: self.path.clone(),
                        key: "sandbox.excludedCommands".into(),
                        expected: "a non-empty command pattern".into(),
                    });
                }
                let entries = sandbox
                    .entry("excludedCommands")
                    .or_insert_with(|| Value::Array(Vec::new()))
                    .as_array_mut()
                    .expect("validated sandbox commands");
                if !current.settings.excluded_commands.contains(&pattern) {
                    entries.push(Value::String(pattern));
                }
            }
        }
        let mut contents =
            serde_json::to_vec_pretty(&settings).expect("settings serialize to JSON");
        contents.push(b'\n');
        self.replace_file(&contents).map_err(write_error)
    }
}
