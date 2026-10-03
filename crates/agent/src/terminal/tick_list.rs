use std::collections::{HashMap, HashSet};

use super::{
    display::{DisplayProfile, Glyph},
    style::{Role, RoleLine},
};

pub(super) const TICK_LIST_COLUMNS: usize = 42;
pub(super) const TICK_LIST_AUTO_MIN_COLUMNS: usize = 121;

pub(super) use crate::tick_adapter::TickGroup;
use crate::tick_adapter::{
    TickAdapters, TickDescriptor, TickGlyph, TickListAction, TickListContext,
};

#[derive(Clone, Debug)]
pub(super) struct TickListEntry {
    pub(super) id: String,
    pub(super) group: TickGroup,
    pub(super) glyph: Glyph,
    pub(super) title: String,
    pub(super) kind: String,
    pub(super) status: String,
    pub(super) updated_at_ms: Option<u64>,
    pub(super) actions: Vec<TickListAction>,
}

#[derive(Clone)]
pub(super) struct TickConfirmation {
    pub(super) instance: String,
    pub(super) action: std::sync::Arc<dyn crate::tick_adapter::TickActionExecutor>,
    pub(super) prompt: String,
}
#[derive(Default)]
pub(super) struct TickList {
    pub(super) detail: Option<Box<dyn super::tick_actions::TickDetailView>>,
    pub(super) subscriptions: Vec<crate::runtime::TickSubscriptionStatus>,
    pub(super) metadata: HashMap<String, Option<crate::artifacts::ArtifactMetadata>>,
    pub(super) metadata_failures: HashMap<String, String>,
    pub(super) confirmation: Option<TickConfirmation>,
    entries: Vec<TickListEntry>,
    selected: Option<String>,
    removed_instances: HashSet<String>,
    adapters: Option<TickAdapters>,
    pub(super) chat_snapshot: Option<crate::runtime::ChatSnapshot>,
    pub(super) shell_snapshot: Option<crate::runtime::ShellTaskSnapshot>,
    pub(super) focused: bool,
    pub(super) overlay: bool,
}

pub(super) struct TickListFrame {
    pub(super) lines: Vec<RoleLine>,
    pub(super) targets: Vec<(usize, String)>,
}

