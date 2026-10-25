use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tickloop_harness::protocol::{
    AutoEnvironmentDraft, AutoEnvironmentEntry, AutoSetupOptions, AutoSetupPurpose,
};
use uuid::Uuid;

use super::{
    editor::Editor,
    permissions_surface::{edit_text, editor_window, muted_row, plain_row, selected_row},
    style::RoleLine,
};

pub(super) enum SetupEffect {
    None,
    Back,
    Generate(Uuid, AutoSetupOptions),
    Preview(Vec<AutoEnvironmentEntry>),
}

pub(super) struct AutoSetupSurface {
    directory: std::path::PathBuf,
    options: AutoSetupOptions,
    selected: usize,
    generation: Option<Uuid>,
    draft: Option<AutoEnvironmentDraft>,
    included: Vec<bool>,
    editor: Option<Editor>,
    editing_repositories: bool,
    pub message: Option<String>,
}

impl AutoSetupSurface {
    pub fn new(directory: std::path::PathBuf) -> Self {
        Self {
            directory,
            options: AutoSetupOptions::default(),
            selected: 0,
            generation: None,
            draft: None,
            included: Vec::new(),
            editor: None,
            editing_repositories: false,
            message: None,
        }
    }

    pub fn finish(&mut self, generation: Uuid, result: Result<AutoEnvironmentDraft, String>) {
        if self.generation != Some(generation) {
            return;
        }
        self.generation = None;
        match result {
            Ok(draft) => {
                self.included = vec![true; draft.entries.len()];
                self.draft = Some(draft);
                self.selected = 0;
            }
            Err(error) => self.message = Some(error),
        }
    }

