use std::collections::{HashMap, HashSet};

use unicode_width::UnicodeWidthStr;
use uuid::Uuid;

use super::{
    console::{AgentTreeRow, MIN_REQUEST_AGENT_TITLE_COLUMNS, RowExtras, truncate_width},
    display::DisplayProfile,
    footer::{FooterFrame, FooterItemId},
    running_shimmer::RunningShimmer,
    style::{Role, RoleLine},
};

const MAX_PANEL_ITEMS: usize = 8;
const MAX_BACKGROUND_TITLE_COLUMNS: usize = 24;
const BACKGROUND_AGENT_NAME_PREFIX: &str = "Agent ";
const STATUS_SEGMENT_SEPARATOR: &str = " · ";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct BackgroundStatusNotice {
    pub(super) title: Option<String>,
    pub(super) ordinal: Option<usize>,
    pub(super) message: String,
    pub(super) message_role: Role,
    pub(super) compact_message: Option<String>,
    pub(super) necessary: bool,
}

impl BackgroundStatusNotice {
    pub(super) fn line(&self, width: usize) -> RoleLine {
        let message = if UnicodeWidthStr::width(self.message.as_str()) > width {
            self.compact_message.as_deref().unwrap_or(&self.message)
        } else {
            &self.message
        };
        let message_width = UnicodeWidthStr::width(message);
        let number = self
            .ordinal
            .map(|number| format!(" ({number})"))
            .unwrap_or_default();
        let Some(title) = &self.title else {
            let mut full = RoleLine::role("Background Agent", Role::Text);
            full.push(" · label unavailable · ", Some(Role::MutedText));
            full.push(message, Some(self.message_role));
            return if UnicodeWidthStr::width(full.text().as_str()) <= width {
                full
            } else {
                RoleLine::role(message, self.message_role)
            };
        };
        let title_columns = width
            .saturating_sub(
                message_width
                    + UnicodeWidthStr::width(BACKGROUND_AGENT_NAME_PREFIX)
                    + UnicodeWidthStr::width(STATUS_SEGMENT_SEPARATOR)
                    + UnicodeWidthStr::width(number.as_str()),
            )
            .min(MAX_BACKGROUND_TITLE_COLUMNS);
        if title_columns == 0 {
            return RoleLine::role(message, self.message_role);
        }
        let title = if UnicodeWidthStr::width(title.as_str()) > title_columns {
            format!(
                "{}…",
                truncate_width(title, title_columns.saturating_sub(1))
            )
        } else {
            title.clone()
        };
        let name = format!("{BACKGROUND_AGENT_NAME_PREFIX}{title}{number}");
        let mut line = RoleLine::default();
        if self.necessary {
            line.push(message, Some(self.message_role));
            line.push(STATUS_SEGMENT_SEPARATOR, Some(Role::MutedText));
            line.push(name, Some(Role::Text));
        } else {
            line.push(name, Some(Role::Text));
            line.push(STATUS_SEGMENT_SEPARATOR, Some(Role::MutedText));
            line.push(message, Some(self.message_role));
        }
        line
    }
}

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
    agents: Vec<AgentTreeRow>,
    /// Reading numbers shown after repeated family titles.
    agent_ordinals: HashMap<Uuid, usize>,
    /// The family member this View shows.
    visible_agent: Option<Uuid>,
    selected_agent: Option<Uuid>,
    collapsed_agents: HashSet<Uuid>,
    panel_offsets: HashMap<FooterItemId, usize>,
    visible: Option<FooterItemId>,
    manually_selected: bool,
    pinned: bool,
    offset: usize,
    selected_task: Option<Uuid>,
    selected_todo: usize,
    running_shimmer: RunningShimmer,
    prefers_reduced_motion: bool,
}