impl TickList {
    pub(super) fn refresh_action(
        &mut self,
    ) -> std::sync::Arc<dyn crate::tick_adapter::TickActionExecutor> {
        let actions = self
            .adapters
            .get_or_insert_with(TickAdapters::presentation)
            .refresh_actions();
        super::tick_actions::builtin::refresh(actions)
    }
    pub(super) fn entries(&self) -> &[TickListEntry] {
        &self.entries
    }
    pub(super) fn selected(&self) -> Option<&TickListEntry> {
        self.selected
            .as_ref()
            .and_then(|id| self.entries.iter().find(|entry| &entry.id == id))
    }
    pub(super) fn rebuild(&mut self) {
        let context = TickListContext {
            chat: self.chat_snapshot.clone(),
            shells: self.shell_snapshot.clone(),
            subscriptions: self.subscriptions.clone(),
            metadata: self.metadata.clone(),
            metadata_failures: self.metadata_failures.clone(),
        };
        self.entries = self
            .adapters
            .get_or_insert_with(TickAdapters::presentation)
            .list(&context)
            .into_iter()
            .map(TickListEntry::from)
            .filter(|entry| !self.removed_instances.contains(&entry.id))
            .collect();
        self.reconcile_selection();
    }
    #[cfg(test)]
    pub(super) fn replace(&mut self, group: TickGroup, rows: Vec<TickListEntry>) {
        self.entries.retain(|entry| entry.group != group);
        self.entries.extend(
            rows.into_iter()
                .filter(|entry| !self.removed_instances.contains(&entry.id)),
        );
        self.entries.sort_by(|a, b| {
            a.group
                .cmp(&b.group)
                .then_with(|| b.updated_at_ms.cmp(&a.updated_at_ms))
                .then_with(|| a.id.cmp(&b.id))
        });
        self.entries.dedup_by(|a, b| a.id == b.id);
        self.reconcile_selection();
    }
    fn reconcile_selection(&mut self) {
        if self.entries.is_empty() {
            self.detail = None;
            self.focused = false;
            self.overlay = false;
        }
        if self.selected().is_none() {
            self.selected = self.entries.first().map(|entry| entry.id.clone());
        }
    }
    pub(super) fn select(&mut self, id: &str) {
        if self.entries.iter().any(|entry| entry.id == id) {
            self.selected = Some(id.into());
        }
    }
    pub(super) fn focus(&mut self, overlay: bool, group: Option<TickGroup>) {
        self.focused = true;
        self.overlay = overlay;
        if let Some(group) = group
            && let Some(entry) = self.entries.iter().find(|entry| entry.group == group)
        {
            self.selected = Some(entry.id.clone());
        }
        if self.selected.is_none() {
            self.selected = self.entries.first().map(|entry| entry.id.clone());
        }
    }
    pub(super) fn move_selection(&mut self, delta: isize) {
        let index = self
            .entries
            .iter()
            .position(|entry| Some(&entry.id) == self.selected.as_ref())
            .unwrap_or(0);
        if let Some(entry) = self.entries.get(
            index
                .saturating_add_signed(delta)
                .min(self.entries.len().saturating_sub(1)),
        ) {
            self.selected = Some(entry.id.clone());
        }
    }
    pub(super) fn remove_local(&mut self, id: &str) {
        self.removed_instances.insert(id.into());
        self.entries.retain(|entry| entry.id != id);
        self.reconcile_selection();
    }
    #[cfg(test)]
    pub(super) fn lines(
        &self,
        width: usize,
        height: usize,
        display: DisplayProfile,
        now_ms: u64,
    ) -> Vec<RoleLine> {
        self.frame(width, height, display, now_ms).lines
    }

