use super::{
    conversation_picker::{
        ConversationEntry, ConversationPicker, ConversationPickerEffect, ConversationPickerFocus,
        ConversationPickerPurpose, fit_hint,
    },
    display::{DisplayProfile, Glyph},
    permissions_surface::tab_header_line,
    style::{Role, RoleLine},
};
use crate::artifacts::{ArtifactMetadata, ArtifactPage};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::{
    collections::HashSet,
    time::{Duration, Instant},
};
use uuid::Uuid;

const SEARCH_DEBOUNCE: Duration = Duration::from_millis(250);
const TABS: [&str; 4] = ["All", "Mine", "Shared", "Pinned"];

#[derive(Clone)]
pub(super) struct ArtifactLoad {
    pub(super) picker: Uuid,
    pub(super) generation: u64,
    pub(super) ownership: &'static str,
    pub(super) pinned: bool,
    pub(super) query: String,
    pub(super) cursor: Option<String>,
}
#[derive(Clone, Debug)]
pub(super) enum ArtifactPickerAction {
    Add,
    Open,
    Copy,
    Pin(bool),
    Rename(String),
    Delete,
}
#[derive(Clone, Debug)]
pub(super) struct ArtifactChoice {
    pub(super) picker: Uuid,
    pub(super) artifact: ArtifactMetadata,
    pub(super) action: ArtifactPickerAction,
}

pub(super) enum ArtifactPickerEffect {
    None,
    Close,
    Choice(Box<ArtifactChoice>),
    EditQuery,
}

pub(super) struct ArtifactsPicker {
    picker: ConversationPicker,
    pub(super) chat: Option<Uuid>,
    email: String,
    entries: Vec<ArtifactMetadata>,
    added: HashSet<String>,
    pinned: HashSet<String>,
    tab: usize,
    generation: u64,
    load_at: Option<Instant>,
    loading: bool,
    next_cursor: Option<String>,
    cursor: Option<String>,
    error: Option<String>,
    top: usize,
    rename: Option<(ArtifactMetadata, ConversationPicker)>,
    delete: Option<ArtifactMetadata>,
}

