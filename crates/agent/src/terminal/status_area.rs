use unicode_width::UnicodeWidthStr;
use uuid::Uuid;

use super::{
    footer::{FooterFrame, FooterItemId},
    running_shimmer::RunningShimmer,
    style::{Role, RoleLine},
};

const MAX_PANEL_ITEMS: usize = 4;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct StatusItem {
    pub(super) id: String,
    pub(super) task_id: Option<Uuid>,
    pub(super) can_stop: bool,
    pub(super) title: String,
    pub(super) state: &'static str,
    pub(super) role: Role,
}

#[derive(Default)]
pub(super) struct StatusArea {
    mode: String,
    model: String,
    context: Option<(u64, usize)>,
    tasks: Vec<StatusItem>,
    todos: Vec<StatusItem>,
    visible: Option<FooterItemId>,
    offset: usize,
    selected_task: Option<Uuid>,
    running_shimmer: RunningShimmer,
    prefers_reduced_motion: bool,
}

impl StatusArea {
    pub(super) fn is_configured(&self) -> bool {
        !self.mode.is_empty()
    }

    pub(super) fn set_reduced_motion(&mut self, prefers_reduced_motion: bool) {
        self.prefers_reduced_motion = prefers_reduced_motion;
    }

    pub(super) fn has_visible_running_task(&self) -> bool {
        self.visible == Some(FooterItemId::Tasks)
            && self.tasks.iter().any(|task| task.state == "running")
    }

    pub(super) fn update(
        &mut self,
        mode: String,
        model: String,
        context: Option<(u64, usize)>,
        tasks: Vec<StatusItem>,
        todos: Vec<StatusItem>,
    ) {
        self.mode = mode;
        self.model = model;
        self.context = context.filter(|(_, limit)| *limit > 0);
        self.tasks = tasks;
        self.todos = todos;
        if !self
            .selected_task
            .is_some_and(|selected| self.tasks.iter().any(|task| task.task_id == Some(selected)))
        {
            self.selected_task = self.tasks.first().and_then(|task| task.task_id);
            self.offset = 0;
        }
        if self
            .visible
            .is_some_and(|panel| self.items(panel).is_empty())
        {
            self.visible = None;
            self.offset = 0;
        }
        self.clamp_offset();
    }

