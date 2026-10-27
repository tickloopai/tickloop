use std::path::PathBuf;

use tickloop_code_settings::claude_auto_import::{
    ImportCandidate, ImportKind, prepare_import, preview_auto_mode, read_user_auto_mode,
};

use super::{AutoRuleChange, AutoRuleSettings, Role, RoleLine, muted_row, selected_row};

pub(super) struct AutoImportDraft {
    source_file: PathBuf,
    auto_mode: serde_json::Value,
    candidates: Vec<ImportCandidate>,
    selected: usize,
    pub(super) scroll: Option<usize>,
}

impl AutoImportDraft {
    pub(super) fn from_user_settings(current: &AutoRuleSettings) -> Result<Self, String> {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let configured = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from);
        let (source_file, auto_mode) = read_user_auto_mode(home.as_deref(), configured.as_deref())?;
        let candidates = preview_auto_mode(&auto_mode, current)?;
        if candidates.is_empty() {
            return Err("No Auto rules or Environment entries in autoMode".into());
        }
        let mut draft = Self {
            source_file,
            auto_mode,
            candidates,
            selected: 0,
            scroll: None,
        };
        draft.selected = draft.ordered_indices()[0];
        Ok(draft)
    }

    pub(super) fn refresh(&mut self, current: &AutoRuleSettings) -> Result<(), String> {
        self.candidates = preview_auto_mode(&self.auto_mode, current)?;
        self.selected = self.ordered_indices().first().copied().unwrap_or_default();
        self.scroll = None;
        Ok(())
    }

    fn groups(&self) -> Vec<(&'static str, Vec<usize>)> {
        let mut groups = Vec::new();
        for (kind, adding, label) in [
            (
                ImportKind::SoftAllow,
                true,
                "Soft allow · Will add · Will relax review",
            ),
            (
                ImportKind::SoftAllow,
                false,
                "Soft allow · Will skip · Will relax review if selected",
            ),
            (ImportKind::SoftDeny, true, "Soft deny · Will add"),
            (ImportKind::SoftDeny, false, "Soft deny · Will skip"),
            (
                ImportKind::Environment,
                true,
                "Environment (global) · Will add",
            ),
            (
                ImportKind::Environment,
                false,
                "Environment (global) · Will skip",
            ),
        ] {
            let indices: Vec<_> = self
                .candidates
                .iter()
                .enumerate()
                .filter(|(_, row)| row.kind == kind && row.selected == adding)
                .map(|(index, _)| index)
                .collect();
            if !indices.is_empty() {
                groups.push((label, indices));
            }
        }
        groups
    }

    fn ordered_indices(&self) -> Vec<usize> {
        self.groups()
            .into_iter()
            .flat_map(|(_, indices)| indices)
            .collect()
    }

    pub(super) fn move_selection(&mut self, down: bool) {
        let indices = self.ordered_indices();
        let position = indices
            .iter()
            .position(|index| *index == self.selected)
            .unwrap_or_default();
        let next = if down {
            (position + 1).min(indices.len().saturating_sub(1))
        } else {
            position.saturating_sub(1)
        };
        self.selected = indices.get(next).copied().unwrap_or_default();
        self.scroll = None;
    }

    pub(super) fn toggle(&mut self) {
        if let Some(candidate) = self.candidates.get_mut(self.selected) {
            candidate.selected = !candidate.selected;
        }
        self.scroll = None;
    }

    pub(super) fn change(&self, current: &AutoRuleSettings) -> Result<AutoRuleChange, String> {
        prepare_import(&self.candidates, &self.source_file, current)
    }

    pub(super) fn lines(&self) -> Vec<RoleLine> {
        let mut lines = vec![
            RoleLine::role("Import from Claude Code · Preview", Role::DialogTitle),
            muted_row(format!("Source: {}", self.source_file.display())),
            muted_row("One-time copy. Built-in rules and explicit Ask/Deny stay in effect."),
        ];
        for (label, indices) in self.groups() {
            lines.push(RoleLine::default());
            lines.push(RoleLine::role(label, Role::DialogTitle));
            for index in indices {
                let candidate = &self.candidates[index];
                let text = if candidate.text.is_empty() {
                    "<empty>"
                } else {
                    &candidate.text
                };
                let mut row = selected_row(
                    format!("[{}] {text}", if candidate.selected { "x" } else { " " }),
                    index == self.selected,
                );
                if candidate.kind == ImportKind::SoftAllow {
                    row.push(" · Will relax review", Some(Role::Warning));
                }
                lines.push(row);
                if let Some(reason) = &candidate.skip_reason {
                    lines.push(muted_row(format!("    Default skip: {reason}")));
                }
            }
        }
        lines
    }
}