    pub fn key(&mut self, key: KeyEvent) -> SetupEffect {
        if let Some(editor) = &mut self.editor {
            match key.code {
                KeyCode::Esc => self.editor = None,
                KeyCode::Enter => {
                    let text = editor.draft().trim().to_owned();
                    if self.editing_repositories {
                        self.options.other_repositories = text
                            .lines()
                            .filter(|line| !line.trim().is_empty())
                            .map(|line| line.trim().into())
                            .collect();
                    } else if !text.is_empty()
                        && let Some(draft) = &mut self.draft
                    {
                        draft.entries[self.selected].text = text;
                    }
                    self.editor = None;
                }
                _ => edit_text(editor, key),
            }
            return SetupEffect::None;
        }
        if key.code == KeyCode::Esc {
            return SetupEffect::Back;
        }
        if self.generation.is_some() {
            return SetupEffect::None;
        }
        let count = self
            .draft
            .as_ref()
            .map_or(4, |draft| draft.entries.len() + 1);
        match (key.code, key.modifiers) {
            (KeyCode::Char('s'), KeyModifiers::NONE)
                if self
                    .draft
                    .as_ref()
                    .is_some_and(|draft| self.selected < draft.entries.len()) =>
            {
                let entry = &mut self.draft.as_mut().unwrap().entries[self.selected];
                entry.directory = if entry.directory.is_some() {
                    None
                } else {
                    Some(self.directory.clone())
                };
                entry.include_descendants = false;
            }
            (KeyCode::Char('r'), KeyModifiers::NONE)
                if self
                    .draft
                    .as_ref()
                    .is_some_and(|draft| self.selected < draft.entries.len()) =>
            {
                let entry = &mut self.draft.as_mut().unwrap().entries[self.selected];
                if entry.directory.is_some() {
                    entry.include_descendants = !entry.include_descendants;
                }
            }
            (KeyCode::Up | KeyCode::Char('k'), KeyModifiers::NONE)
            | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                self.selected = self.selected.saturating_sub(1)
            }
            (KeyCode::Down | KeyCode::Char('j'), KeyModifiers::NONE)
            | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                self.selected = (self.selected + 1).min(count - 1)
            }
            (KeyCode::Char('e'), KeyModifiers::NONE)
                if self
                    .draft
                    .as_ref()
                    .is_some_and(|draft| self.selected < draft.entries.len()) =>
            {
                let mut editor = Editor::new();
                let text = self.draft.as_ref().unwrap().entries[self.selected]
                    .text
                    .clone();
                editor.replace_draft(text.clone(), text.len());
                self.editor = Some(editor);
                self.editing_repositories = false;
            }
            (KeyCode::Enter | KeyCode::Char(' '), _) => {
                if let Some(draft) = &self.draft {
                    if self.selected == draft.entries.len() {
                        return SetupEffect::Preview(
                            draft
                                .entries
                                .iter()
                                .zip(&self.included)
                                .filter(|(_, included)| **included)
                                .map(|(entry, _)| entry.clone())
                                .collect(),
                        );
                    }
                    self.included[self.selected] = !self.included[self.selected];
                } else {
                    match self.selected {
                        0 => {
                            self.options.purpose = match self.options.purpose {
                                AutoSetupPurpose::Development => AutoSetupPurpose::Research,
                                AutoSetupPurpose::Research => AutoSetupPurpose::Mixed,
                                AutoSetupPurpose::Mixed => AutoSetupPurpose::Development,
                            }
                        }
                        1 => self.options.scan_shell_history = !self.options.scan_shell_history,
                        2 => {
                            let mut editor = Editor::new();
                            let text = self
                                .options
                                .other_repositories
                                .iter()
                                .map(|path| path.display().to_string())
                                .collect::<Vec<_>>()
                                .join("\n");
                            editor.replace_draft(text.clone(), text.len());
                            self.editor = Some(editor);
                            self.editing_repositories = true;
                        }
                        _ => {
                            let id = tickloop_ids::generate();
                            self.generation = Some(id);
                            return SetupEffect::Generate(id, self.options.clone());
                        }
                    }
                }
            }
            _ => {}
        }
        SetupEffect::None
    }

    pub fn external_draft(&self) -> Option<String> {
        self.editor.as_ref().map(|editor| editor.draft().to_owned())
    }
    pub fn replace_external_draft(&mut self, text: String) {
        if let Some(editor) = &mut self.editor {
            let len = text.len();
            editor.replace_draft(text, len);
        }
    }

    pub fn paste(&mut self, text: &str) {
        if let Some(editor) = &mut self.editor {
            editor.apply(super::editor::EditorCommand::InsertPasted(text.to_owned()));
        }
    }

    pub fn lines(&self, width: usize) -> Vec<RoleLine> {
        let mut lines = vec![plain_row("Auto Environment setup · nothing applied")];
        if self.generation.is_some() {
            lines.push(muted_row("Generating draft… esc discards the result"));
            return lines;
        }
        if let Some(editor) = &self.editor {
            lines.push(plain_row(if self.editing_repositories {
                "Repository directories, one per line (empty disables scan)"
            } else {
                "Edit Environment fact"
            }));
            lines.push(plain_row(editor_window(editor, width)));
        } else if let Some(draft) = &self.draft {
            lines.push(muted_row(
                "Verify each fact and its scope. Defaults apply to the exact project directory.",
            ));
            for (index, entry) in draft.entries.iter().enumerate() {
                lines.push(selected_row(
                    format!(
                        "[{}] {}",
                        if self.included[index] { 'x' } else { ' ' },
                        entry.text
                    ),
                    self.selected == index,
                ));
                lines.push(muted_row(format!(
                    "Scope: {}{}",
                    entry
                        .directory
                        .as_ref()
                        .map_or_else(|| "Global".into(), |path| path.display().to_string()),
                    if entry.include_descendants {
                        " and descendants"
                    } else {
                        ""
                    }
                )));
                lines.push(muted_row(format!("Source: {}", entry.source)));
            }
            lines.extend(
                draft
                    .gaps
                    .iter()
                    .map(|gap| muted_row(format!("Not scanned: {gap}"))),
            );
            lines.push(selected_row(
                "Review selected additions",
                self.selected == draft.entries.len(),
            ));
        } else {
            lines.push(muted_row("Scan project metadata and Git remote identifiers. No credentials, scripts, or permission rules are generated."));
            lines.push(selected_row(
                format!("Purpose: {:?}", self.options.purpose),
                self.selected == 0,
            ));
            lines.push(selected_row(
                format!(
                    "Shell history (command names only): {}",
                    if self.options.scan_shell_history {
                        "on"
                    } else {
                        "off"
                    }
                ),
                self.selected == 1,
            ));
            lines.push(selected_row(
                format!(
                    "Other repositories: {} selected",
                    self.options.other_repositories.len()
                ),
                self.selected == 2,
            ));
            lines.push(selected_row(
                "Continue · generate draft",
                self.selected == 3,
            ));
        }
        if let Some(message) = &self.message {
            lines.push(plain_row(message));
        }
        lines
    }
}
