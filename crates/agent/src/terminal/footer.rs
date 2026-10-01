#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum FooterItemId {
    Tasks,
    Todo,
    Agents,
}

impl FooterItemId {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Tasks => "Shell",
            Self::Todo => "Todo",
            Self::Agents => "Agents",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FooterAction {
    Down,
    Up,
    Previous,
    Next,
    Clear,
    Open,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FooterEffect {
    None,
    ReturnToEditor,
    Open(FooterItemId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct FooterFrameItem {
    pub(super) id: FooterItemId,
    pub(super) selected: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct FooterFrame {
    pub(super) items: Vec<FooterFrameItem>,
    pub(super) selected: Option<FooterItemId>,
    /// Whether keys move within the open list rather than across the summary row.
    pub(super) list_focused: bool,
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

    pub(super) fn select(&mut self, item: FooterItemId) {
        if self.items.contains(&item) {
            self.selected = Some(item);
        }
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
            list_focused: false,
        }
    }

    pub(super) fn apply(&mut self, action: FooterAction) -> FooterEffect {
        match action {
            FooterAction::Down => self.move_next(true),
            FooterAction::Up => self.move_up(),
            FooterAction::Previous => self.move_previous(),
            FooterAction::Next => self.move_next(false),
            FooterAction::Clear => {
                self.selected = None;
                FooterEffect::ReturnToEditor
            }
            FooterAction::Open => self.open(),
        }
    }

    fn move_next(&mut self, select_first: bool) -> FooterEffect {
        let Some(selected) = self.selected else {
            if select_first {
                self.selected = self.items.first().copied();
            }
            return FooterEffect::None;
        };
        let Some(index) = self.items.iter().position(|item| *item == selected) else {
            self.selected = None;
            return FooterEffect::None;
        };
        if let Some(item) = self.items.get(index + 1) {
            self.selected = Some(*item);
        }
        FooterEffect::None
    }

    fn move_up(&mut self) -> FooterEffect {
        let Some(selected) = self.selected else {
            return FooterEffect::ReturnToEditor;
        };
        let Some(index) = self.items.iter().position(|item| *item == selected) else {
            self.selected = None;
            return FooterEffect::ReturnToEditor;
        };
        if index == 0 {
            self.selected = None;
            FooterEffect::ReturnToEditor
        } else {
            self.selected = Some(self.items[index - 1]);
            FooterEffect::None
        }
    }

    fn move_previous(&mut self) -> FooterEffect {
        let Some(selected) = self.selected else {
            return FooterEffect::None;
        };
        let Some(index) = self.items.iter().position(|item| *item == selected) else {
            self.selected = None;
            return FooterEffect::None;
        };
        if let Some(index) = index.checked_sub(1) {
            self.selected = Some(self.items[index]);
        }
        FooterEffect::None
    }

    fn open(&mut self) -> FooterEffect {
        let Some(selected) = self.selected else {
            return FooterEffect::None;
        };
        if !self.items.contains(&selected) {
            self.selected = None;
            return FooterEffect::None;
        }
        self.selected = None;
        FooterEffect::Open(selected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn down_selects_only_a_visible_first_item() {
        let mut footer = FooterState::default();

        assert_eq!(footer.apply(FooterAction::Down), FooterEffect::None);
        assert_eq!(footer.selected(), None);

        footer.update([FooterItemId::Tasks, FooterItemId::Todo]);
        assert_eq!(footer.apply(FooterAction::Down), FooterEffect::None);
        assert_eq!(footer.selected(), Some(FooterItemId::Tasks));
        assert_eq!(
            footer.frame(),
            FooterFrame {
                items: vec![
                    FooterFrameItem {
                        id: FooterItemId::Tasks,
                        selected: true,
                    },
                    FooterFrameItem {
                        id: FooterItemId::Todo,
                        selected: false,
                    },
                ],
                selected: Some(FooterItemId::Tasks),
                list_focused: false,
            }
        );
    }

    #[test]
    fn update_preserves_identity_but_does_not_revive_a_removed_selection() {
        let mut footer = FooterState::default();
        footer.update([FooterItemId::Tasks, FooterItemId::Todo]);
        footer.apply(FooterAction::Down);
        footer.apply(FooterAction::Next);

        footer.update([FooterItemId::Todo, FooterItemId::Tasks]);
        assert_eq!(footer.selected(), Some(FooterItemId::Todo));

        footer.update([FooterItemId::Tasks]);
        assert_eq!(footer.selected(), None);

        footer.update([FooterItemId::Todo, FooterItemId::Tasks]);
        assert_eq!(footer.selected(), None);
        assert!(footer.frame().items.iter().all(|item| !item.selected));
    }

    #[test]
    fn navigation_clamps_and_up_returns_to_the_editor_at_the_top() {
        let mut footer = FooterState::default();
        footer.update([FooterItemId::Tasks, FooterItemId::Todo]);
        footer.apply(FooterAction::Down);

        footer.apply(FooterAction::Next);
        footer.apply(FooterAction::Next);
        assert_eq!(footer.selected(), Some(FooterItemId::Todo));

        footer.apply(FooterAction::Previous);
        footer.apply(FooterAction::Previous);
        assert_eq!(footer.selected(), Some(FooterItemId::Tasks));

        assert_eq!(footer.apply(FooterAction::Up), FooterEffect::ReturnToEditor);
        assert_eq!(footer.selected(), None);
    }

    #[test]
    fn down_advances_after_footer_is_focused() {
        let mut footer = FooterState::default();
        footer.update([FooterItemId::Tasks, FooterItemId::Todo]);

        footer.apply(FooterAction::Down);
        footer.apply(FooterAction::Down);

        assert_eq!(footer.selected(), Some(FooterItemId::Todo));
    }

    #[test]
    fn open_returns_the_typed_identity_and_clears_focus() {
        let mut footer = FooterState::default();
        footer.update([FooterItemId::Tasks, FooterItemId::Todo]);
        footer.apply(FooterAction::Down);

        assert_eq!(
            footer.apply(FooterAction::Open),
            FooterEffect::Open(FooterItemId::Tasks)
        );
        assert_eq!(footer.selected(), None);
        assert_eq!(footer.apply(FooterAction::Open), FooterEffect::None);
    }

    #[test]
    fn clear_returns_to_the_editor_and_duplicate_items_are_removed() {
        let mut footer = FooterState::default();
        footer.update([FooterItemId::Tasks, FooterItemId::Tasks, FooterItemId::Todo]);
        footer.apply(FooterAction::Down);

        assert_eq!(
            footer.apply(FooterAction::Clear),
            FooterEffect::ReturnToEditor
        );
        assert_eq!(footer.selected(), None);
        assert_eq!(footer.frame().items.len(), 2);
        assert_eq!(FooterItemId::Tasks.label(), "Shell");
        assert_eq!(FooterItemId::Todo.label(), "Todo");
    }
}
