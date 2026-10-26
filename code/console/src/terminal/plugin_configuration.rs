use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::Value;
use std::collections::BTreeMap;
use tick_plugins::{PluginManifest, UserConfigField};
use tickloop_code_settings::plugin_config::{PluginUserConfig, parse_input};
use unicode_width::UnicodeWidthStr;

use super::{
    console::wrap_text,
    editor::{Editor, EditorCommand},
    permissions_surface::{edit_text, editor_window},
    safe_line,
    style::{Role, RoleLine},
};

#[derive(Clone, Default, PartialEq, Eq)]
pub(super) struct ConfigInput(pub BTreeMap<String, Value>);
impl std::fmt::Debug for ConfigInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ConfigInput { .. }")
    }
}

pub(super) enum ConfigAction {
    None,
    Cancel,
    Save,
    ExternalEditor,
}

pub(super) struct ConfigurationForm {
    pub name: String,
    fields: Vec<(String, UserConfigField)>,
    saved: PluginUserConfig,
    index: usize,
    pub editor: Editor,
    pub input: ConfigInput,
    error: Option<String>,
}

impl ConfigurationForm {
    pub fn new(
        manifest: &PluginManifest,
        saved: PluginUserConfig,
        only_missing: bool,
    ) -> Option<Self> {
        let fields = manifest
            .user_config
            .iter()
            .filter(|(key, _)| !only_missing || !saved.is_filled(key))
            .map(|(key, field)| (key.clone(), field.clone()))
            .collect::<Vec<_>>();
        if fields.is_empty() {
            return None;
        }
        Some(Self {
            name: manifest.name.clone(),
            fields,
            saved,
            index: 0,
            editor: Editor::new(),
            input: ConfigInput::default(),
            error: None,
        })
    }

    pub fn paste(&mut self, text: &str) {
        self.editor.apply(EditorCommand::InsertPasted(
            text.chars().filter(|c| !c.is_control()).collect(),
        ));
    }

    pub fn key(&mut self, key: KeyEvent) -> ConfigAction {
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => ConfigAction::Cancel,
            (KeyCode::Char('g'), KeyModifiers::CONTROL) => {
                if self.fields[self.index].1.sensitive {
                    self.error =
                        Some("Sensitive values must be entered in this masked field".into());
                    ConfigAction::None
                } else {
                    ConfigAction::ExternalEditor
                }
            }
            (KeyCode::Enter, _) => {
                let (name, field) = &self.fields[self.index];
                let text = self.editor.draft();
                if !text.is_empty() || (!self.saved.is_filled(name) && field.required) {
                    match parse_input(field, text) {
                        Ok(value) => {
                            self.input.0.insert(name.clone(), value);
                        }
                        Err(error) => {
                            self.error = Some(error.into());
                            return ConfigAction::None;
                        }
                    }
                }
                if self.index + 1 == self.fields.len() {
                    return ConfigAction::Save;
                }
                self.index += 1;
                self.editor = Editor::new();
                self.error = None;
                ConfigAction::None
            }
            _ => {
                edit_text(&mut self.editor, key);
                ConfigAction::None
            }
        }
    }

    pub fn lines(&self, width: usize) -> (Vec<RoleLine>, usize) {
        let (name, field) = &self.fields[self.index];
        let mut lines = vec![RoleLine::role(
            format!("Configure {}", safe_line(&self.name)),
            Role::Text,
        )];
        for text in [
            field.title.as_deref().unwrap_or(name),
            field.description.as_deref().unwrap_or(""),
        ] {
            lines.extend(
                wrap_text(&safe_line(text), width.max(1))
                    .into_iter()
                    .map(|line| RoleLine::role(line, Role::MutedText)),
            );
        }
        if let Some(value) = self.saved.display_value(name) {
            lines.extend(
                wrap_text(&format!("Enter keeps {}", safe_line(&value)), width.max(1))
                    .into_iter()
                    .map(|line| RoleLine::role(line, Role::MutedText)),
            );
        }
        if let Some(error) = &self.error {
            lines.extend(
                wrap_text(error, width.max(1))
                    .into_iter()
                    .map(|line| RoleLine::role(line, Role::Error)),
            );
        }
        let input = if field.sensitive {
            "*".repeat(self.editor.draft().chars().count())
                .chars()
                .rev()
                .take(width.saturating_sub(2))
                .collect::<String>()
        } else {
            editor_window(&self.editor, width.saturating_sub(2))
        };
        let column = (2 + input.width()).min(width.saturating_sub(1));
        lines.push(RoleLine::role(format!("> {input}"), Role::Text));
        (lines, column)
    }
}