    pub(super) fn frame(
        &self,
        width: usize,
        height: usize,
        display: DisplayProfile,
        now_ms: u64,
    ) -> TickListFrame {
        let mut lines = vec![RoleLine::role(
            "Ticks",
            if self.focused {
                Role::Text
            } else {
                Role::MutedText
            },
        )];

        let mut rows = Vec::new();
        let mut row_ids = Vec::new();
        let mut selected_row = 0;
        let groups = self
            .entries
            .iter()
            .map(|entry| entry.group)
            .collect::<std::collections::BTreeSet<_>>();
        for group in groups {
            let entries: Vec<_> = self
                .entries
                .iter()
                .filter(|entry| entry.group == group)
                .collect();
            if entries.is_empty() {
                continue;
            }
            rows.push(RoleLine::role(
                format!("{} ({})", group.title(), entries.len()),
                Role::MutedText,
            ));
            row_ids.push(None);
            for entry in entries {
                let selected = self.focused && self.selected.as_deref() == Some(&entry.id);
                if selected {
                    selected_row = rows.len();
                }
                rows.push(RoleLine::role(
                    format!(
                        "{}{} {}",
                        if selected { "› " } else { "  " },
                        display.glyph(entry.glyph),
                        super::safe_line(&entry.title)
                    ),
                    if selected {
                        Role::Text
                    } else {
                        Role::MutedText
                    },
                ));
                row_ids.push(Some(entry.id.clone()));
                rows.push(RoleLine::role(
                    format!(
                        "    {}{} · {}",
                        entry.kind,
                        if entry.status.is_empty() {
                            String::new()
                        } else {
                            format!(" · {}", entry.status)
                        },
                        relative_time(entry.updated_at_ms, now_ms)
                    ),
                    Role::MutedText,
                ));
                row_ids.push(Some(entry.id.clone()));
            }
        }
        let capacity = height.saturating_sub(2);
        let offset = selected_row.saturating_sub(capacity.saturating_sub(2));
        let targets = row_ids
            .into_iter()
            .skip(offset)
            .take(capacity)
            .enumerate()
            .filter_map(|(row, id)| id.map(|id| (row + 1, id)))
            .collect();
        lines.extend(rows.into_iter().skip(offset).take(capacity));
        if self.focused && height > 2 {
            let mut hints = self
                .selected()
                .map(|entry| {
                    entry
                        .actions
                        .iter()
                        .map(|action| {
                            format!(
                                "{} {}",
                                if action.key == 'o' {
                                    "Enter".into()
                                } else {
                                    action.key.to_string()
                                },
                                action.label
                            )
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            hints.push("Esc".into());
            lines.push(RoleLine::role(hints.join(" · "), Role::MutedText));
        }
        lines.truncate(height);
        for line in &mut lines {
            line.truncate(width);
        }
        TickListFrame { lines, targets }
    }
}

pub(super) fn relative_time(updated: Option<u64>, now: u64) -> String {
    let Some(updated) = updated else {
        return "—".into();
    };
    let seconds = now.saturating_sub(updated) / 1000;
    match seconds {
        0 => "now".into(),
        1..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        3600..86400 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn elapsed_update_time_uses_the_specified_units() {
        for (updated, expected) in [
            (None, "—"),
            (Some(90000000), "now"),
            (Some(89955000), "45s"),
            (Some(89820000), "3m"),
            (Some(82800000), "2h"),
            (Some(3600000), "1d"),
        ] {
            assert_eq!(relative_time(updated, 90000000), expected);
        }
    }
}

impl From<TickDescriptor> for TickListEntry {
    fn from(row: TickDescriptor) -> Self {
        Self {
            id: row.instance.to_string(),
            group: row.group,
            glyph: match row.glyph {
                TickGlyph::Artifact => Glyph::ArtifactTick,
                TickGlyph::Shell => Glyph::ShellTick,
                TickGlyph::Protocol => Glyph::ProtocolTick,
                TickGlyph::Terminal => Glyph::TerminalTick,
            },
            title: row.title,
            kind: row.kind,
            status: row.status,
            updated_at_ms: row.updated_at_ms,
            actions: row.actions,
        }
    }
}

#[cfg(test)]
mod adapter_tests {
    use super::*;
    use crate::tick_adapter::{TickAdapter, TickListOperation};
    use std::sync::Arc;

    struct Calendar;
    impl TickAdapter for Calendar {
        fn tick_type(&self) -> &'static str {
            "calendar"
        }
        fn list(&self, _: &TickListContext) -> Vec<TickDescriptor> {
            vec![TickDescriptor {
                instance: crate::tick_stream::InstanceRef::new("calendar", "earnings").unwrap(),
                group: TickGroup {
                    order: 4,
                    name: "Calendars",
                },
                glyph: TickGlyph::Protocol,
                title: "Earnings calendar".into(),
                kind: "Calendar".into(),
                status: "waiting".into(),
                updated_at_ms: None,
                running: true,
                actions: [('o', "inspect"), ('z', "expand")]
                    .into_iter()
                    .map(|(key, label)| TickListAction {
                        key,
                        label,
                        operation: TickListOperation::Notify("opened".into()),
                    })
                    .collect(),
            }]
        }
        fn stream(&self) -> Option<Arc<dyn crate::tick_stream::TickStreamPort>> {
            None
        }
    }
    #[test]
    fn registered_adapter_groups_rows_and_hints_render_without_list_type_changes() {
        let mut list = TickList {
            adapters: Some(
                TickAdapters::new([Arc::new(Calendar) as Arc<dyn TickAdapter>]).unwrap(),
            ),
            ..Default::default()
        };
        list.rebuild();
        list.focus(false, None);
        let text = list
            .lines(80, 20, DisplayProfile::default(), 0)
            .iter()
            .map(RoleLine::text)
            .collect::<Vec<_>>()
            .join("\n");
        for expected in [
            "Calendars (1)",
            "Earnings calendar",
            "Calendar · waiting · —",
            "Enter inspect · z expand · Esc",
        ] {
            assert!(text.contains(expected), "{text}");
        }
        assert!(!text.contains("Subscribed"));
    }
}
