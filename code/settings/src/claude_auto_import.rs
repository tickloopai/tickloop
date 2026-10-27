use std::path::{Path, PathBuf};

use serde_json::Value;
use tickloop_harness::protocol::{
    AutoEnvironmentEntry, AutoEnvironmentEntrySource, AutoRuleChange, AutoRuleKind,
    AutoRuleSettings,
};

const MAX_IMPORT_GROUP_ENTRIES: usize = 256;
const MAX_IMPORT_TEXT_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImportKind {
    SoftAllow,
    SoftDeny,
    Environment,
}

#[derive(Clone, Debug)]
pub struct ImportCandidate {
    pub kind: ImportKind,
    pub text: String,
    pub selected: bool,
    pub skip_reason: Option<String>,
}

pub fn read_user_auto_mode(
    home: Option<&Path>,
    config_dir: Option<&Path>,
) -> Result<(PathBuf, Value), String> {
    let directory = match config_dir.filter(|path| !path.as_os_str().is_empty()) {
        Some(directory) => directory.to_path_buf(),
        None => home
            .ok_or("Cannot locate the Claude Code user directory")?
            .join(".claude"),
    };
    let directory = if directory.is_absolute() {
        directory
    } else {
        std::env::current_dir()
            .map_err(|error| format!("Cannot resolve the Claude Code user directory: {error}"))?
            .join(directory)
    };
    let path = directory.join(crate::SETTINGS_FILE_NAME);
    let bytes =
        std::fs::read(&path).map_err(|error| format!("Cannot read {}: {error}", path.display()))?;
    let settings: Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("Invalid JSON in {}: {error}", path.display()))?;
    let auto_mode = settings
        .get("autoMode")
        .ok_or_else(|| format!("No autoMode in {}", path.display()))?
        .clone();
    preview_auto_mode(&auto_mode, &AutoRuleSettings::default())?;
    Ok((path, auto_mode))
}

pub fn preview_auto_mode(
    auto_mode: &Value,
    current: &AutoRuleSettings,
) -> Result<Vec<ImportCandidate>, String> {
    let object = auto_mode.as_object().ok_or("autoMode must be an object")?;
    let builtins = crate::AutoSettings::default().policy();
    let mut preview: Vec<ImportCandidate> = Vec::new();
    for (field, kind) in [
        ("allow", ImportKind::SoftAllow),
        ("soft_deny", ImportKind::SoftDeny),
        ("deny", ImportKind::SoftDeny),
        ("environment", ImportKind::Environment),
    ] {
        let Some(value) = object.get(field) else {
            continue;
        };
        let texts = value
            .as_array()
            .ok_or_else(|| format!("autoMode.{field} must be an array of strings"))?;
        for value in texts {
            let text = value
                .as_str()
                .ok_or_else(|| format!("autoMode.{field} must be an array of strings"))?
                .trim();
            let duplicate = match kind {
                ImportKind::Environment => current
                    .environment_entries
                    .iter()
                    .any(|entry| entry.text.trim() == text),
                _ => [
                    AutoRuleKind::SoftAllow,
                    AutoRuleKind::SoftDeny,
                    AutoRuleKind::HardDeny,
                ]
                .iter()
                .any(|kind| {
                    current
                        .group(*kind)
                        .custom
                        .iter()
                        .any(|rule| rule.text.trim() == text)
                }),
            };
            let existing_count = match kind {
                ImportKind::SoftAllow => current.soft_allow.custom.len(),
                ImportKind::SoftDeny => current.soft_deny.custom.len(),
                ImportKind::Environment => current.environment_entries.len(),
            };
            let selected_count = preview
                .iter()
                .filter(|row| row.kind == kind && row.selected)
                .count();
            let skip_reason = if text.is_empty() {
                Some("Empty text".into())
            } else if text.len() > MAX_IMPORT_TEXT_BYTES {
                Some(format!("Text exceeds {MAX_IMPORT_TEXT_BYTES} bytes"))
            } else if duplicate
                || preview
                    .iter()
                    .any(|row| row.kind == kind && row.text == text)
            {
                Some("Duplicate custom text".into())
            } else if kind != ImportKind::Environment
                && builtins
                    .rules
                    .iter()
                    .any(|rule| rule_name(&rule.text) == rule_name(text))
            {
                Some("Same name as a built-in rule".into())
            } else if existing_count + selected_count >= MAX_IMPORT_GROUP_ENTRIES {
                Some(format!("Group exceeds {MAX_IMPORT_GROUP_ENTRIES} entries"))
            } else if kind == ImportKind::Environment && current.environment_replacement.is_some() {
                Some("Restore or migrate the legacy Environment before adding entries".into())
            } else {
                None
            };
            preview.push(ImportCandidate {
                kind,
                text: text.into(),
                selected: skip_reason.is_none(),
                skip_reason,
            });
        }
    }
    Ok(preview)
}

fn rule_name(text: &str) -> &str {
    text.split_once(':').map_or(text, |(name, _)| name).trim()
}

pub fn prepare_import(
    candidates: &[ImportCandidate],
    source_file: &Path,
    current: &AutoRuleSettings,
) -> Result<AutoRuleChange, String> {
    let mut soft_allow = Vec::new();
    let mut soft_deny = Vec::new();
    let mut environment_entries = Vec::new();
    for candidate in candidates.iter().filter(|candidate| candidate.selected) {
        if candidate.text.trim().is_empty() || candidate.text.len() > MAX_IMPORT_TEXT_BYTES {
            return Err(format!(
                "Selected entries must contain text and fit within {MAX_IMPORT_TEXT_BYTES} bytes"
            ));
        }
        match candidate.kind {
            ImportKind::SoftAllow => soft_allow.push(candidate.text.clone()),
            ImportKind::SoftDeny => soft_deny.push(candidate.text.clone()),
            ImportKind::Environment => environment_entries.push(AutoEnvironmentEntry {
                id: tickloop_harness::ids::generate(),
                text: candidate.text.clone(),
                directory: None,
                include_descendants: false,
                source: AutoEnvironmentEntrySource::Imported {
                    file: Some(source_file.to_path_buf()),
                    imported_at_ms: None,
                },
                confirmed_at_ms: 0,
                changes: Vec::new(),
            }),
        }
    }
    if soft_allow.is_empty() && soft_deny.is_empty() && environment_entries.is_empty() {
        return Err("No entries selected; nothing applied".into());
    }
    for (added, existing) in [
        (soft_allow.len(), current.soft_allow.custom.len()),
        (soft_deny.len(), current.soft_deny.custom.len()),
        (environment_entries.len(), current.environment_entries.len()),
    ] {
        if added > 0 && added + existing > MAX_IMPORT_GROUP_ENTRIES {
            return Err(format!(
                "Selected additions exceed {MAX_IMPORT_GROUP_ENTRIES} entries in a group"
            ));
        }
    }
    let change = AutoRuleChange::Append {
        soft_allow,
        soft_deny,
        environment_entries,
    };
    current.clone().apply(change.clone())?;
    Ok(change)
}