impl StatusArea {
    pub(super) fn notice_columns(&self, width: usize) -> usize {
        width.saturating_sub(UnicodeWidthStr::width(
            format!("⏵⏵ {} · ", self.mode).as_str(),
        ))
    }

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
        let selected_index = self
            .tasks
            .iter()
            .position(|task| task.task_id == self.selected_task)
            .unwrap_or(0);
        self.mode = mode;
        self.model = model;
        self.context = context.filter(|(_, limit)| *limit > 0);
        self.tasks = tasks;
        self.todos = todos;
        self.selected_todo = self.selected_todo.min(self.todos.len().saturating_sub(1));
        if !self
            .selected_task
            .is_some_and(|selected| self.tasks.iter().any(|task| task.task_id == Some(selected)))
        {
            self.selected_task = self
                .tasks
                .get(selected_index.min(self.tasks.len().saturating_sub(1)))
                .and_then(|task| task.task_id);
        }
        self.reconcile_panel();
        self.clamp_offset();
    }

    pub(super) fn available_panels(&self) -> impl Iterator<Item = FooterItemId> + '_ {
        [FooterItemId::Todo, FooterItemId::Agents]
            .into_iter()
            .filter(|panel| {
                self.item_count(*panel) > 0 || self.pinned && self.visible == Some(*panel)
            })
    }

    /// The reading number this family panel shows for `agent_id` when another family row reads
    /// alike, so a request header and the row it names always agree.
    pub(super) fn ordinal(&self, agent_id: Uuid) -> Option<usize> {
        self.agent_ordinals.get(&agent_id).copied()
    }

    pub(super) fn agent_title(&self, agent_id: Uuid) -> Option<&str> {
        self.agents
            .iter()
            .find(|row| row.agent_id == agent_id)
            .map(|row| row.title.as_str())
    }

    /// Replaces the family rows; `ordinals` are the View's reading numbers for repeated titles.
    pub(super) fn update_agents(
        &mut self,
        rows: Vec<AgentTreeRow>,
        visible: Option<Uuid>,
        ordinals: HashMap<Uuid, usize>,
    ) {
        self.visible_agent = visible;
        self.agent_ordinals = ordinals;
        let selected_index = self
            .agents
            .iter()
            .position(|row| Some(row.agent_id) == self.selected_agent)
            .unwrap_or(0);
        self.agents = rows;
        if self
            .selected_agent
            .is_none_or(|id| !self.agents.iter().any(|row| row.agent_id == id))
        {
            self.selected_agent = self
                .agents
                .get(selected_index.min(self.agents.len().saturating_sub(1)))
                .map(|row| row.agent_id);
        }
        self.reconcile_panel();
        self.clamp_offset();
    }

    pub(super) fn selected_agent(&self) -> Option<Uuid> {
        (self.visible == Some(FooterItemId::Agents))
            .then_some(self.selected_agent)
            .flatten()
    }

    fn visible_agents(&self) -> Vec<&AgentTreeRow> {
        self.agents
            .iter()
            .filter(|row| {
                let mut parent = row.parent_id;
                while let Some(id) = parent {
                    if self.collapsed_agents.contains(&id) {
                        return false;
                    }
                    parent = self
                        .agents
                        .iter()
                        .find(|row| row.agent_id == id)
                        .and_then(|row| row.parent_id);
                }
                true
            })
            .collect()
    }

    pub(super) fn toggle_selected_agent(&mut self) {
        if let Some(id) = self.selected_agent
            && !self.collapsed_agents.remove(&id)
        {
            self.collapsed_agents.insert(id);
        }
        self.clamp_offset();
    }

    pub(super) fn selection_at_start(&self, panel: FooterItemId) -> bool {
        match panel {
            FooterItemId::Tasks => {
                self.selected_task == self.tasks.first().and_then(|row| row.task_id)
            }
            FooterItemId::Agents => {
                self.selected_agent == self.visible_agents().first().map(|row| row.agent_id)
            }
            FooterItemId::Todo => self.selected_todo == 0,
        }
    }

    pub(super) fn move_agent_selection(&mut self, amount: isize, capacity: usize) {
        let agents = self.visible_agents();
        if self.visible != Some(FooterItemId::Agents) || agents.is_empty() {
            return;
        }
        let current = agents
            .iter()
            .position(|row| Some(row.agent_id) == self.selected_agent)
            .unwrap_or(0);
        let next = current.saturating_add_signed(amount).min(agents.len() - 1);
        self.selected_agent = Some(agents[next].agent_id);
        if next < self.offset {
            self.offset = next;
        } else if next >= self.offset + capacity.max(1) {
            self.offset = next + 1 - capacity.max(1);
        }
    }

    pub(super) fn select(&mut self, panel: FooterItemId) {
        if self.item_count(panel) == 0 {
            return;
        }
        self.manually_selected = true;
        self.show_panel(Some(panel));
    }

    fn reconcile_panel(&mut self) {
        if self.pinned {
            return;
        }
        if self.manually_selected && self.visible.is_some_and(|panel| self.item_count(panel) > 0) {
            return;
        }
        self.manually_selected = false;
        let panel = self.available_panels().next();
        self.show_panel(panel);
    }

    fn show_panel(&mut self, panel: Option<FooterItemId>) {
        if self.visible != panel {
            if let Some(previous) = self.visible {
                self.panel_offsets.insert(previous, self.offset);
            }
            self.visible = panel;
            self.offset = panel
                .and_then(|panel| self.panel_offsets.get(&panel).copied())
                .unwrap_or(0);
            self.clamp_offset();
        }
    }

    pub(super) fn pin_panel(&mut self) {
        self.pinned = self.visible.is_some();
        self.manually_selected = true;
    }

    pub(super) fn release_panel(&mut self) {
        self.pinned = false;
        self.reconcile_panel();
    }

    pub(super) fn is_open(&self) -> bool {
        self.visible.is_some()
    }

    pub(super) fn selected_panel(&self) -> Option<FooterItemId> {
        self.visible
    }

    pub(super) fn scroll(&mut self, amount: isize, capacity: usize) {
        let count = self.visible.map_or(0, |panel| self.item_count(panel));
        let max_offset = count.saturating_sub(capacity);
        self.offset = self.offset.saturating_add_signed(amount).min(max_offset);
    }

    pub(super) fn selected_task(&self) -> Option<Uuid> {
        (self.visible == Some(FooterItemId::Tasks))
            .then_some(self.selected_task)
            .flatten()
    }

    pub(super) fn task_ids(&self) -> impl Iterator<Item = Uuid> + '_ {
        self.tasks.iter().filter_map(|task| task.task_id)
    }

    pub(super) fn single_task(&self) -> Option<Uuid> {
        (self.tasks.len() == 1)
            .then(|| self.tasks[0].task_id)
            .flatten()
    }

    pub(super) fn selected_todo(&self) -> Option<(usize, &StatusItem)> {
        (self.visible == Some(FooterItemId::Todo))
            .then(|| {
                self.todos
                    .get(self.selected_todo)
                    .map(|item| (self.selected_todo, item))
            })
            .flatten()
    }

    pub(super) fn move_todo_selection(&mut self, amount: isize, capacity: usize) {
        if self.visible != Some(FooterItemId::Todo) || self.todos.is_empty() {
            return;
        }
        self.selected_todo = self
            .selected_todo
            .saturating_add_signed(amount)
            .min(self.todos.len() - 1);
        if self.selected_todo < self.offset {
            self.offset = self.selected_todo;
        } else if self.selected_todo >= self.offset + capacity.max(1) {
            self.offset = self.selected_todo + 1 - capacity.max(1);
        }
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
        if self.todos.is_empty()
            && self.agents.is_empty()
            && self.visible != Some(FooterItemId::Tasks)
        {
            return 0;
        }
        if height < 4 {
            return 0;
        }
        let Some(panel) = self.visible else {
            return 1;
        };
        1 + self
            .item_count(panel)
            .min((height / 6).clamp(3, MAX_PANEL_ITEMS))
            .min(height.saturating_sub(6))
    }

    #[cfg(test)]
    pub(super) fn status_line(&self, width: usize, notice: &str) -> RoleLine {
        self.status_line_with_background(width, notice, None)
    }

    pub(super) fn status_line_with_background(
        &self,
        width: usize,
        notice: &str,
        background: Option<&BackgroundStatusNotice>,
    ) -> RoleLine {
        let mut line = RoleLine::default();
        let mode = format!("⏵⏵ {}", self.mode);
        let mode_role = match self.mode.as_str() {
            "Plan" => Role::PlanMode,
            "Ask" => Role::AutoAccept,
            "Full Access" => Role::Error,
            "Auto" => Role::Warning,
            _ => Role::Text,
        };
        line.push(mode.clone(), Some(mode_role));
        let (context, context_role) = self.context.map_or_else(
            || ("Context —".to_owned(), Role::MutedText),
            |(used, limit)| {
                let percent =
                    used.saturating_mul(100).saturating_add(limit as u64 / 2) / limit as u64;
                (format!("Context {percent}% used"), Role::Success)
            },
        );
        let necessary_background = background.filter(|notice| notice.necessary);
        let ordinary_background = background.filter(|notice| !notice.necessary);
        let mut notice_line = RoleLine::role(notice, Role::Warning);
        if let Some(background) = necessary_background {
            let available = width.saturating_sub(UnicodeWidthStr::width(mode.as_str()) + 3);
            if !notice.is_empty() {
                notice_line.push(STATUS_SEGMENT_SEPARATOR, Some(Role::MutedText));
            }
            notice_line.spans.extend(background.line(available).spans);
        }
        let notice = notice_line.text();
        if !notice.is_empty()
            && UnicodeWidthStr::width(notice.as_str()) > self.notice_columns(width)
        {
            let mut urgent = notice_line;
            urgent.truncate(width);
            return urgent.filled(Role::StatusBar);
        }
        let notice_width = if notice.is_empty() {
            0
        } else {
            3 + UnicodeWidthStr::width(notice.as_str())
        };
        let outcome_width = ordinary_background.map_or(0, |background| {
            3 + UnicodeWidthStr::width(background.message.as_str())
        });
        let mut used_width = UnicodeWidthStr::width(mode.as_str());
        for (text, role) in [(&self.model, Role::Text), (&context, context_role)] {
            let segment_width = 3 + UnicodeWidthStr::width(text.as_str());
            if used_width + segment_width + notice_width + outcome_width > width {
                break;
            }
            line.push(" · ", Some(Role::MutedText));
            line.push(text.clone(), Some(role));
            used_width += segment_width;
        }
        if !notice.is_empty() {
            line.push(" · ", Some(Role::MutedText));
            line.spans.extend(notice_line.spans);
            used_width += notice_width;
        }
        if let Some(background) = ordinary_background {
            let available = width.saturating_sub(used_width + 3);
            if available > 0 {
                line.push(" · ", Some(Role::MutedText));
                line.spans.extend(background.line(available).spans);
            }
        }
        line.truncate(width);
        line.filled(Role::StatusBar)
    }

    pub(super) fn panel_lines(
        &self,
        footer: &FooterFrame,
        width: usize,
        height: usize,
        display: DisplayProfile,
    ) -> Vec<RoleLine> {
        let row_count = self.row_count(height);
        self.panel_lines_with_rows(footer, width, row_count, display)
    }

    pub(super) fn panel_lines_with_rows(
        &self,
        footer: &FooterFrame,
        width: usize,
        row_count: usize,
        display: DisplayProfile,
    ) -> Vec<RoleLine> {
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
        let summary_focused = footer.items.iter().any(|item| item.selected);
        let summary = if let Some(panel) = self.visible {
            let count = self.item_count(panel);
            if footer.list_focused && panel == FooterItemId::Tasks {
                let action = if self.stoppable_task().is_some() {
                    " · x stop"
                } else {
                    ""
                };
                format!("{count} items · Enter details{action}")
            } else if footer.list_focused && panel == FooterItemId::Todo {
                format!("{count} items · Enter details")
            } else if summary_focused && !footer.list_focused {
                format!("{count} items · Enter list")
            } else if count > row_count.saturating_sub(1) {
                format!("{count} items · scroll")
            } else {
                format!("{count} items")
            }
        } else if summary_focused {
            "Enter/Space expand".to_owned()
        } else {
            "↓ open".to_owned()
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
        if panel == FooterItemId::Agents {
            let agents = self.visible_agents();
            let capacity = row_count.saturating_sub(1);
            let selected_index = agents
                .iter()
                .position(|row| Some(row.agent_id) == self.selected_agent)
                .unwrap_or(0);
            let offset = if selected_index < self.offset {
                selected_index
            } else if selected_index >= self.offset + capacity.max(1) {
                selected_index + 1 - capacity.max(1)
            } else {
                self.offset
            };
            let status = |row: &AgentTreeRow| {
                let mut status = vec![(row.status.clone(), row.status_role())];
                if Some(row.agent_id) == self.visible_agent {
                    status.push((" · ".into(), Role::MutedText));
                    status.push(("viewing".into(), Role::Text));
                }
                row.own_status_variants(status, width + 1)
            };
            for row in agents.iter().skip(offset).take(capacity) {
                let selected = footer.selected == Some(FooterItemId::Agents)
                    && Some(row.agent_id) == self.selected_agent;
                let line = row.summary_line(
                    width,
                    selected,
                    &RowExtras {
                        glyph: display.glyph(row.glyph),
                        ..RowExtras::default()
                    },
                    self.agent_ordinals.get(&row.agent_id).copied(),
                    &status(row),
                    MIN_REQUEST_AGENT_TITLE_COLUMNS,
                );
                lines.push(if selected {
                    line.filled(Role::Selection)
                } else {
                    line.filled(Role::StatusPanel)
                });
            }
            return lines;
        }
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
        for (index, item) in items.iter().enumerate().skip(offset).take(capacity) {
            let mut row = RoleLine::default();
            let selected = footer.list_focused
                && footer.selected == Some(panel)
                && match panel {
                    FooterItemId::Tasks => {
                        item.task_id.is_some() && item.task_id == self.selected_task
                    }
                    FooterItemId::Todo => index == self.selected_todo,
                    FooterItemId::Agents => false,
                };
            row.push(if selected { "› " } else { "  " }, Some(Role::Suggestion));
            row.push(format!("{}  ", item.id), Some(Role::MutedText));
            let right = format!("  {}", item.state);
            let title_width = width
                .saturating_sub(UnicodeWidthStr::width(item.id.as_str()) + 4)
                .saturating_sub(UnicodeWidthStr::width(right.as_str()));
            let mut title = RoleLine::plain(item.title.replace('\n', " "));
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
            FooterItemId::Agents => &[],
        }
    }

    fn item_count(&self, panel: FooterItemId) -> usize {
        if panel == FooterItemId::Agents {
            self.agents.len()
        } else {
            self.items(panel).len()
        }
    }

    fn header_label(&self, panel: FooterItemId) -> String {
        match panel {
            FooterItemId::Tasks => format!("Shell {}", self.tasks.len()),
            FooterItemId::Todo => format!("Todo {}", self.todos.len()),
            FooterItemId::Agents => format!("Agents {}", self.agents.len()),
        }
    }

    pub(super) fn panel_targets(
        &self,
        footer: &FooterFrame,
        width: usize,
    ) -> Vec<(std::ops::Range<usize>, FooterItemId)> {
        let mut start = 0;
        let mut targets = Vec::new();
        for item in &footer.items {
            let end = start + UnicodeWidthStr::width(self.header_label(item.id).as_str());
            if end <= width {
                targets.push((start..end, item.id));
            }
            start = end + 2;
        }
        targets
    }

    fn clamp_offset(&mut self) {
        let count = self.visible.map_or(0, |panel| self.item_count(panel));
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
            list_focused: selected.is_some(),
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
    fn known_context_uses_success_while_unknown_context_and_modes_keep_their_roles() {
        let context_role = |area: &StatusArea| {
            area.status_line(80, "")
                .spans
                .iter()
                .find(|span| span.text.contains("Context"))
                .and_then(|span| span.role)
        };
        let mut area = StatusArea::default();
        area.update("Auto".into(), "model".into(), None, vec![], vec![]);
        assert_eq!(context_role(&area), Some(Role::MutedText));
        assert_eq!(area.status_line(80, "").spans[0].role, Some(Role::Warning));
        area.update(
            "Auto".into(),
            "model".into(),
            Some((42_000, 200_000)),
            vec![],
            vec![],
        );
        assert_eq!(context_role(&area), Some(Role::Success));
        assert!(
            area.status_line(80, "")
                .spans
                .iter()
                .all(|span| span.role != Some(Role::Error))
        );
        area.update("Plan".into(), "model".into(), None, vec![], vec![]);
        assert_eq!(area.status_line(80, "").spans[0].role, Some(Role::PlanMode));
    }

    #[test]
    fn background_waiting_facts_and_agent_labels_use_separate_semantic_roles() {
        let mut area = StatusArea::default();
        area.update("Auto".into(), "model".into(), None, vec![], vec![]);
        for message in ["waiting for approval", "waiting for your answer"] {
            let notice = BackgroundStatusNotice {
                title: Some("Review the implementation".into()),
                ordinal: Some(2),
                message: message.into(),
                message_role: Role::Permission,
                compact_message: None,
                necessary: true,
            };
            for width in [20, 40, 120] {
                let line = area.status_line_with_background(width, "", Some(&notice));
                let waiting = line
                    .spans
                    .iter()
                    .find(|span| span.text.starts_with("waiting"));
                assert_eq!(
                    waiting.and_then(|span| span.role),
                    Some(Role::Permission),
                    "{line:?}"
                );
                for span in &line.spans {
                    if span.text.contains("Agent ") {
                        assert_eq!(span.role, Some(Role::Text), "{line:?}");
                    }
                    if span.text == " · " {
                        assert_eq!(span.role, Some(Role::MutedText), "{line:?}");
                    }
                }
            }
        }
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
        let focused_list = footer(
            &[FooterItemId::Tasks, FooterItemId::Todo],
            Some(FooterItemId::Tasks),
        );
        let footer = footer(&[FooterItemId::Tasks, FooterItemId::Todo], None);
        let collapsed = area.panel_lines(&footer, 80, 24, DisplayProfile::default());
        assert_eq!(collapsed.len(), 2);
        assert!(collapsed[0].text().contains("Shell 6  Todo 1"));
        assert!(
            collapsed[0].text().contains("1 items"),
            "automatic selection leaves editor ownership unchanged"
        );
        area.select(FooterItemId::Tasks);
        assert!(
            area.panel_lines(&focused_list, 80, 24, DisplayProfile::default())[0]
                .text()
                .contains("Enter details"),
            "a focused task list names its Enter action"
        );
        let first = area.panel_lines(&footer, 80, 24, DisplayProfile::default());
        assert_eq!(first.len(), 5);
        assert!(first[0].text().contains("Shell 6  Todo 1"));
        assert!(first[0].text().contains("6 items · scroll"));
        assert!(first[1].text().contains("task 1"));
        assert!(first[4].text().contains("task 4"));
        area.scroll(2, 4);
        assert!(
            area.panel_lines(&footer, 80, 24, DisplayProfile::default())[1]
                .text()
                .contains("task 3")
        );
        area.select(FooterItemId::Todo);
        let todo = area.panel_lines(&footer, 80, 24, DisplayProfile::default());
        assert_eq!(todo.len(), 2);
        assert!(todo[1].text().contains("review plan"));
        area.release_panel();
        area.update(
            "Auto".into(),
            "model".into(),
            None,
            vec![item("sh-1", "task 1")],
            vec![item("1.", "review plan")],
        );
        assert_eq!(
            area.panel_lines(&footer, 80, 24, DisplayProfile::default())
                .len(),
            2
        );
        area.select(FooterItemId::Tasks);
        assert!(
            area.panel_lines(&footer, 80, 24, DisplayProfile::default())[1]
                .text()
                .contains("task 1")
        );
        area.update("Auto".into(), "model".into(), None, vec![], vec![]);
        assert!(
            area.panel_lines(&footer, 80, 24, DisplayProfile::default())
                .is_empty()
        );
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
            area.panel_lines(&footer, 80, 24, DisplayProfile::default())[4]
                .text()
                .contains("› sh-5")
        );
        assert!(
            area.panel_lines(&footer, 80, 8, DisplayProfile::default())[2]
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
        assert_eq!(area.selected_task(), Some(Uuid::from_u128(4)));
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
        let row = &area.panel_lines(
            &footer(&[FooterItemId::Tasks], None),
            80,
            24,
            DisplayProfile::default(),
        )[1];
        assert!(row.spans.iter().any(|span| {
            span.text.contains("running") && span.role == Some(Role::RunningShade0)
        }));
    }
}
