#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum FooterItemId {
    Agents,
}

impl FooterItemId {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Agents => "Subagents",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct FooterFrameItem {
    pub(super) id: FooterItemId,
    pub(super) selected: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct FooterFrame {
    pub(super) items: Vec<FooterFrameItem>,
    /// The list that owns the keys; `None` while the input does.
    pub(super) selected: Option<FooterItemId>,
}

#[derive(Default)]
pub(super) struct FooterState {
    items: Vec<FooterItemId>,
    selected: Option<FooterItemId>,
}

impl FooterState {
    pub(super) fn update(&mut self, items: impl IntoIterator<Item = FooterItemId>) {
        let mut visible = Vec::new();
        for item in items {
            if !visible.contains(&item) {
                visible.push(item);
            }
        }
        if self.selected.is_some_and(|item| !visible.contains(&item)) {
            self.selected = None;
        }
        self.items = visible;
    }

    pub(super) fn selected(&self) -> Option<FooterItemId> {
        self.selected
    }

    /// Gives `item`'s list the keys, when it is shown.
    pub(super) fn select(&mut self, item: FooterItemId) {
        if self.items.contains(&item) {
            self.selected = Some(item);
        }
    }

    /// Returns the keys to the input.
    pub(super) fn clear(&mut self) {
        self.selected = None;
    }

    pub(super) fn frame(&self) -> FooterFrame {
        FooterFrame {
            items: self
                .items
                .iter()
                .copied()
                .map(|id| FooterFrameItem {
                    id,
                    selected: self.selected == Some(id),
                })
                .collect(),
            selected: self.selected,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn the_only_footer_entry_is_subagents_and_hidden_entries_do_not_take_focus() {
        let mut footer = FooterState::default();
        footer.select(FooterItemId::Agents);
        assert_eq!(footer.selected(), None);
        footer.update([FooterItemId::Agents, FooterItemId::Agents]);
        footer.select(FooterItemId::Agents);
        assert_eq!(footer.selected(), Some(FooterItemId::Agents));
        assert_eq!(footer.frame().items.len(), 1);
        assert_eq!(FooterItemId::Agents.label(), "Subagents");
        footer.update([]);
        assert_eq!(footer.selected(), None);
        footer.update([FooterItemId::Agents]);
        footer.select(FooterItemId::Agents);
        footer.clear();
        assert_eq!(footer.selected(), None);
    }
}
