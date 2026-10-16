use std::collections::{HashMap, HashSet};

use unicode_width::UnicodeWidthStr;
use uuid::Uuid;

use super::{
    console::{AgentRow, truncate_width},
    display::{DisplayProfile, Glyph},
    footer::{FooterFrame, FooterItemId},
    style::{Role, RoleLine},
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tickloop_agent_protocol::{LoopState, SharedTaskStatus, TaskBoard};

const MAX_PANEL_ITEMS: usize = 8;
const COMPACT_TASK_ROWS: usize = 3;
const TASK_COMPACT_RESERVED_ROWS: usize = 12;
const TASK_ROW_FIXED_COLUMNS: usize = 5;
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

#[derive(Clone, Copy, Default, PartialEq)]
enum TaskPresentation {
    #[default]
    Compact,
    Expanded,
    Collapsed,
}

#[derive(Clone, Copy, Default)]
struct TaskViewState {
    presentation: TaskPresentation,
    selected: usize,
}

#[derive(Default)]
pub(super) struct StatusArea {
    mode: String,
    model: String,
    context: Option<(u64, usize)>,
    tasks: Option<TaskBoard>,
    task_view: TaskViewState,
    task_views: HashMap<(Uuid, Uuid), TaskViewState>,
    tasks_focused: bool,
    task_focus_available: Option<bool>,
    task_details: bool,
    agents: Vec<AgentRow>,
    /// The family member this View shows.
    visible_agent: Option<Uuid>,
    selected_agent: Option<Uuid>,
    collapsed_agents: HashSet<Uuid>,
    pub(super) agents_collapsed: bool,
    panel_offsets: HashMap<FooterItemId, usize>,
    visible: Option<FooterItemId>,
    offset: usize,
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

    /// Returns whether anything the status area shows changed.
    pub(super) fn update(
        &mut self,
        mode: String,
        model: String,
        context: Option<(u64, usize)>,
        tasks: Option<TaskBoard>,
    ) -> bool {
        let context = context.filter(|(_, limit)| *limit > 0);
        let changed = self.mode != mode
            || self.model != model
            || self.context != context
            || self.tasks != tasks;
        self.mode = mode;
        self.model = model;
        self.context = context;
        let previous_scope = self
            .tasks
            .as_ref()
            .map(|board| (board.root_chat_id, board.root_loop_id));
        let next_scope = tasks
            .as_ref()
            .map(|board| (board.root_chat_id, board.root_loop_id));
        if previous_scope != next_scope {
            if let Some(scope) = previous_scope {
                self.task_views.insert(scope, self.task_view);
            }
            self.task_view = next_scope
                .and_then(|scope| self.task_views.get(&scope).copied())
                .unwrap_or_default();
            self.tasks_focused = false;
            self.task_details = false;
        }
        self.tasks = tasks;
        let count = self.tasks.as_ref().map_or(0, |board| board.tasks.len());
        self.task_view.selected = self.task_view.selected.min(count.saturating_sub(1));
        if count == 0 {
            self.release_task_focus();
        }

        self.reconcile_panel();
        self.clamp_offset();
        changed
    }

    /// Returns whether the shown mode changed.
    pub(super) fn set_mode(&mut self, mode: String) -> bool {
        let changed = self.mode != mode;
        self.mode = mode;
        changed
    }

    pub(super) fn available_panels(&self) -> impl Iterator<Item = FooterItemId> + '_ {
        [FooterItemId::Agents]
            .into_iter()
            .filter(|_| self.family_row_count() > 0)
    }

    pub(super) fn agent_title(&self, agent_id: Uuid) -> Option<&str> {
        self.agents
            .iter()
            .find(|row| row.agent_id == agent_id)
            .map(|row| {
                row.subagent
                    .as_ref()
                    .map_or(row.title.as_str(), |registration| {
                        registration.handle.as_str()
                    })
            })
    }

    pub(super) fn viewed_child(&self) -> Option<&AgentRow> {
        self.agents.iter().find(|row| {
            Some(row.agent_id) == self.visible_agent
                && (row.subagent.is_some() || row.parent_id.is_some())
        })
    }

    /// Replaces the visible family rows.
    pub(super) fn update_agents(&mut self, rows: Vec<AgentRow>, visible: Option<Uuid>) {
        self.visible_agent = visible;
        self.agents = rows;
        self.restore_agent_selection();
        self.reconcile_panel();
        self.clamp_offset();
    }

    /// Keeps the remembered row while the list shows it; otherwise falls back to the viewed
    /// member, then to the first row.
    pub(super) fn restore_agent_selection(&mut self) {
        let agents = self.visible_agents();
        let shown = |id: Option<Uuid>| id.filter(|id| agents.iter().any(|row| row.agent_id == *id));
        self.selected_agent = shown(self.selected_agent)
            .or(shown(self.visible_agent))
            .or_else(|| agents.first().map(|row| row.agent_id));
    }

    /// Carries the remembered row into another member's View, so switching members keeps it.
    pub(super) fn adopt_agent_selection(&mut self, other: &Self) {
        if other.selected_agent.is_some() {
            self.selected_agent = other.selected_agent;
        }
    }

    pub(super) fn adopt_task_views(&mut self, other: &Self) {
        self.task_views.extend(other.task_views.clone());
        if let Some(board) = &other.tasks {
            self.task_views
                .insert((board.root_chat_id, board.root_loop_id), other.task_view);
        }
        if let Some(board) = &self.tasks
            && let Some(view) = self
                .task_views
                .get(&(board.root_chat_id, board.root_loop_id))
        {
            self.task_view = *view;
            self.task_view.selected = self
                .task_view
                .selected
                .min(board.tasks.len().saturating_sub(1));
        }
        self.release_task_focus();
    }

    /// The selected member when it is a finished subagent the panel may drop now.
    pub(super) fn selected_dismissible(&self) -> Option<Uuid> {
        self.selected_agent()
            .filter(|id| Some(*id) != self.visible_agent)
            .and_then(|id| self.agents.iter().find(|row| row.agent_id == id))
            .filter(|row| {
                row.parent_id.is_some()
                    && matches!(
                        row.state,
                        LoopState::Completed
                            | LoopState::Failed
                            | LoopState::Stopped
                            | LoopState::Interrupted
                            | LoopState::Expired
                    )
            })
            .map(|row| row.agent_id)
    }

    pub(super) fn selected_agent(&self) -> Option<Uuid> {
        (self.visible == Some(FooterItemId::Agents))
            .then_some(self.selected_agent)
            .flatten()
    }

    fn visible_agents(&self) -> Vec<&AgentRow> {
        self.agents
            .iter()
            .filter(|row| {
                let mut parent = row.parent_id;
                let mut visited = std::collections::HashSet::new();
                while let Some(id) = parent {
                    if !visited.insert(id) {
                        return false;
                    }
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

    pub(super) fn selection_at_start(&self) -> bool {
        self.selected_agent == self.visible_agents().first().map(|row| row.agent_id)
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
        if self.family_row_count() == 0 {
            return;
        }
        self.show_panel(Some(panel));
    }

    pub(super) fn reconcile_panel(&mut self) {
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

    pub(super) fn is_open(&self) -> bool {
        self.visible.is_some()
            && !(self.visible == Some(FooterItemId::Agents) && self.agents_collapsed)
    }

    pub(super) fn toggle_agents(&mut self) {
        self.select(FooterItemId::Agents);
        self.agents_collapsed = !self.agents_collapsed;
    }

    #[cfg(test)]
    pub(super) fn selected_panel(&self) -> Option<FooterItemId> {
        self.visible
    }

    pub(super) fn row_count(&self, height: usize) -> usize {
        if self
            .tasks
            .as_ref()
            .is_none_or(|board| board.tasks.is_empty())
            && self.agents.is_empty()
        {
            return 0;
        }
        if height < 4 {
            return 0;
        }
        let Some(panel) = self.visible else {
            return 1;
        };
        if panel == FooterItemId::Agents && self.agents_collapsed {
            return 1;
        }
        1 + self
            .family_row_count()
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
        let (context, context_role) = match self.context {
            Some((used, limit)) => {
                let percent =
                    used.saturating_mul(100).saturating_add(limit as u64 / 2) / limit as u64;
                (format!("Context {percent}% used"), Role::Success)
            }
            None => ("Context unknown".into(), Role::MutedText),
        };
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
        for (text, role) in [
            (Some(self.model.as_str()), Role::Text),
            (Some(context.as_str()), context_role),
        ] {
            let Some(text) = text else {
                continue;
            };
            let segment_width = 3 + UnicodeWidthStr::width(text);
            if used_width + segment_width + notice_width + outcome_width > width {
                break;
            }
            line.push(" · ", Some(Role::MutedText));
            line.push(text.to_owned(), Some(role));
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

    pub(super) fn release_task_focus(&mut self) {
        self.tasks_focused = false;
        self.task_details = false;
    }

    pub(super) fn task_key(&mut self, key: KeyEvent) -> bool {
        let count = self.tasks.as_ref().map_or(0, |board| board.tasks.len());
        if count == 0 || self.task_focus_available == Some(false) {
            return false;
        }
        if key.code == KeyCode::Char('t') && key.modifiers == KeyModifiers::CONTROL {
            self.task_view.presentation = if self.tasks_focused {
                TaskPresentation::Collapsed
            } else {
                TaskPresentation::Expanded
            };
            self.tasks_focused = self.task_view.presentation == TaskPresentation::Expanded;
            self.task_details = false;
            return true;
        }
        if !self.tasks_focused {
            return false;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => {
                if self.task_details {
                    self.task_details = false;
                } else {
                    self.release_task_focus();
                }
            }
            (KeyCode::Down | KeyCode::Char('j'), KeyModifiers::NONE)
            | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                self.task_view.selected = (self.task_view.selected + 1).min(count - 1);
                self.task_details = false;
            }
            (KeyCode::Up | KeyCode::Char('k'), KeyModifiers::NONE)
            | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                self.task_view.selected = self.task_view.selected.saturating_sub(1);
                self.task_details = false;
            }
            (KeyCode::Enter, KeyModifiers::NONE) => self.task_details = !self.task_details,
            _ => {
                self.release_task_focus();
                return false;
            }
        }
        true
    }

    pub(super) fn tasks_can_take_focus(&self) -> bool {
        self.task_focus_available == Some(true)
    }

    pub(super) fn task_lines_in_viewport(
        &mut self,
        width: usize,
        height: usize,
        viewport_rows: usize,
        display: DisplayProfile,
    ) -> Vec<RoleLine> {
        let (preview, selected_row) = self.task_frame(width, height, display, true);
        let available = selected_row.is_some_and(|row| {
            // The scrollback tail reserves a blank row below the task list.
            viewport_rows > preview.len().saturating_sub(row)
        });
        self.task_focus_available = Some(available);
        if !available {
            self.release_task_focus();
        }
        self.task_lines(width, height, display)
    }

    pub(super) fn task_lines(
        &self,
        width: usize,
        height: usize,
        display: DisplayProfile,
    ) -> Vec<RoleLine> {
        self.task_frame(width, height, display, false).0
    }

    fn task_frame(
        &self,
        width: usize,
        height: usize,
        display: DisplayProfile,
        focus_preview: bool,
    ) -> (Vec<RoleLine>, Option<usize>) {
        let Some(board) = self.tasks.as_ref().filter(|board| !board.tasks.is_empty()) else {
            return (Vec::new(), None);
        };
        let expanded = focus_preview || self.task_view.presentation == TaskPresentation::Expanded;
        let focused = focus_preview || self.tasks_focused;
        let completed = board
            .tasks
            .iter()
            .filter(|task| task.status == SharedTaskStatus::Completed)
            .count();
        let mut header = RoleLine::role(
            format!(
                "  Tasks · {completed}/{} completed · {}",
                board.tasks.len(),
                if self.task_focus_available == Some(false) {
                    "Resize to view"
                } else if focused {
                    "Ctrl+T collapse"
                } else if expanded {
                    "Ctrl+T focus"
                } else {
                    "Ctrl+T expand"
                }
            ),
            Role::MutedText,
        );
        header.truncate(width);
        let mut lines = vec![header];
        if !focus_preview && self.task_view.presentation == TaskPresentation::Collapsed {
            return (lines, None);
        }
        let capacity = if expanded {
            let reserved = if self.task_details { 20 } else { 16 };
            (height.saturating_sub(reserved) / 2).clamp(1, MAX_PANEL_ITEMS)
        } else {
            height
                .saturating_sub(TASK_COMPACT_RESERVED_ROWS)
                .min(COMPACT_TASK_ROWS)
        };
        if capacity == 0 {
            return (lines, None);
        }
        let start = if expanded {
            self.task_view.selected.saturating_sub(capacity - 1)
        } else {
            0
        };
        let mut visible_tasks = board.tasks.iter().enumerate().collect::<Vec<_>>();
        if !expanded {
            visible_tasks.sort_by_key(|(_, task)| match task.status {
                SharedTaskStatus::NeedsAttention => 0,
                SharedTaskStatus::InProgress => 1,
                SharedTaskStatus::Pending => 2,
                SharedTaskStatus::Completed | SharedTaskStatus::Cancelled => 3,
            });
        }
        let mut selected_row = None;
        for (index, task) in visible_tasks.into_iter().skip(start).take(capacity) {
            let (glyph, role, label) = match task.status {
                SharedTaskStatus::Pending => (Glyph::TodoPending, Role::MutedText, "pending"),
                SharedTaskStatus::InProgress => {
                    (Glyph::TodoInProgress, Role::InProgressTodo, "in progress")
                }
                SharedTaskStatus::Completed => (Glyph::TodoCompleted, Role::Success, "completed"),
                SharedTaskStatus::Cancelled => (Glyph::TodoPending, Role::MutedText, "cancelled"),
                SharedTaskStatus::NeedsAttention => {
                    (Glyph::TodoPending, Role::Error, "needs attention")
                }
            };
            let title = super::safe_text(&task.title).replace(['\n', '\t'], " ");
            let blocked = board.blocked_by(task);
            let mut details = label.to_owned();
            if board.is_team
                && let Some(owner) = &task.owner
            {
                details.push_str(&format!(" · {}", super::safe_text(&owner.name)));
            }
            if !blocked.is_empty() {
                details.push_str(&format!(" · blocked ({})", blocked.len()));
            }
            let title = if expanded {
                title
            } else {
                super::conversation_picker::ellipsize(
                    &title,
                    width.saturating_sub(
                        details.width() + display.glyph(glyph).width() + TASK_ROW_FIXED_COLUMNS,
                    ),
                )
            };
            if index == self.task_view.selected {
                selected_row = Some(lines.len());
            }
            lines.extend(
                super::list_row::ListRow {
                    selected: focused && index == self.task_view.selected,
                    glyph: display.glyph(glyph),
                    glyph_role: role,
                    name: &title,
                    details,
                    fill: None,
                    subscription: None,
                    bar_columns: None,
                }
                .lines(width)
                .into_iter()
                .take(if expanded { 2 } else { 1 }),
            );
            if index == self.task_view.selected && self.task_details {
                for text in [
                    task.description.clone(),
                    format!(
                        "Dependencies: {}",
                        if task.dependencies.is_empty() {
                            "none".into()
                        } else {
                            task.dependencies
                                .iter()
                                .map(|id| {
                                    board
                                        .tasks
                                        .iter()
                                        .find(|dependency| dependency.id == *id)
                                        .map_or_else(
                                            || id.to_string(),
                                            |dependency| dependency.title.clone(),
                                        )
                                })
                                .collect::<Vec<_>>()
                                .join(", ")
                        }
                    ),
                    task.result.clone().unwrap_or_default(),
                ] {
                    if !text.is_empty() {
                        let mut line = RoleLine::role(
                            format!("    {}", super::safe_text(&text).replace(['\n', '\t'], " ")),
                            Role::MutedText,
                        );
                        line.truncate(width);
                        lines.push(line);
                    }
                }
            }
        }
        if expanded && board.tasks.len() > capacity {
            lines.push(RoleLine::role(
                format!(
                    "  {}–{} of {}",
                    start + 1,
                    (start + capacity).min(board.tasks.len()),
                    board.tasks.len()
                ),
                Role::MutedText,
            ));
        }
        if focused {
            let mut hint = RoleLine::role("  j/k move · Enter details · Esc back", Role::MutedText);
            hint.truncate(width);
            lines.push(hint);
        }
        for line in &mut lines {
            line.truncate(width);
        }
        (lines, selected_row)
    }

    /// When a working member's run time next changes the open Subagents list.
    pub(super) fn next_frame_ms(&self, now: u64) -> Option<u64> {
        if !self.is_open() {
            return None;
        }
        self.visible_agents()
            .into_iter()
            .filter_map(|row| super::subagent_panel::next_change_ms(row, now))
            .min()
    }

    pub(super) fn panel_lines_with_rows(
        &self,
        footer: &FooterFrame,
        width: usize,
        row_count: usize,
        display: DisplayProfile,
    ) -> Vec<RoleLine> {
        self.panel_lines_at(
            footer,
            width,
            row_count,
            display,
            tickloop_agent_base::private_fs::unix_time_milliseconds(),
        )
    }

    pub(super) fn panel_lines_at(
        &self,
        footer: &FooterFrame,
        width: usize,
        row_count: usize,
        display: DisplayProfile,
        now: u64,
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
        let render_header = !footer.items.is_empty();
        header.truncate(width);
        let mut lines = if render_header {
            vec![header]
        } else {
            Vec::new()
        };
        if self.visible.is_none() || self.agents_collapsed {
            return lines;
        }
        let agents = self.visible_agents();
        let capacity = row_count.saturating_sub(usize::from(render_header));
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
        let name_columns = agents
            .iter()
            .filter(|row| row.subagent.is_some() || row.parent_id.is_some())
            .map(|row| {
                UnicodeWidthStr::width(
                    row.subagent
                        .as_ref()
                        .map_or("child", |registration| registration.handle.as_str()),
                )
            })
            .max()
            .unwrap_or(4);
        for row in agents.iter().skip(offset).take(capacity) {
            let selected = footer.selected == Some(FooterItemId::Agents)
                && Some(row.agent_id) == self.selected_agent;
            let line = super::subagent_panel::family_line(
                row,
                selected,
                Some(row.agent_id) == self.visible_agent,
                (width, name_columns, display, now),
            );
            lines.push(if selected {
                line.filled(Role::Selection)
            } else {
                line
            });
        }
        lines
    }

    fn family_row_count(&self) -> usize {
        if self
            .agents
            .iter()
            .any(|row| row.subagent.is_some() || row.parent_id.is_some())
        {
            self.agents.len()
        } else {
            0
        }
    }

    pub(super) fn header_label(&self, panel: FooterItemId) -> String {
        match panel {
            FooterItemId::Agents => {
                let subagent_count = self
                    .agents
                    .iter()
                    .filter(|row| row.subagent.is_some() || row.parent_id.is_some())
                    .count();
                format!("Subagents {subagent_count}")
            }
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
        let count = self.visible.map_or(0, |_| self.family_row_count());
        self.offset = self.offset.min(count.saturating_sub(1));
    }
}

#[cfg(test)]
pub(super) fn test_task_board(titles: &[&str]) -> TaskBoard {
    let mut board = TaskBoard::new(Uuid::from_u128(1010), Uuid::from_u128(1012));
    board.tasks = titles
        .iter()
        .enumerate()
        .map(|(index, title)| tickloop_agent_protocol::SharedTask {
            id: Uuid::from_u128(2000 + index as u128),
            title: (*title).into(),
            description: "Verify the task".into(),
            dependencies: vec![],
            status: SharedTaskStatus::InProgress,
            owner: Some(tickloop_agent_protocol::TaskOwner {
                chat_id: Uuid::from_u128(1010),
                name: "main".into(),
            }),
            claim: None,
            delegated_runs: Vec::new(),
            revision: 1,
            result: None,
            report: None,
        })
        .collect();
    board
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subagent_panel_labels_only_the_family_root_main() {
        let root = Uuid::from_u128(1);
        let row = |id, parent, title: &str| AgentRow {
            agent_id: Uuid::from_u128(id),
            parent_id: parent,
            title: title.into(),
            status: "Running".into(),
            state: tickloop_agent_protocol::LoopState::Running,
            project: String::new(),
            workspace: None,
            approvals: 0,
            questions: 0,
            group: crate::agent_console::AttentionGroup::Working,
            glyph: Glyph::AgentRunning,
            preview: None,
            updated_at: None,
            run_started_at: None,
            subagent: parent.map(|_| tickloop_agent_protocol::SubagentRegistration {
                child_chat_id: Uuid::from_u128(id),
                handle: title.into(),
                description: title.into(),
                created_at: tickloop_agent_base::private_fs::unix_time_milliseconds(),
                color: None,
            }),
        };
        let mut area = StatusArea::default();
        area.update_agents(
            vec![
                row(1, None, "Long original human message"),
                row(2, Some(root), "file-check"),
            ],
            Some(root),
        );
        let footer = FooterFrame {
            items: Vec::new(),
            selected: None,
        };
        let lines = area.panel_lines_with_rows(&footer, 48, 3, DisplayProfile::default());
        assert_eq!(lines[0].text(), "  ● main");
        assert!(lines[1].text().contains("○ file-check"), "{lines:?}");
        assert!(
            !lines
                .iter()
                .any(|line| line.text().contains("Long original"))
        );
    }

    #[test]
    fn subagent_panel_shows_run_elapsed_and_explicit_idle_state() {
        use crate::{agent_console::AttentionGroup, terminal::style::ThemeName};
        let root = Uuid::from_u128(1);
        let child = Uuid::from_u128(2);
        let row = |id, parent, title: &str| AgentRow {
            agent_id: id,
            parent_id: parent,
            title: title.into(),
            status: "Running".into(),
            state: tickloop_agent_protocol::LoopState::Running,
            project: String::new(),
            workspace: None,
            approvals: 0,
            questions: 0,
            group: AttentionGroup::Working,
            glyph: Glyph::AgentRunning,
            preview: Some("Run sleep 30".into()),
            updated_at: Some(100_000),
            run_started_at: Some(100_000),
            subagent: parent.map(|_| tickloop_agent_protocol::SubagentRegistration {
                child_chat_id: id,
                handle: title.into(),
                description: "Run sleep 30".into(),
                created_at: 10_000,
                color: Some(tickloop_agent_protocol::AgentColor::Cyan),
            }),
        };
        let mut area = StatusArea::default();
        let mut rows = vec![
            row(root, None, "root title"),
            row(child, Some(root), "sleep-30"),
        ];
        area.update_agents(rows.clone(), Some(child));
        for theme in ThemeName::ALL {
            let mut display = DisplayProfile::default();
            display.set_theme(theme);
            let footer = FooterFrame {
                items: Vec::new(),
                selected: None,
            };
            let lines = area.panel_lines_at(&footer, 80, 3, display, 161_000);
            assert!(lines[1].text().ends_with("1m 1s"), "{lines:?}");
            assert!(lines[1].text().contains("Run sleep 30"));
            assert!(lines.iter().all(|line| line.fill.is_none()));
            assert_eq!(lines[1].spans[1].role, Some(Role::AgentCyan));
            assert_eq!(
                lines[1]
                    .spans
                    .iter()
                    .filter(|span| span.role == Some(Role::AgentCyan))
                    .count(),
                1,
                "only the dot wears the member's Agent Color: {:?}",
                lines[1]
            );
            assert_eq!(lines[0].spans[0].role, Some(Role::MutedText));
            for width in [1, 8, 16, 32, 80] {
                assert!(
                    area.panel_lines_at(&footer, width, 3, display, 161_000)
                        .iter()
                        .all(|line| UnicodeWidthStr::width(line.text().as_str()) <= width)
                );
            }
            let focused = FooterFrame {
                items: Vec::new(),
                selected: Some(FooterItemId::Agents),
            };
            let selected = area.panel_lines_at(&focused, 80, 3, display, 161_000);
            assert_eq!(
                selected
                    .iter()
                    .filter(|line| line.fill == Some(Role::Selection))
                    .count(),
                1
            );
            assert!(
                selected
                    .iter()
                    .all(|line| line.fill.is_none() || line.fill == Some(Role::Selection))
            );
        }
        rows[1].run_started_at = None;
        area.update_agents(rows.clone(), Some(child));
        let footer = FooterFrame {
            items: Vec::new(),
            selected: None,
        };
        let line = &area.panel_lines_at(&footer, 80, 3, DisplayProfile::default(), 161_000)[1];
        assert!(
            line.text().ends_with("Running"),
            "missing Run timestamps cannot use the member creation time: {line:?}"
        );
        assert_eq!(area.next_frame_ms(161_000), None);
        rows[1].run_started_at = Some(100_000);
        rows[1].status = "En cours".into();
        area.update_agents(rows.clone(), Some(child));
        let unfocused = FooterFrame {
            items: Vec::new(),
            selected: None,
        };
        assert!(
            area.panel_lines_at(&unfocused, 80, 3, DisplayProfile::default(), 161_000)[1]
                .text()
                .ends_with("1m 1s")
        );
        let dot = |now| {
            area.panel_lines_at(&unfocused, 80, 3, DisplayProfile::default(), now)[1].spans[1]
                .clone()
        };
        assert_eq!(dot(161_000).text, "●", "the viewed member's dot is filled");
        assert_eq!(
            dot(161_000).role,
            Some(Role::AgentCyan),
            "the dot names the member, not its state"
        );
        assert_eq!(dot(161_500).role, Some(Role::AgentCyan), "it never blinks");
        assert_eq!(
            area.next_frame_ms(161_100),
            Some(162_000),
            "only the run time ticks"
        );
        rows[1].group = AttentionGroup::Waiting;
        rows[1].state = tickloop_agent_protocol::LoopState::WaitingTrigger;
        rows[1].glyph = Glyph::AgentWaiting;
        rows[1].status = "Waiting · trigger".into();
        area.update_agents(rows.clone(), Some(child));
        let display = DisplayProfile::default();
        assert_ne!(
            display.glyph(Glyph::AgentWaiting),
            display.glyph(Glyph::AgentIdle)
        );
        let waiting = &area.panel_lines_at(&unfocused, 80, 3, display, 181_000)[1];
        assert!(
            waiting.text().ends_with("Waiting · trigger"),
            "a waiting member shows its state: {waiting:?}"
        );
        assert_eq!(waiting.spans[1].role, Some(Role::AgentCyan));
        rows[1].group = AttentionGroup::Idle;
        rows[1].state = tickloop_agent_protocol::LoopState::Idle;
        rows[1].status = "Idle".into();
        rows[1].glyph = Glyph::AgentIdle;
        area.update_agents(rows.clone(), Some(child));
        let footer = FooterFrame {
            items: Vec::new(),
            selected: None,
        };
        let idle = &area.panel_lines_at(&footer, 80, 3, DisplayProfile::default(), 181_000)[1];
        assert!(idle.text().ends_with("idle"), "{idle:?}");
        assert_eq!(area.next_frame_ms(181_000), None);
        rows[1].group = AttentionGroup::Working;
        rows[1].state = tickloop_agent_protocol::LoopState::Running;
        rows[1].status = "Running".into();
        rows[1].glyph = Glyph::AgentRunning;
        area.update_agents(rows, Some(child));
        for (elapsed, expected) in [
            (500, "0s"),
            (59_999, "59s"),
            (60_000, "1m 0s"),
            (119_500, "2m 0s"),
            (3_600_000, "1h 0m 0s"),
            (86_400_000, "1d 0h 0m"),
        ] {
            assert!(
                area.panel_lines_at(&footer, 80, 3, DisplayProfile::default(), 100_000 + elapsed)
                    [1]
                .text()
                .ends_with(expected)
            );
        }
        assert!(
            area.panel_lines_at(&footer, 80, 3, DisplayProfile::default(), 221_000)[1]
                .text()
                .ends_with("2m 1s")
        );
    }

    #[test]
    fn context_segment_shows_zero_before_usage_with_a_known_window() {
        let mut area = StatusArea::default();
        area.update("Auto".into(), "model".into(), Some((0, 200_000)), None);
        assert_eq!(
            area.status_line(80, "").text(),
            "⏵⏵ Auto · model · Context 0% used"
        );
    }

    #[test]
    fn context_segment_shows_rounded_zero_usage() {
        let mut area = StatusArea::default();
        area.update("Auto".into(), "model".into(), Some((999, 200_000)), None);
        assert_eq!(
            area.status_line(80, "").text(),
            "⏵⏵ Auto · model · Context 0% used"
        );
    }

    #[test]
    fn context_segment_shows_unknown_when_usage_or_window_is_unavailable() {
        let mut area = StatusArea::default();
        for context in [None, Some((42, 0))] {
            area.update("Auto".into(), "model".into(), context, None);
            assert_eq!(
                area.status_line(80, "").text(),
                "⏵⏵ Auto · model · Context unknown"
            );
        }
    }

    #[test]
    fn context_segment_shows_rounded_usage_and_keeps_the_existing_fit_priority() {
        let mut area = StatusArea::default();
        area.update("Auto".into(), "model".into(), Some((1000, 200_000)), None);
        assert_eq!(
            area.status_line(80, "").text(),
            "⏵⏵ Auto · model · Context 1% used"
        );
        assert_eq!(area.status_line(16, "").text(), "⏵⏵ Auto · model");
        area.update("Auto".into(), "model".into(), None, None);
        assert_eq!(
            area.status_line(80, "Not saved").text(),
            "⏵⏵ Auto · model · Context unknown · Not saved"
        );
    }

    #[test]
    fn status_keeps_mode_and_notice_when_narrow_and_never_invents_usage() {
        let mut area = StatusArea::default();
        area.update("Auto".into(), "gpt-5.6".into(), None, None);
        assert_eq!(
            area.status_line(80, "").text(),
            "⏵⏵ Auto · gpt-5.6 · Context unknown"
        );
        let narrow = area.status_line(24, "Not saved").text();
        assert!(narrow.starts_with("⏵⏵ Auto · Not saved"));
        assert!(!narrow.contains("gpt-5.6"));
        area.update("Auto".into(), "gpt-5.6".into(), Some((0, 200_000)), None);
        assert_eq!(
            area.status_line(80, "").text(),
            "⏵⏵ Auto · gpt-5.6 · Context 0% used"
        );
        area.update(
            "Auto".into(),
            "gpt-5.6".into(),
            Some((42_000, 200_000)),
            None,
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
        area.update("Auto".into(), "model".into(), None, None);
        assert_eq!(context_role(&area), Some(Role::MutedText));
        assert_eq!(area.status_line(80, "").spans[0].role, Some(Role::Warning));
        area.update("Auto".into(), "model".into(), Some((42_000, 200_000)), None);
        assert_eq!(context_role(&area), Some(Role::Success));
        assert!(
            area.status_line(80, "")
                .spans
                .iter()
                .all(|span| span.role != Some(Role::Error))
        );
        area.update("Plan".into(), "model".into(), None, None);
        assert_eq!(area.status_line(80, "").spans[0].role, Some(Role::PlanMode));
    }

    #[test]
    fn background_waiting_facts_and_agent_labels_use_separate_semantic_roles() {
        let mut area = StatusArea::default();
        area.update("Auto".into(), "model".into(), None, None);
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
        area.update("Ask".into(), "model".into(), None, None);
        assert_eq!(
            area.status_line(80, "").spans[0].role,
            Some(Role::AutoAccept)
        );

        area.update("Full Access".into(), "model".into(), None, None);
        assert_eq!(area.status_line(80, "").spans[0].role, Some(Role::Error));
    }

    #[test]
    fn subagent_panel_dots_wear_agent_colors_and_remember_the_selection() {
        use crate::agent_console::AttentionGroup;
        use tickloop_agent_protocol::{AgentColor, SubagentRegistration};
        let root = Uuid::from_u128(1);
        let row = |id: u128, state: LoopState, group: AttentionGroup, completed: bool| AgentRow {
            agent_id: Uuid::from_u128(id),
            parent_id: (id != 1).then_some(root),
            title: format!("member-{id}"),
            status: format!("{state:?}"),
            state,
            project: String::new(),
            workspace: None,
            approvals: 0,
            questions: 0,
            group,
            glyph: super::super::console::status_glyph(group, completed),
            preview: None,
            updated_at: Some(64_000),
            run_started_at: None,
            subagent: (id != 1).then(|| SubagentRegistration {
                child_chat_id: Uuid::from_u128(id),
                handle: format!("member-{id}"),
                description: "work".into(),
                created_at: 1_000,
                color: Some(AgentColor::ALL[id as usize % 8]),
            }),
        };
        let mut area = StatusArea::default();
        area.update("Auto".into(), "model".into(), None, None);
        area.update_agents(
            vec![
                row(1, LoopState::Idle, AttentionGroup::Idle, false),
                row(2, LoopState::Completed, AttentionGroup::Done, true),
                row(3, LoopState::Failed, AttentionGroup::Errors, false),
                row(4, LoopState::Stopped, AttentionGroup::Done, false),
                row(5, LoopState::Interrupted, AttentionGroup::Done, false),
            ],
            Some(root),
        );
        let footer = FooterFrame {
            items: Vec::new(),
            selected: None,
        };
        let lines = area.panel_lines_at(&footer, 60, 6, DisplayProfile::default(), 70_000);
        assert!(
            lines[0]
                .spans
                .iter()
                .all(|span| span.role == Some(Role::Title)),
            "main's dot keeps the viewed row's colour: {:?}",
            lines[0]
        );
        for (index, line) in lines.iter().enumerate().take(5).skip(1) {
            assert_eq!(line.spans[1].text, "○");
            assert_eq!(
                line.spans[1].role,
                Some(Role::agent(AgentColor::ALL[(index + 1) % 8])),
                "{line:?}"
            );
            assert!(
                line.spans
                    .iter()
                    .filter(|span| span.text.contains("member-"))
                    .all(|span| span.role == Some(Role::MutedText)),
                "{line:?}"
            );
            let expected = ["idle", "Failed", "Stopped", "Interrupted"][index - 1];
            assert!(line.text().ends_with(expected), "{line:?}");
            let expected_role =
                [Role::MutedText, Role::Error, Role::Warning, Role::Warning][index - 1];
            assert_eq!(line.spans.last().unwrap().role, Some(expected_role));
            for word in ["done", "failed", "stopped", "✔", "✖"] {
                assert!(!line.text().contains(word), "{line:?}");
            }
        }
        assert_eq!(
            area.next_frame_ms(70_000),
            None,
            "finished members do not keep a running timer"
        );

        area.select(FooterItemId::Agents);
        area.restore_agent_selection();
        assert_eq!(area.selected_agent(), Some(root), "the viewed member first");
        area.move_agent_selection(2, 6);
        assert_eq!(area.selected_dismissible(), Some(Uuid::from_u128(3)));
        let mut next = StatusArea::default();
        next.adopt_agent_selection(&area);
        next.update_agents(area.agents.clone(), Some(Uuid::from_u128(2)));
        next.select(FooterItemId::Agents);
        assert_eq!(
            next.selected_agent(),
            Some(Uuid::from_u128(3)),
            "switching members keeps the remembered row"
        );
        next.update_agents(
            area.agents
                .iter()
                .filter(|row| row.agent_id != Uuid::from_u128(3))
                .cloned()
                .collect(),
            Some(Uuid::from_u128(2)),
        );
        assert_eq!(
            next.selected_agent(),
            Some(Uuid::from_u128(2)),
            "a departed row falls back to the viewed member"
        );
        assert_eq!(next.selected_dismissible(), None, "the viewed member stays");
    }
}

#[cfg(test)]
mod shared_task_tests {
    use super::*;
    #[test]
    fn tasks_are_hidden_when_empty_and_expand_with_owner_state_and_dependencies() {
        let mut area = StatusArea::default();
        assert!(
            area.task_lines(80, 24, DisplayProfile::default())
                .is_empty()
        );
        let mut board = test_task_board(&["Inspect", "Verify"]);
        board.is_team = true;
        board.tasks[0].status = SharedTaskStatus::Completed;
        board.tasks[1].dependencies = vec![board.tasks[0].id];
        area.update("Auto".into(), "model".into(), None, Some(board));
        let compact = area.task_lines(80, 24, DisplayProfile::default());
        assert_eq!(compact.len(), 3);
        assert!(compact[0].text().contains("1/2 completed"));
        assert!(area.task_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL)));
        let expanded = area.task_lines(80, 24, DisplayProfile::default());
        assert!(
            expanded
                .iter()
                .any(|line| line.text().contains("Inspect") && line.text().contains("main"))
        );
        area.task_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        area.task_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            area.task_lines(80, 24, DisplayProfile::default())
                .iter()
                .any(|line| line.text().contains("Dependencies: Inspect"))
        );
        area.release_task_focus();
        assert!(!area.task_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)));
        for width in [1, 8, 20, 80] {
            for line in area.task_lines(width, 10, DisplayProfile::default()) {
                assert!(UnicodeWidthStr::width(line.text().as_str()) <= width);
            }
        }
        area.update("Auto".into(), "model".into(), None, None);
        assert!(
            area.task_lines(80, 24, DisplayProfile::default())
                .is_empty()
        );
    }

    #[test]
    fn compact_tasks_adapt_to_height_and_solo_team_ownership() {
        let mut area = StatusArea::default();
        let mut board = test_task_board(&["Inspect", "Verify", "Publish", "Later"]);
        board.tasks[1].dependencies = vec![board.tasks[0].id];
        area.update("Auto".into(), "model".into(), None, Some(board.clone()));
        let compact = area.task_lines(80, 24, DisplayProfile::default());
        assert_eq!(compact.len(), 4);
        assert!(compact[1].text().contains("Inspect"));
        assert!(compact[1].text().contains("in progress"));
        assert!(compact[2].text().contains("blocked (1)"));
        assert!(compact.iter().all(|line| !line.text().contains("main")));
        assert!(compact.iter().all(|line| !line.text().contains("Later")));
        assert_eq!(area.task_lines(80, 13, DisplayProfile::default()).len(), 2);
        assert_eq!(area.task_lines(80, 10, DisplayProfile::default()).len(), 1);
        area.task_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        area.task_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
        area.task_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            area.task_lines(80, 24, DisplayProfile::default())
                .iter()
                .any(|line| line.text().contains("Dependencies: Inspect"))
        );
        area.release_task_focus();
        board.is_team = true;
        area.update("Auto".into(), "model".into(), None, Some(board));
        assert!(
            area.task_lines(80, 24, DisplayProfile::default())
                .iter()
                .any(|line| line.text().contains("Inspect") && line.text().contains("main"))
        );
    }

    #[test]
    fn unassigned_team_tasks_show_status_without_an_owner_placeholder() {
        let mut area = StatusArea::default();
        let mut board = test_task_board(&["Inspect", "Verify"]);
        board.is_team = true;
        board.tasks[0].owner = None;
        board.tasks[0].status = SharedTaskStatus::Pending;
        board.tasks[1].status = SharedTaskStatus::Pending;
        area.update("Auto".into(), "model".into(), None, Some(board));
        let lines = area.task_lines(80, 24, DisplayProfile::default());
        let unassigned = lines
            .iter()
            .find(|line| line.text().contains("Inspect"))
            .unwrap()
            .text();
        assert!(unassigned.ends_with("pending"), "{unassigned}");
        assert!(
            !unassigned.contains("unassigned") && !unassigned.contains('·'),
            "{unassigned}"
        );
        let assigned = lines
            .iter()
            .find(|line| line.text().contains("Verify"))
            .unwrap()
            .text();
        assert!(assigned.ends_with("pending · main"), "{assigned}");
    }

    #[test]
    fn manual_collapse_survives_updates_empty_data_and_scope_switching() {
        let mut area = StatusArea::default();
        let mut board = test_task_board(&["Inspect", "Verify"]);
        area.update("Auto".into(), "model".into(), None, Some(board.clone()));
        let toggle = KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL);
        area.task_key(toggle);
        area.task_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        area.task_key(toggle);
        assert!(!area.task_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)));
        board.tasks[0].status = SharedTaskStatus::Completed;
        board.is_team = true;
        board.revision += 1;
        area.update("Auto".into(), "model".into(), None, Some(board.clone()));
        assert_eq!(area.task_lines(80, 24, DisplayProfile::default()).len(), 1);
        area.update("Auto".into(), "model".into(), None, None);
        let mut other = board.clone();
        other.root_loop_id = Uuid::from_u128(9999);
        area.update("Auto".into(), "model".into(), None, Some(other));
        assert_eq!(area.task_lines(80, 24, DisplayProfile::default()).len(), 3);
        area.update("Auto".into(), "model".into(), None, Some(board.clone()));
        assert_eq!(area.task_lines(80, 24, DisplayProfile::default()).len(), 1);
        assert_eq!(area.task_view.selected, 1);
        let mut member = StatusArea::default();
        member.update("Auto".into(), "model".into(), None, Some(board));
        member.adopt_task_views(&area);
        assert_eq!(
            member.task_lines(80, 24, DisplayProfile::default()).len(),
            1
        );
        assert_eq!(member.task_view.selected, 1);
        assert!(!member.tasks_focused);
    }

    #[test]
    fn escape_releases_details_then_focus_without_collapsing() {
        let mut area = StatusArea::default();
        area.update(
            "Auto".into(),
            "model".into(),
            None,
            Some(test_task_board(&["Inspect"])),
        );
        area.task_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        area.task_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let escape = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert!(area.task_key(escape));
        assert!(area.tasks_focused);
        assert!(!area.task_details);
        assert!(area.task_key(escape));
        assert!(!area.tasks_focused);
        assert!(area.task_view.presentation == TaskPresentation::Expanded);
        assert!(
            area.task_lines(80, 24, DisplayProfile::default())
                .iter()
                .any(|line| line.text().contains("Inspect"))
        );
        assert!(!area.task_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)));
    }

    #[test]
    fn compact_tasks_prioritize_unfinished_work_without_reordering_full_list() {
        let mut area = StatusArea::default();
        let mut board =
            test_task_board(&["Finished", "Cancelled", "Pending", "Running", "Attention"]);
        board.tasks[0].status = SharedTaskStatus::Completed;
        board.tasks[1].status = SharedTaskStatus::Cancelled;
        board.tasks[2].status = SharedTaskStatus::Pending;
        board.tasks[4].status = SharedTaskStatus::NeedsAttention;
        area.update("Auto".into(), "model".into(), None, Some(board));
        let compact = area.task_lines(80, 24, DisplayProfile::default());
        assert!(compact[1].text().contains("Attention"));
        assert!(compact[2].text().contains("Running"));
        assert!(compact[3].text().contains("Pending"));
        area.task_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        let expanded = area.task_lines(80, 40, DisplayProfile::default());
        assert!(expanded[1].text().contains("Finished"));
        assert!(expanded[2].text().contains("Cancelled"));
    }

    #[test]
    fn task_focus_requires_room_for_the_selected_wrapped_row_and_hints() {
        let mut area = StatusArea::default();
        area.update(
            "Auto".into(),
            "model".into(),
            None,
            Some(test_task_board(&[
                "A long task title that wraps across the list row",
            ])),
        );
        let toggle = KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL);
        area.task_lines_in_viewport(24, 24, 3, DisplayProfile::default());
        assert!(!area.tasks_can_take_focus());
        assert!(!area.task_key(toggle));
        area.task_lines_in_viewport(24, 24, 4, DisplayProfile::default());
        assert!(area.tasks_can_take_focus());
        assert!(area.task_key(toggle));
        area.task_lines_in_viewport(24, 24, 3, DisplayProfile::default());
        assert!(!area.task_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)));
        assert!(area.task_view.presentation == TaskPresentation::Expanded);
    }
}
