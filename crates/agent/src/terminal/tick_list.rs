use std::collections::{HashMap, HashSet};

use super::{
    display::{DisplayProfile, Glyph},
    style::{Role, RoleLine},
};

pub(super) const TICK_LIST_COLUMNS: usize = 42;
pub(super) const TICK_LIST_AUTO_MIN_COLUMNS: usize = 121;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TickGroup {
    Artifacts,
    Shells,
    Subscribed,
}

impl TickGroup {
    pub(super) fn title(self) -> &'static str {
        match self {
            Self::Artifacts => "Artifacts",
            Self::Shells => "Shells",
            Self::Subscribed => "Subscribed",
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct TickListEntry {
    pub(super) id: String,
    pub(super) group: TickGroup,
    pub(super) glyph: Glyph,
    pub(super) title: String,
    pub(super) kind: String,
    pub(super) status: String,
    pub(super) updated_at_ms: Option<u64>,
    pub(super) running: bool,
}

#[derive(Default)]
pub(super) struct TickList {
    pub(super) shell_detail: Option<super::tick_shell::TickShell>,
    pub(super) subscriptions: Vec<crate::runtime::TickSubscriptionStatus>,
    pub(super) metadata: HashMap<String, Option<crate::artifacts::ArtifactMetadata>>,
    pub(super) metadata_failures: HashMap<String, String>,
    pub(super) confirmation: Option<String>,
    entries: Vec<TickListEntry>,
    selected: Option<String>,
    removed_shells: HashSet<String>,
    pub(super) focused: bool,
    pub(super) overlay: bool,
}

pub(super) struct TickListFrame {
    pub(super) lines: Vec<RoleLine>,
    pub(super) targets: Vec<(usize, String)>,
}

impl TickList {
    pub(super) fn entries(&self) -> &[TickListEntry] {
        &self.entries
    }
    pub(super) fn selected(&self) -> Option<&TickListEntry> {
        self.selected
            .as_ref()
            .and_then(|id| self.entries.iter().find(|entry| &entry.id == id))
    }
    pub(super) fn replace(&mut self, group: TickGroup, rows: Vec<TickListEntry>) {
        self.entries.retain(|entry| entry.group != group);
        self.entries.extend(rows.into_iter().filter(|entry| {
            group != TickGroup::Shells || !self.removed_shells.contains(&entry.id)
        }));
        let joined: HashSet<_> = self
            .entries
            .iter()
            .filter(|entry| entry.group == TickGroup::Artifacts)
            .map(|entry| entry.id.clone())
            .collect();
        self.entries.retain(|entry| {
            if entry.group != TickGroup::Subscribed {
                return true;
            }
            let subscription_id = uuid::Uuid::parse_str(&entry.id).ok();
            !self.subscriptions.iter().any(|row| {
                Some(row.id) == subscription_id && joined.contains(row.instance.as_str())
            })
        });
        self.entries.sort_by(|a, b| {
            let order = |group| match group {
                TickGroup::Artifacts => 0,
                TickGroup::Shells => 1,
                TickGroup::Subscribed => 2,
            };
            order(a.group)
                .cmp(&order(b.group))
                .then_with(|| b.updated_at_ms.cmp(&a.updated_at_ms))
                .then_with(|| a.id.cmp(&b.id))
        });
        if self.entries.is_empty() {
            self.shell_detail = None;
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
    pub(super) fn remove_shell(&mut self, id: &str) {
        self.removed_shells.insert(id.into());
        self.entries.retain(|entry| entry.id != id);
        self.selected = self.entries.first().map(|entry| entry.id.clone());
        if self.entries.is_empty() {
            self.focused = false;
            self.overlay = false;
        }
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
        for group in [
            TickGroup::Artifacts,
            TickGroup::Shells,
            TickGroup::Subscribed,
        ] {
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
            let artifact = self
                .selected()
                .is_some_and(|entry| entry.glyph == Glyph::ArtifactTick);
            lines.push(RoleLine::role(
                if artifact {
                    "Enter open · x remove · c copy · f feedback · Esc"
                } else {
                    "Enter open · x remove · Esc"
                },
                Role::MutedText,
            ));
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