    pub(super) fn available_panels(&self) -> impl Iterator<Item = FooterItemId> + '_ {
        [FooterItemId::Tasks, FooterItemId::Todo]
            .into_iter()
            .filter(|panel| !self.items(*panel).is_empty())
    }

    pub(super) fn select(&mut self, panel: FooterItemId) {
        if !self.items(panel).is_empty() && self.visible != Some(panel) {
            self.visible = Some(panel);
            self.offset = 0;
        }
    }

    pub(super) fn collapse(&mut self) {
        self.visible = None;
    }

    pub(super) fn toggle(&mut self, panel: FooterItemId) {
        if self.visible == Some(panel) {
            self.collapse();
        } else {
            self.select(panel);
        }
    }

    pub(super) fn is_open(&self) -> bool {
        self.visible.is_some()
    }

    pub(super) fn scroll(&mut self, amount: isize, capacity: usize) {
        let count = self.visible.map_or(0, |panel| self.items(panel).len());
        let max_offset = count.saturating_sub(capacity);
        self.offset = self.offset.saturating_add_signed(amount).min(max_offset);
    }

    pub(super) fn selected_task(&self) -> Option<Uuid> {
        (self.visible == Some(FooterItemId::Tasks))
            .then_some(self.selected_task)
            .flatten()
    }

    pub(super) fn stoppable_task(&self) -> Option<Uuid> {
        let selected = self.selected_task()?;
        self.tasks
            .iter()
            .find(|task| task.task_id == Some(selected) && task.can_stop)
            .map(|_| selected)
    }

    pub(super) fn move_task_selection(&mut self, amount: isize, capacity: usize) {
        if self.visible != Some(FooterItemId::Tasks) || self.tasks.is_empty() {
            return;
        }
        let current = self
            .tasks
            .iter()
            .position(|task| task.task_id == self.selected_task)
            .unwrap_or(0);
        let next = current
            .saturating_add_signed(amount)
            .min(self.tasks.len() - 1);
        self.selected_task = self.tasks[next].task_id;
        if next < self.offset {
            self.offset = next;
        } else if capacity > 0 && next >= self.offset + capacity {
            self.offset = next + 1 - capacity;
        }
    }

    pub(super) fn row_count(&self, height: usize) -> usize {
        if self.tasks.is_empty() && self.todos.is_empty() {
            return 0;
        }
        if height < 4 {
            return 0;
        }
        let Some(panel) = self.visible else {
            return 1;
        };
        1 + self
            .items(panel)
            .len()
            .min(MAX_PANEL_ITEMS)
            .min(height.saturating_sub(6))
    }

    pub(super) fn status_line(&self, width: usize, notice: &str) -> RoleLine {
        let mut line = RoleLine::default();
        let mode = format!("⏵⏵ {}", self.mode);
        let mode_role = match self.mode.as_str() {
            "Plan" => Role::PlanMode,
            "Ask" => Role::AutoAccept,
            "Full Access" => Role::Error,
            _ => Role::Warning,
        };
        line.push(mode.clone(), Some(mode_role));
        let context = self.context.map_or_else(
            || "Context —".to_owned(),
            |(used, limit)| {
                format!(
                    "Context {}% used",
                    used.saturating_mul(100).saturating_add(limit as u64 / 2) / limit as u64
                )
            },
        );
        let notice_width = if notice.is_empty() {
            0
        } else {
            3 + UnicodeWidthStr::width(notice)
        };
        let mut used_width = UnicodeWidthStr::width(mode.as_str());
        for (text, role) in [(&self.model, Role::Text), (&context, Role::Success)] {
            let segment_width = 3 + UnicodeWidthStr::width(text.as_str());
            if used_width + segment_width + notice_width > width {
                break;
            }
            line.push(" · ", Some(Role::MutedText));
            line.push(text.clone(), Some(role));
            used_width += segment_width;
        }
        if !notice.is_empty() {
            line.push(" · ", Some(Role::MutedText));
            line.push(notice.to_owned(), Some(Role::Warning));
        }
        line.truncate(width);
        line.filled(Role::StatusBar)
    }

    pub(super) fn panel_lines(
        &self,
        footer: &FooterFrame,
        width: usize,
        height: usize,
    ) -> Vec<RoleLine> {
        let row_count = self.row_count(height);
        if row_count == 0 {
            return Vec::new();
        }
        let mut header = RoleLine::default();
        for item in &footer.items {
            if !header.text().is_empty() {
                header.push("  ", None);
            }
            let label = self.header_label(item.id);
            let role = if self.visible == Some(item.id) || item.selected {
                Role::Suggestion
            } else {
                Role::MutedText
            };
            header.push(label, Some(role));
        }
        let summary = if let Some(panel) = self.visible {
            let count = self.items(panel).len();
            if panel == FooterItemId::Tasks && footer.selected == Some(FooterItemId::Tasks) {
                let action = if self.stoppable_task().is_some() {
                    " · x stop"
                } else {
                    ""
                };
                format!("{count} items · Enter details{action}")
            } else if count > MAX_PANEL_ITEMS {
                format!("{count} items · scroll")
            } else {
                format!("{count} items")
            }
        } else {
            "Enter/Space expand".to_owned()
        };
        let space = width
            .saturating_sub(UnicodeWidthStr::width(header.text().as_str()))
            .saturating_sub(UnicodeWidthStr::width(summary.as_str()));
        if space > 1 {
            header.push(" ".repeat(space), None);
            header.push(summary, Some(Role::MutedText));
        }
        header.truncate(width);
        let mut lines = vec![header.filled(Role::StatusPanel)];
        let Some(panel) = self.visible else {
            return lines;
        };
        let items = self.items(panel);
        let capacity = row_count - 1;
        let offset = if panel == FooterItemId::Tasks
            && footer.selected == Some(FooterItemId::Tasks)
            && let Some(selected) = self.selected_task
            && let Some(index) = items.iter().position(|item| item.task_id == Some(selected))
        {
            if index < self.offset {
                index
            } else if index >= self.offset + capacity {
                index + 1 - capacity
            } else {
                self.offset
            }
        } else {
            self.offset
        };
        for item in items.iter().skip(offset).take(capacity) {
            let mut row = RoleLine::default();
            let selected = panel == FooterItemId::Tasks
                && footer.selected == Some(FooterItemId::Tasks)
                && item.task_id.is_some()
                && item.task_id == self.selected_task;
            row.push(if selected { "› " } else { "  " }, Some(Role::Suggestion));
            row.push(format!("{}  ", item.id), Some(Role::MutedText));
            let right = format!("  {}", item.state);
            let title_width = width
                .saturating_sub(UnicodeWidthStr::width(item.id.as_str()) + 4)
                .saturating_sub(UnicodeWidthStr::width(right.as_str()));
            let mut title = RoleLine::plain(item.title.clone());
            title.truncate(title_width);
            row.push(title.text(), Some(Role::Text));
            let padding = width
                .saturating_sub(UnicodeWidthStr::width(row.text().as_str()))
                .saturating_sub(UnicodeWidthStr::width(right.as_str()));
            row.push(" ".repeat(padding), None);
            if item.state == "running" {
                self.running_shimmer
                    .append(&mut row, &right, self.prefers_reduced_motion);
            } else {
                row.push(right, Some(item.role));
            }
            row.truncate(width);
            lines.push(row.filled(Role::StatusPanel));
        }
        lines
    }

    fn items(&self, panel: FooterItemId) -> &[StatusItem] {
        match panel {
            FooterItemId::Tasks => &self.tasks,
            FooterItemId::Todo => &self.todos,
        }
    }

    fn header_label(&self, panel: FooterItemId) -> String {
        match panel {
            FooterItemId::Tasks => format!("Tasks {}", self.tasks.len()),
            FooterItemId::Todo => format!("Todo {}", self.todos.len()),
        }
    }

    pub(super) fn panel_at_column(
        &self,
        footer: &FooterFrame,
        column: usize,
    ) -> Option<FooterItemId> {
        let mut start = 0;
        for item in &footer.items {
            let end = start + UnicodeWidthStr::width(self.header_label(item.id).as_str());
            if (start..end).contains(&column) {
                return Some(item.id);
            }
            start = end + 2;
        }
        None
    }

    fn clamp_offset(&mut self) {
        let count = self.visible.map_or(0, |panel| self.items(panel).len());
        self.offset = self.offset.min(count.saturating_sub(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, title: &str) -> StatusItem {
        StatusItem {
            id: id.into(),
            task_id: None,
            can_stop: false,
            title: title.into(),
            state: "running",
            role: Role::Success,
        }
    }

    fn footer(items: &[FooterItemId], selected: Option<FooterItemId>) -> FooterFrame {
        FooterFrame {
            items: items
                .iter()
                .copied()
                .map(|id| super::super::footer::FooterFrameItem {
                    id,
                    selected: selected == Some(id),
                })
                .collect(),
            selected,
        }
    }

    #[test]
    fn status_keeps_mode_and_notice_when_narrow_and_never_invents_usage() {
        let mut area = StatusArea::default();
        area.update("Auto".into(), "gpt-5.6".into(), None, vec![], vec![]);
        assert_eq!(
            area.status_line(80, "").text(),
            "⏵⏵ Auto · gpt-5.6 · Context —"
        );
        let narrow = area.status_line(24, "Not saved").text();
        assert!(narrow.starts_with("⏵⏵ Auto · Not saved"));
        assert!(!narrow.contains("gpt-5.6"));
        area.update(
            "Auto".into(),
            "gpt-5.6".into(),
            Some((0, 200_000)),
            vec![],
            vec![],
        );
        assert_eq!(
            area.status_line(80, "").text(),
            "⏵⏵ Auto · gpt-5.6 · Context 0% used"
        );
        area.update(
            "Auto".into(),
            "gpt-5.6".into(),
            Some((42_000, 200_000)),
            vec![],
            vec![],
        );
        assert_eq!(
            area.status_line(80, "").text(),
            "⏵⏵ Auto · gpt-5.6 · Context 21% used"
        );
    }

    #[test]
    fn permission_mode_status_uses_distinct_roles_for_ask_and_full_access() {
        let mut area = StatusArea::default();
        area.update("Ask".into(), "model".into(), None, vec![], vec![]);
        assert_eq!(
            area.status_line(80, "").spans[0].role,
            Some(Role::AutoAccept)
        );

        area.update("Full Access".into(), "model".into(), None, vec![], vec![]);
        assert_eq!(area.status_line(80, "").spans[0].role, Some(Role::Error));
    }

    #[test]
    fn only_one_available_panel_is_shown_and_scrolls_within_four_rows() {
        let mut area = StatusArea::default();
        area.update(
            "Auto".into(),
            "model".into(),
            None,
            (1..=6)
                .map(|index| item(&format!("sh-{index}"), &format!("task {index}")))
                .collect(),
            vec![item("1.", "review plan")],
        );
        let footer = footer(&[FooterItemId::Tasks, FooterItemId::Todo], None);
        let collapsed = area.panel_lines(&footer, 80, 24);
        assert_eq!(collapsed.len(), 1);
        assert!(collapsed[0].text().contains("Tasks 6  Todo 1"));
        assert!(collapsed[0].text().contains("Enter/Space expand"));
        area.select(FooterItemId::Tasks);
        let first = area.panel_lines(&footer, 80, 24);
        assert_eq!(first.len(), 5);
        assert!(first[0].text().contains("Tasks 6  Todo 1"));
        assert!(first[0].text().contains("6 items · scroll"));
        assert!(first[1].text().contains("task 1"));
        assert!(first[4].text().contains("task 4"));
        area.scroll(2, 4);
        assert!(
            area.panel_lines(&footer, 80, 24)[1]
                .text()
                .contains("task 3")
        );
        area.select(FooterItemId::Todo);
        let todo = area.panel_lines(&footer, 80, 24);
        assert_eq!(todo.len(), 2);
        assert!(todo[1].text().contains("review plan"));
        area.collapse();
        area.update(
            "Auto".into(),
            "model".into(),
            None,
            vec![item("sh-1", "task 1")],
            vec![item("1.", "review plan")],
        );
        assert_eq!(area.panel_lines(&footer, 80, 24).len(), 1);
        area.select(FooterItemId::Tasks);
        assert!(
            area.panel_lines(&footer, 80, 24)[1]
                .text()
                .contains("task 1")
        );
        area.update("Auto".into(), "model".into(), None, vec![], vec![]);
        assert!(area.panel_lines(&footer, 80, 24).is_empty());
    }

    #[test]
    fn task_selection_tracks_identity_and_keeps_the_selected_row_visible() {
        let mut area = StatusArea::default();
        let tasks = (1..=6)
            .map(|index| StatusItem {
                id: format!("sh-{index}"),
                task_id: Some(Uuid::from_u128(index)),
                can_stop: true,
                title: format!("task {index}"),
                state: "running",
                role: Role::Success,
            })
            .collect::<Vec<_>>();
        area.update("Auto".into(), "model".into(), None, tasks.clone(), vec![]);
        area.select(FooterItemId::Tasks);
        let footer = footer(&[FooterItemId::Tasks], Some(FooterItemId::Tasks));
        assert_eq!(area.selected_task(), Some(Uuid::from_u128(1)));

        area.move_task_selection(4, 4);
        assert_eq!(area.selected_task(), Some(Uuid::from_u128(5)));
        assert!(
            area.panel_lines(&footer, 80, 24)[4]
                .text()
                .contains("› sh-5")
        );
        assert!(
            area.panel_lines(&footer, 80, 8)[2]
                .text()
                .contains("› sh-5")
        );

        area.update(
            "Auto".into(),
            "model".into(),
            None,
            tasks[1..].to_vec(),
            vec![],
        );
        assert_eq!(area.selected_task(), Some(Uuid::from_u128(5)));
        area.update(
            "Auto".into(),
            "model".into(),
            None,
            tasks[1..4].to_vec(),
            vec![],
        );
        assert_eq!(area.selected_task(), Some(Uuid::from_u128(2)));
    }

    #[test]
    fn running_task_uses_the_shared_shimmer_and_reduced_motion_is_static() {
        let mut area = StatusArea::default();
        area.update(
            "Auto".into(),
            "model".into(),
            None,
            vec![item("sh-1", "watch prices")],
            vec![],
        );
        area.select(FooterItemId::Tasks);
        area.set_reduced_motion(true);
        assert!(area.has_visible_running_task());
        let row = &area.panel_lines(&footer(&[FooterItemId::Tasks], None), 80, 24)[1];
        assert!(row.spans.iter().any(|span| {
            span.text.contains("running") && span.role == Some(Role::RunningShade0)
        }));
    }
}