impl ArtifactsPicker {
    pub(super) fn new(chat: Option<Uuid>, email: String, added: HashSet<String>) -> Self {
        Self {
            picker: ConversationPicker::new(ConversationPickerPurpose::Resume, Vec::new()),
            chat,
            email,
            entries: Vec::new(),
            added,
            pinned: HashSet::new(),
            tab: 0,
            generation: 0,
            load_at: Some(Instant::now()),
            loading: false,
            next_cursor: None,
            cursor: None,
            error: None,
            top: 0,
            rename: None,
            delete: None,
        }
    }
    pub(super) fn id(&self) -> Uuid {
        self.picker.id()
    }
    fn reload(&mut self, debounce: bool) {
        self.generation += 1;
        self.load_at = Some(
            Instant::now()
                + if debounce {
                    SEARCH_DEBOUNCE
                } else {
                    Duration::ZERO
                },
        );
        self.loading = false;
        self.cursor = None;
        self.next_cursor = None;
        self.error = None;
    }
    pub(super) fn take_load(&mut self) -> Option<ArtifactLoad> {
        if self.loading || self.load_at.is_none_or(|at| at > Instant::now()) {
            return None;
        }
        self.load_at = None;
        self.loading = true;
        Some(ArtifactLoad {
            picker: self.id(),
            generation: self.generation,
            ownership: ["all", "owned", "shared", "all"][self.tab],
            pinned: self.tab == 3,
            query: self.picker.query().into(),
            cursor: self.cursor.clone(),
        })
    }
    pub(super) fn loaded(
        &mut self,
        request: &ArtifactLoad,
        result: Result<(ArtifactPage, HashSet<String>, bool), String>,
    ) {
        if request.picker != self.id() || request.generation != self.generation {
            return;
        }
        self.loading = false;
        match result {
            Err(error) => self.error = Some(error),
            Ok((mut page, pinned, reset)) => {
                self.pinned = pinned;
                self.next_cursor = page.next_cursor.take();
                if request.cursor.is_none() || reset {
                    self.entries.clear();
                }
                for entry in page.artifacts {
                    if let Some(existing) = self
                        .entries
                        .iter_mut()
                        .find(|existing| existing.id == entry.id)
                    {
                        *existing = entry;
                    } else {
                        self.entries.push(entry);
                    }
                }
                self.entries.sort_by(|a, b| {
                    self.added
                        .contains(&b.id)
                        .cmp(&self.added.contains(&a.id))
                        .then_with(|| b.updated_at.cmp(&a.updated_at))
                        .then_with(|| a.id.cmp(&b.id))
                });
                self.rebuild();
            }
        }
    }
    fn rebuild(&mut self) {
        let entries = self
            .entries
            .iter()
            .map(|entry| ConversationEntry::new(&entry.title, &[], &[]))
            .collect();
        self.picker.replace_entries(entries);
    }
    fn selected(&self) -> Option<&ArtifactMetadata> {
        self.picker
            .selection()
            .and_then(|index| self.entries.get(index))
    }
    fn choice(
        &self,
        artifact: ArtifactMetadata,
        action: ArtifactPickerAction,
    ) -> ArtifactPickerEffect {
        ArtifactPickerEffect::Choice(Box::new(ArtifactChoice {
            picker: self.id(),
            artifact,
            action,
        }))
    }
    pub(super) fn external_query(&self) -> String {
        self.rename
            .as_ref()
            .map(|(_, picker)| picker.query())
            .unwrap_or(self.picker.query())
            .into()
    }
    pub(super) fn replace_query(&mut self, text: &str) {
        if let Some((_, picker)) = &mut self.rename {
            picker.replace_query_text(text);
        } else {
            self.picker.replace_query_text(text);
            if self.tab != 3 {
                self.reload(true);
            }
        }
    }
    pub(super) fn paste(&mut self, text: &str) {
        if let Some((_, picker)) = &mut self.rename {
            picker.paste(text);
        } else {
            self.picker.paste(text);
            if self.tab != 3 {
                self.reload(true);
            }
        }
    }
    pub(super) fn key(&mut self, key: KeyEvent) -> ArtifactPickerEffect {
        if let Some(artifact) = self.delete.clone() {
            if key.code == KeyCode::Esc {
                self.delete = None;
            } else if key.code == KeyCode::Enter {
                self.delete = None;
                return self.choice(artifact, ArtifactPickerAction::Delete);
            }
            return ArtifactPickerEffect::None;
        }
        if let Some((artifact, picker)) = &mut self.rename {
            if key.code == KeyCode::Esc {
                self.rename = None;
                return ArtifactPickerEffect::None;
            }
            if key.code == KeyCode::Enter {
                let artifact = artifact.clone();
                let title = picker.query().trim().to_owned();
                if title.is_empty() {
                    self.error = Some("Title cannot be empty".into());
                    return ArtifactPickerEffect::None;
                }
                self.rename = None;
                return self.choice(artifact, ArtifactPickerAction::Rename(title));
            }
            return if picker.key(key) == ConversationPickerEffect::EditQuery {
                ArtifactPickerEffect::EditQuery
            } else {
                ArtifactPickerEffect::None
            };
        }
        if self.picker.focus() == ConversationPickerFocus::Search && key.code == KeyCode::Esc {
            self.picker.focus_list();
            return ArtifactPickerEffect::None;
        }
        if self.picker.focus() == ConversationPickerFocus::List {
            let delta = match key.code {
                KeyCode::Left | KeyCode::BackTab => Some(-1),
                KeyCode::Right | KeyCode::Tab => Some(1),
                _ => None,
            };
            if let Some(delta) = delta {
                self.tab = (self.tab as isize + delta).rem_euclid(4) as usize;
                self.entries.clear();
                self.rebuild();
                self.reload(false);
                return ArtifactPickerEffect::None;
            }
            if key.code == KeyCode::Char('r') && key.modifiers == KeyModifiers::NONE {
                self.reload(false);
                return ArtifactPickerEffect::None;
            }
            if let Some(artifact) = self.selected().cloned() {
                match (key.code, key.modifiers) {
                    (KeyCode::Char('o'), KeyModifiers::NONE) => {
                        return self.choice(artifact, ArtifactPickerAction::Open);
                    }
                    (KeyCode::Char('c'), KeyModifiers::NONE) => {
                        return self.choice(artifact, ArtifactPickerAction::Copy);
                    }
                    (KeyCode::Char('p'), KeyModifiers::NONE) => {
                        let pinned = !self.pinned.contains(&artifact.id);
                        return self.choice(artifact, ArtifactPickerAction::Pin(pinned));
                    }
                    (KeyCode::Char('r'), KeyModifiers::CONTROL)
                        if matches!(artifact.access_role.as_str(), "owner" | "editor") =>
                    {
                        let mut picker =
                            ConversationPicker::new(ConversationPickerPurpose::Resume, Vec::new());
                        picker.replace_query_text(&artifact.title);
                        self.rename = Some((artifact, picker));
                        return ArtifactPickerEffect::None;
                    }
                    (KeyCode::Char('d'), KeyModifiers::NONE) if artifact.access_role == "owner" => {
                        self.delete = Some(artifact);
                        return ArtifactPickerEffect::None;
                    }
                    (KeyCode::Char('d'), KeyModifiers::NONE)
                    | (KeyCode::Char('r'), KeyModifiers::CONTROL) => {
                        self.error = Some("Your role does not allow this operation".into());
                        return ArtifactPickerEffect::None;
                    }
                    _ => {}
                }
            }
        }
        let before = self.picker.query().to_owned();
        match self.picker.key(key) {
            ConversationPickerEffect::Open(index) => {
                if let Some(artifact) = self.entries.get(index) {
                    return self.choice(artifact.clone(), ArtifactPickerAction::Add);
                }
            }
            ConversationPickerEffect::Cancel => return ArtifactPickerEffect::Close,
            ConversationPickerEffect::EditQuery => return ArtifactPickerEffect::EditQuery,
            _ => {}
        }
        if before != self.picker.query() && self.tab != 3 {
            self.reload(true);
        }
        let at_end = self
            .picker
            .selection()
            .is_some_and(|index| Some(&index) == self.picker.matching_indices().last());
        if at_end
            && !self.loading
            && self.load_at.is_none()
            && let Some(cursor) = self.next_cursor.take()
        {
            self.cursor = Some(cursor);
            self.load_at = Some(Instant::now());
        }
        ArtifactPickerEffect::None
    }
    pub(super) fn report(&mut self, message: String) {
        self.error = Some(message);
    }
    pub(super) fn refresh(&mut self) {
        self.reload(false);
    }
    pub(super) fn frame(
        &mut self,
        width: usize,
        height: usize,
        display: DisplayProfile,
    ) -> super::conversation_picker::ConversationPickerFrame {
        let mut header = tab_header_line("Artifacts", self.tab, &TABS, width);
        let spare = width.saturating_sub(
            unicode_width::UnicodeWidthStr::width(header.text().as_str()) + self.email.len() + 2,
        );
        if unicode_width::UnicodeWidthStr::width(header.text().as_str()) + self.email.len() + 2
            <= width
        {
            header.push(" ".repeat(spare + 2), None);
            header.push(&self.email, Some(Role::MutedText));
        }
        let mut lines = vec![header];
        let (search, column) = self.picker.search_line(width);
        let mut cursor =
            (self.picker.focus() == ConversationPickerFocus::Search).then_some((1, column));
        lines.push(search);
        let mut rows = Vec::new();
        let mut selected_row = 0;
        let mut previous_group = None;
        let now = crate::private_fs::unix_time_milliseconds();
        for index in self.picker.matching_indices() {
            let artifact = &self.entries[*index];
            let added = self.added.contains(&artifact.id);
            if previous_group != Some(added) {
                rows.push(RoleLine::role(
                    if added {
                        "This Chat"
                    } else {
                        "Other artifacts"
                    },
                    Role::MutedText,
                ));
                previous_group = Some(added);
            }
            let selected = self.picker.selection() == Some(*index);
            if selected {
                selected_row = rows.len();
            }
            rows.push(RoleLine::role(
                format!(
                    "{}{} {}{}",
                    if selected { "› " } else { "  " },
                    display.glyph(Glyph::ArtifactTick),
                    super::safe_line(&artifact.title),
                    if self.pinned.contains(&artifact.id) {
                        " ★"
                    } else {
                        ""
                    }
                ),
                if selected {
                    Role::Selection
                } else {
                    Role::Text
                },
            ));
            let updated = artifact
                .updated_at
                .as_deref()
                .and_then(crate::transcript::parse_timestamp_ms)
                .map(|value| value.max(0) as u64);
            rows.push(RoleLine::role(
                format!(
                    "    {} · {}{}",
                    artifact.access_role,
                    super::tick_list::relative_time(updated, now),
                    if artifact.status == "reserved" {
                        " · reserved"
                    } else {
                        " ago"
                    }
                ),
                Role::MutedText,
            ));
        }
        let capacity = height.saturating_sub(3 + usize::from(self.rename.is_some()));
        if selected_row < self.top {
            self.top = selected_row;
        }
        if selected_row + 2 > self.top + capacity {
            self.top = (selected_row + 2).saturating_sub(capacity);
        }
        if rows.is_empty() {
            rows.push(RoleLine::role(
                if self.loading {
                    "Loading…"
                } else {
                    "No artifacts"
                },
                Role::MutedText,
            ));
        }
        lines.extend(rows.into_iter().skip(self.top).take(capacity));
        while lines.len() < height.saturating_sub(1 + usize::from(self.rename.is_some())) {
            lines.push(RoleLine::default());
        }
        if let Some((_, rename)) = &self.rename {
            let (line, column) = rename.search_line(width);
            cursor = Some((lines.len(), column));
            lines.push(line);
        }
        let hint = if self.delete.is_some() {
            "Delete permanently · 无法撤销 · Enter confirm · Esc cancel".into()
        } else if self.rename.is_some() {
            "Rename · Enter save · Ctrl+G editor · Esc cancel".into()
        } else if let Some(error) = &self.error {
            format!("{error} · r retry")
        } else if self.loading {
            "Loading… · Esc close".into()
        } else {
            fit_hint(
                &[
                    ("Enter add".into(), 10),
                    ("/ search".into(), 7),
                    ("o open · c copy · p pin".into(), 5),
                    ("Ctrl+R rename · d delete · r refresh".into(), 4),
                    ("Esc close".into(), 9),
                ],
                width,
                display,
            )
        };
        lines.push(RoleLine::role(hint, Role::MutedText));
        lines.truncate(height);
        for line in &mut lines {
            line.truncate(width);
        }
        super::conversation_picker::ConversationPickerFrame { lines, cursor }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pinned_search_matches_titles_only_and_hides_basic_navigation_hints() {
        let mut picker = ArtifactsPicker::new(None, "owner@example.test".into(), HashSet::new());
        for _ in 0..3 {
            picker.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        }
        load(
            &mut picker,
            vec![
                artifact("a", "Research", "owner"),
                artifact("b", "Owner notes", "viewer"),
            ],
        );
        picker.replace_query("owner");
        assert_eq!(picker.selected().map(|entry| entry.id.as_str()), Some("b"));
        picker.replace_query("research");
        assert_eq!(picker.selected().map(|entry| entry.id.as_str()), Some("a"));
        let frame = picker.frame(180, 20, DisplayProfile::default());
        assert!(
            !frame
                .lines
                .iter()
                .any(|line| line.text().contains("←/→ tabs"))
        );
    }
    fn artifact(id: &str, title: &str, role: &str) -> ArtifactMetadata {
        serde_json::from_value(serde_json::json!({"id":id,"title":title,"format":"markdown","access_role":role,"url":format!("https://docs.example/a/{id}"),"updated_at":"2026-10-06T00:00:00Z"})).unwrap()
    }
    fn load(picker: &mut ArtifactsPicker, rows: Vec<ArtifactMetadata>) {
        let request = picker.take_load().unwrap();
        picker.loaded(
            &request,
            Ok((
                ArtifactPage {
                    artifacts: rows,
                    next_cursor: None,
                },
                HashSet::new(),
                false,
            )),
        );
    }
    #[test]
    fn picker_partitions_rows_switches_tabs_and_drops_previous_generation() {
        let mut picker = ArtifactsPicker::new(
            None,
            "owner@example.test".into(),
            HashSet::from(["here".into()]),
        );
        load(
            &mut picker,
            vec![
                artifact("other", "Other", "owner"),
                artifact("here", "Here", "editor"),
            ],
        );
        let frame = picker.frame(100, 20, DisplayProfile::default());
        let text = frame
            .lines
            .iter()
            .map(RoleLine::text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("[All]") && text.contains("owner@example.test"));
        assert!(text.find("This Chat").unwrap() < text.find("Other artifacts").unwrap());
        picker.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        let old = picker.take_load().unwrap();
        picker.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        picker.loaded(
            &old,
            Ok((
                ArtifactPage {
                    artifacts: vec![artifact("stale", "Stale", "owner")],
                    next_cursor: None,
                },
                HashSet::new(),
                false,
            )),
        );
        assert!(picker.entries.is_empty());
        assert!(
            picker.frame(100, 20, DisplayProfile::default()).lines[0]
                .text()
                .contains("[Shared]")
        );
    }
    #[test]
    fn picker_search_escape_returns_to_list_and_delete_requires_owner_confirmation() {
        let mut picker = ArtifactsPicker::new(None, String::new(), HashSet::new());
        load(&mut picker, vec![artifact("doc", "Doc", "owner")]);
        picker.key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        assert!(matches!(
            picker.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            ArtifactPickerEffect::None
        ));
        picker.key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        assert!(
            picker
                .frame(80, 12, DisplayProfile::default())
                .lines
                .iter()
                .any(|line| line.text().contains("无法撤销"))
        );
        assert!(matches!(
            picker.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            ArtifactPickerEffect::Choice(choice) if matches!(choice.action, ArtifactPickerAction::Delete)
        ));
        assert!(matches!(
            picker.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            ArtifactPickerEffect::Close
        ));
    }
    #[test]
    fn pinned_search_filters_locally_and_viewers_cannot_rename() {
        let mut picker = ArtifactsPicker::new(None, String::new(), HashSet::new());
        picker.tab = 3;
        load(
            &mut picker,
            vec![
                artifact("a", "Apple", "viewer"),
                artifact("b", "Banana", "owner"),
            ],
        );
        picker.replace_query("Apple");
        assert!(picker.take_load().is_none());
        assert_eq!(picker.picker.matching_indices(), &[0]);
        picker.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        picker.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        assert!(picker.rename.is_none());
        assert!(picker.error.as_deref().unwrap().contains("role"));
    }
}
