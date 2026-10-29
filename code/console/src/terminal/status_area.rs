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
use tickloop_harness::protocol::{LoopState, SharedTask, SharedTaskStatus, TaskBoard};

const MAX_PANEL_ITEMS: usize = 8;
const MIN_COMPACT_TASK_ROWS: usize = 3;
const MAX_COMPACT_TASK_ROWS: usize = 5;
const TASK_COMPACT_HEIGHT_OFFSET: usize = 14;
const TASK_COMPACT_RESERVED_ROWS: usize = 12;
const MIN_TASK_TITLE_COLUMNS: usize = 8;
const MAX_BACKGROUND_TITLE_COLUMNS: usize = 24;
const BACKGROUND_AGENT_NAME_PREFIX: &str = "Agent ";
const STATUS_SEGMENT_SEPARATOR: &str = " · ";

/// What the status line's context segment shows: the share of the window used, in the warning
/// role once the context is nearly full.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ContextGauge {
    pub(super) used_tokens: u64,
    pub(super) window_tokens: usize,
    /// Within the warning buffer of the automatic compaction line.
    pub(super) is_nearly_full: bool,
}

impl From<(u64, usize)> for ContextGauge {
    fn from((used_tokens, window_tokens): (u64, usize)) -> Self {
        Self {
            used_tokens,
            window_tokens,
            is_nearly_full: false,
        }
    }
}

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

#[derive(Default)]
struct TaskCounts {
    pending: usize,
    in_progress: usize,
    completed: usize,
    needs_attention: usize,
    cancelled: usize,
}

impl TaskCounts {
    fn of<'a>(tasks: impl Iterator<Item = &'a SharedTask>) -> Self {
        let mut counts = Self::default();
        for task in tasks {
            match task.status {
                SharedTaskStatus::Pending => counts.pending += 1,
                SharedTaskStatus::InProgress => counts.in_progress += 1,
                SharedTaskStatus::Completed => counts.completed += 1,
                SharedTaskStatus::NeedsAttention => counts.needs_attention += 1,
                SharedTaskStatus::Cancelled => counts.cancelled += 1,
            }
        }
        counts
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
    context: Option<ContextGauge>,
    tasks: Option<TaskBoard>,
    task_view: TaskViewState,
    task_views: HashMap<
        (
            tickloop_harness::protocol::ChatId,
            tickloop_harness::protocol::LoopId,
        ),
        TaskViewState,
    >,
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
        context: Option<ContextGauge>,
        tasks: Option<TaskBoard>,
    ) -> bool {
        let context = context.filter(|gauge| gauge.window_tokens > 0);
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
                        LoopState::Idle | LoopState::Failed | LoopState::Interrupted
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
        self.status_line_with_indicator(width, notice, None, None)
    }

    pub(super) fn status_line_with_indicator(
        &self,
        width: usize,
        notice: &str,
        background: Option<&BackgroundStatusNotice>,
        indicator: Option<RoleLine>,
    ) -> RoleLine {
        let mut line = RoleLine::default();
        let compact_model = indicator
            .as_ref()
            .map(|_| super::loop_view_text::ellipsize(&self.model, width.saturating_sub(3) / 2));
        let model = compact_model.as_deref().unwrap_or(&self.model);
        let mode = format!("⏵⏵ {}", self.mode);
        let mode_role = match self.mode.as_str() {
            "Plan" => Role::PlanMode,
            "Ask" => Role::AutoAccept,
            "Full Access" => Role::Error,
            "Auto" => Role::Warning,
            _ => Role::Text,
        };
        let (context, context_role) = match self.context {
            Some(gauge) => {
                let window = gauge.window_tokens as u64;
                let percent = gauge
                    .used_tokens
                    .saturating_mul(100)
                    .saturating_add(window / 2)
                    / window;
                let role = if gauge.is_nearly_full {
                    Role::Warning
                } else {
                    Role::Success
                };
                (format!("Context {percent}% used"), role)
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
        if let Some(mut indicator) = indicator {
            let model_columns = UnicodeWidthStr::width(model);
            indicator.truncate(width.saturating_sub((model_columns + 3).min(width / 2)));
            line.spans.extend(indicator.spans);
        }
        let mode_columns = UnicodeWidthStr::width(mode.as_str());
        if line.spans.is_empty()
            || UnicodeWidthStr::width(line.text().as_str())
                + mode_columns
                + 6
                + UnicodeWidthStr::width(model)
                + notice_width
                + outcome_width
                <= width
        {
            if !line.spans.is_empty() {
                line.push(STATUS_SEGMENT_SEPARATOR, Some(Role::MutedText));
            }
            line.push(mode, Some(mode_role));
        }
        let mut used_width = UnicodeWidthStr::width(line.text().as_str());
        for (text, role) in [
            (Some(model), Role::Text),
            (Some(context.as_str()), context_role),
        ] {
            let Some(text) = text else {
                continue;
            };
            let segment_width = 3 + UnicodeWidthStr::width(text);
            if used_width + segment_width + notice_width + outcome_width > width {
                break;
            }
            line.push(STATUS_SEGMENT_SEPARATOR, Some(Role::MutedText));
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
        under_activity: bool,
    ) -> Vec<RoleLine> {
        let connector = under_activity.then(|| format!("  {}", display.glyph(Glyph::ToolResult)));
        let connector_columns = connector.as_ref().map_or(0, |prefix| prefix.width());
        let task_columns = width.saturating_sub(connector_columns);
        let (preview, selected_row) = self.task_frame(task_columns, height, display, true);
        let available = task_columns
            > super::list_row::ROW_INDENT.width() + display.glyph(Glyph::TodoPending).width()
            && selected_row.is_some_and(|row| {
                // The scrollback tail reserves a blank row below the task list.
                viewport_rows > preview.len().saturating_sub(row)
            });
        self.task_focus_available = Some(available);
        if !available {
            self.release_task_focus();
        }
        let mut lines = self.task_lines(task_columns, height, display);
        if let Some(connector) = connector {
            for (index, line) in lines.iter_mut().enumerate() {
                let mut nested = RoleLine::role(
                    if index == 0 {
                        connector.clone()
                    } else {
                        " ".repeat(connector_columns)
                    },
                    Role::MutedText,
                );
                nested.spans.append(&mut line.spans);
                line.spans = nested.spans;
                if line.fill.is_some() {
                    line.fill_start += connector_columns;
                }
                line.truncate(width);
            }
        }
        lines
    }

    pub(super) fn task_lines(
        &self,
        width: usize,
        height: usize,
        display: DisplayProfile,
    ) -> Vec<RoleLine> {
        self.task_frame(width, height, display, false).0
    }

    fn task_line(
        &self,
        board: &TaskBoard,
        index: usize,
        width: usize,
        display: DisplayProfile,
        selected: bool,
    ) -> RoleLine {
        let task = &board.tasks[index];
        let blocked = board.blocked_by(task);
        let (glyph, glyph_role, title_role) = match task.status {
            SharedTaskStatus::Pending => (
                Glyph::TodoPending,
                Role::Text,
                if blocked.is_empty() {
                    Role::Text
                } else {
                    Role::MutedText
                },
            ),
            SharedTaskStatus::InProgress => (
                Glyph::TodoInProgress,
                Role::InProgressTodo,
                if blocked.is_empty() {
                    Role::Strong
                } else {
                    Role::MutedStrong
                },
            ),
            SharedTaskStatus::Completed => {
                (Glyph::TodoCompleted, Role::Success, Role::CompletedTodo)
            }
            SharedTaskStatus::Cancelled => (Glyph::AgentStopped, Role::Warning, Role::MutedText),
            SharedTaskStatus::NeedsAttention => (Glyph::Warning, Role::Error, Role::Error),
        };
        let mut suffix = RoleLine::default();
        if board.is_team
            && let Some(owner) = &task.owner
        {
            let color = self.agents.iter().find_map(|row| {
                row.subagent
                    .as_ref()
                    .filter(|registration| registration.child_chat_id == owner.chat_id)
                    .and_then(|registration| registration.color)
            });
            suffix.push(" (", Some(Role::MutedText));
            suffix.push(
                format!("@{}", super::safe_line(&owner.name)),
                Some(color.map_or(Role::MutedText, Role::agent)),
            );
            suffix.push(")", Some(Role::MutedText));
        }
        if !blocked.is_empty() {
            let blockers = blocked
                .iter()
                .map(|id| {
                    board
                        .tasks
                        .iter()
                        .position(|task| task.id == *id)
                        .map_or_else(
                            || "unknown task".into(),
                            |index| {
                                format!(
                                    "#{}",
                                    board
                                        .task_number(index)
                                        .map_or_else(|| "?".into(), |number| number.to_string())
                                )
                            },
                        )
                })
                .collect::<Vec<_>>();
            suffix.push(
                format!(" ▸ blocked by {}", blockers.join(", ")),
                Some(Role::MutedText),
            );
        }
        let mut line = RoleLine::role(super::list_row::ROW_INDENT, Role::Text);
        line.push(display.glyph(glyph), Some(glyph_role));
        line.push(" ", Some(Role::Text));
        let available = width.saturating_sub(line.text().width());
        let title_columns = available
            .saturating_sub(suffix.text().width())
            .max(MIN_TASK_TITLE_COLUMNS.min(available));
        line.push(
            super::conversation_picker::ellipsize(&super::safe_line(&task.title), title_columns),
            Some(title_role),
        );
        line.spans.extend(suffix.spans);
        line.truncate(width);
        if selected {
            line = line.filled(Role::Selection);
        }
        line
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
        let counts = TaskCounts::of(board.tasks.iter());
        let mut header = RoleLine::role("  ", Role::MutedText);
        header.push(board.tasks.len().to_string(), Some(Role::MutedStrong));
        header.push(" tasks (", Some(Role::MutedText));
        let mut first = true;
        for (count, label, show_zero) in [
            (counts.completed, "done", true),
            (counts.in_progress, "in progress", false),
            (counts.pending, "open", true),
            (counts.needs_attention, "needs attention", false),
            (counts.cancelled, "cancelled", false),
        ] {
            if count == 0 && !show_zero {
                continue;
            }
            if !first {
                header.push(", ", Some(Role::MutedText));
            }
            first = false;
            header.push(count.to_string(), Some(Role::MutedStrong));
            header.push(format!(" {label}"), Some(Role::MutedText));
        }
        let hint = if self.task_focus_available == Some(false) {
            "Resize to view"
        } else if focused {
            "ctrl+t collapse"
        } else if expanded {
            "ctrl+t focus"
        } else {
            "ctrl+t expand"
        };
        header.push(format!(") · {hint}"), Some(Role::MutedText));
        header.truncate(width);
        let mut lines = vec![header];
        if !focus_preview && self.task_view.presentation == TaskPresentation::Collapsed {
            return (lines, None);
        }
        let capacity = if expanded {
            let reserved = if self.task_details {
                20
            } else {
                TASK_COMPACT_HEIGHT_OFFSET
            };
            height
                .saturating_sub(reserved)
                .clamp(1, MAX_COMPACT_TASK_ROWS)
        } else {
            height.saturating_sub(TASK_COMPACT_RESERVED_ROWS).min(
                height
                    .saturating_sub(TASK_COMPACT_HEIGHT_OFFSET)
                    .clamp(MIN_COMPACT_TASK_ROWS, MAX_COMPACT_TASK_ROWS),
            )
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
        let hidden = TaskCounts::of(
            visible_tasks
                .iter()
                .enumerate()
                .filter(|(position, _)| *position < start || *position >= start + capacity)
                .map(|(_, (_, task))| *task),
        );
        let mut selected_row = None;
        for (index, task) in visible_tasks.into_iter().skip(start).take(capacity) {
            if index == self.task_view.selected {
                selected_row = Some(lines.len());
            }
            lines.push(self.task_line(
                board,
                index,
                width,
                display,
                focused && index == self.task_view.selected,
            ));
            if index == self.task_view.selected && self.task_details {
                for text in [
                    format!(
                        "Task #{} · {} · {}",
                        board
                            .task_number(index)
                            .map_or_else(|| "?".into(), |number| number.to_string()),
                        match task.status {
                            SharedTaskStatus::Pending => "pending",
                            SharedTaskStatus::InProgress => "in progress",
                            SharedTaskStatus::Completed => "completed",
                            SharedTaskStatus::NeedsAttention => "needs attention",
                            SharedTaskStatus::Cancelled => "cancelled",
                        },
                        task.id.public()
                    ),
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
                                        .enumerate()
                                        .find(|(_, dependency)| dependency.id == *id)
                                        .map_or_else(
                                            || id.public(),
                                            |(index, dependency)| {
                                                format!(
                                                    "#{} {}",
                                                    board.task_number(index).map_or_else(
                                                        || "?".into(),
                                                        |number| number.to_string()
                                                    ),
                                                    dependency.title
                                                )
                                            },
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
                            format!("    {}", super::safe_line(&text)),
                            Role::MutedText,
                        );
                        line.truncate(width);
                        lines.push(line);
                    }
                }
            }
        }
        if board.tasks.len() > capacity {
            let summary = [
                (hidden.in_progress, "in progress"),
                (hidden.pending, "pending"),
                (hidden.completed, "completed"),
                (hidden.needs_attention, "needs attention"),
                (hidden.cancelled, "cancelled"),
            ]
            .into_iter()
            .filter(|(count, _)| *count > 0)
            .map(|(count, label)| format!("{count} {label}"))
            .collect::<Vec<_>>();
            lines.push(RoleLine::role(
                format!("  … +{}", summary.join(", ")),
                Role::MutedText,
            ));
        }
        if focused {
            lines.push(RoleLine::role(
                "  ctrl+n/p move · enter details · esc back",
                Role::MutedText,
            ));
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
            tickloop_harness::base::private_fs::unix_time_milliseconds(),
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
    let mut board = TaskBoard::new(Uuid::from_u128(1010).into(), Uuid::from_u128(1012).into());
    board.tasks = titles
        .iter()
        .enumerate()
        .map(|(index, title)| tickloop_harness::protocol::SharedTask {
            id: Uuid::from_u128(2000 + index as u128).into(),
            title: (*title).into(),
            description: "Verify the task".into(),
            dependencies: vec![],
            status: SharedTaskStatus::InProgress,
            owner: Some(tickloop_harness::protocol::TaskOwner {
                chat_id: Uuid::from_u128(1010).into(),
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
            state: tickloop_harness::protocol::LoopState::Running,
            project: String::new(),
            workspace: None,
            approvals: 0,
            questions: 0,
            agent_state: crate::agent_console::AgentState::Working,
            group: crate::agent_console::AttentionGroup::Working,
            glyph: Glyph::AgentRunning,
            preview: None,
            updated_at: None,
            run_started_at: None,
            subagent: parent.map(|_| tickloop_harness::protocol::SubagentRegistration {
                child_chat_id: Uuid::from_u128(id).into(),
                handle: title.into(),
                description: title.into(),
                created_at: tickloop_harness::base::private_fs::unix_time_milliseconds(),
                color: None,
            }),
            ended: None,
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
    fn subagent_names_and_descriptions_remain_single_line_with_whole_graphemes() {
        let root = Uuid::from_u128(1);
        let child = AgentRow {
            agent_id: Uuid::from_u128(2),
            parent_id: Some(root),
            title: String::new(),
            status: "Running".into(),
            state: tickloop_harness::protocol::LoopState::Running,
            project: String::new(),
            workspace: None,
            approvals: 0,
            questions: 0,
            agent_state: crate::agent_console::AgentState::Working,
            group: crate::agent_console::AttentionGroup::Working,
            glyph: Glyph::AgentRunning,
            preview: None,
            updated_at: None,
            run_started_at: Some(0),
            subagent: Some(tickloop_harness::protocol::SubagentRegistration {
                child_chat_id: Uuid::from_u128(2).into(),
                handle: "👩‍💻中文 mixed long name".into(),
                description: "Mixed 描述 👩‍💻 long description".repeat(10),
                created_at: 0,
                color: None,
            }),
            ended: None,
        };
        for width in 0..=60 {
            for selected in [false, true] {
                let line = super::super::subagent_panel::family_line(
                    &child,
                    selected,
                    false,
                    (width, 12, DisplayProfile::default(), 14_000),
                );
                let text = line.text();
                assert!(!text.contains('\n'));
                assert!(text.width() <= width, "{width}: {text:?}");
                if width >= 20 {
                    assert!(text.contains('…') && text.ends_with("14s"), "{text:?}");
                    assert!(text.contains("👩‍💻"), "{text:?}");
                }
            }
        }
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
            state: tickloop_harness::protocol::LoopState::Running,
            project: String::new(),
            workspace: None,
            approvals: 0,
            questions: 0,
            agent_state: crate::agent_console::AgentState::Working,
            group: AttentionGroup::Working,
            glyph: Glyph::AgentRunning,
            preview: Some("Run sleep 30".into()),
            updated_at: Some(100_000),
            run_started_at: Some(100_000),
            subagent: parent.map(|_| tickloop_harness::protocol::SubagentRegistration {
                child_chat_id: id.into(),
                handle: title.into(),
                description: "Run sleep 30".into(),
                created_at: 10_000,
                color: Some(tickloop_harness::protocol::AgentColor::Cyan),
            }),
            ended: None,
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
        rows[1].agent_state = crate::agent_console::AgentState::Idle;
        rows[1].group = AttentionGroup::Completed;
        rows[1].state = tickloop_harness::protocol::LoopState::Idle;
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
        rows[1].agent_state = crate::agent_console::AgentState::Working;
        rows[1].group = AttentionGroup::Working;
        rows[1].state = tickloop_harness::protocol::LoopState::Running;
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
        area.update(
            "Auto".into(),
            "model".into(),
            Some((0, 200_000).into()),
            None,
        );
        assert_eq!(
            area.status_line(80, "").text(),
            "⏵⏵ Auto · model · Context 0% used"
        );
    }

    #[test]
    fn context_segment_shows_rounded_zero_usage() {
        let mut area = StatusArea::default();
        area.update(
            "Auto".into(),
            "model".into(),
            Some((999, 200_000).into()),
            None,
        );
        assert_eq!(
            area.status_line(80, "").text(),
            "⏵⏵ Auto · model · Context 0% used"
        );
    }

    #[test]
    fn context_segment_shows_unknown_when_usage_or_window_is_unavailable() {
        let mut area = StatusArea::default();
        for context in [None, Some((42, 0).into())] {
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
        area.update(
            "Auto".into(),
            "model".into(),
            Some((1000, 200_000).into()),
            None,
        );
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
        area.update(
            "Auto".into(),
            "gpt-5.6".into(),
            Some((0, 200_000).into()),
            None,
        );
        assert_eq!(
            area.status_line(80, "").text(),
            "⏵⏵ Auto · gpt-5.6 · Context 0% used"
        );
        area.update(
            "Auto".into(),
            "gpt-5.6".into(),
            Some((42_000, 200_000).into()),
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
        area.update(
            "Auto".into(),
            "model".into(),
            Some((42_000, 200_000).into()),
            None,
        );
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
    fn a_nearly_full_context_switches_to_the_warning_role_with_the_same_text() {
        let mut area = StatusArea::default();
        area.update(
            "Ask".into(),
            "model".into(),
            Some(ContextGauge {
                used_tokens: 150_000,
                window_tokens: 200_000,
                is_nearly_full: true,
            }),
            None,
        );
        let line = area.status_line(80, "");
        assert_eq!(line.text(), "⏵⏵ Ask · model · Context 75% used");
        let context = line
            .spans
            .iter()
            .find(|span| span.text.contains("Context"))
            .unwrap();
        assert_eq!(context.role, Some(Role::Warning));
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
                let line = area.status_line_with_indicator(width, "", Some(&notice), None);
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
    fn indicator_keeps_goal_and_model_before_optional_mode_and_context() {
        let mut area = StatusArea::default();
        area.update(
            "Auto".into(),
            "fixture".into(),
            Some((21000, 100000).into()),
            None,
        );
        for (width, expected) in [
            (
                100,
                "Pursuing goal (1m) · ⏵⏵ Auto · fixture · Context 21% used",
            ),
            (30, "Pursuing goal (1m) · fixture"),
        ] {
            let line = area.status_line_with_indicator(
                width,
                "",
                None,
                Some(RoleLine::role("Pursuing goal (1m)", Role::Accent)),
            );
            assert_eq!(line.text(), expected);
            for (text, role) in [
                ("Pursuing goal", Role::Accent),
                ("fixture", Role::Text),
                ("Context 21% used", Role::Success),
            ] {
                if line.text().contains(text) {
                    assert!(
                        line.spans
                            .iter()
                            .any(|span| span.text.contains(text) && span.role == Some(role)),
                        "{line:?}"
                    );
                }
            }
        }
        for width in [1, 8, 20, 24, 28] {
            let line = area.status_line_with_indicator(
                width,
                "",
                None,
                Some(RoleLine::role("Pursuing goal (12.5K / 50K)", Role::Accent)),
            );
            assert!(line.text().starts_with('P'));
            assert!(UnicodeWidthStr::width(line.text().as_str()) <= width);
            if width >= 20 {
                assert!(line.text().ends_with("fixture"), "{line:?}");
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
        use crate::agent_console::AgentState;
        use tickloop_harness::protocol::{AgentColor, SubagentRegistration};
        let root = Uuid::from_u128(1);
        let row = |id: u128, state: LoopState, agent_state: AgentState| AgentRow {
            agent_id: Uuid::from_u128(id),
            parent_id: (id != 1).then_some(root),
            title: format!("member-{id}"),
            status: format!("{state:?}"),
            state,
            project: String::new(),
            workspace: None,
            approvals: 0,
            questions: 0,
            agent_state,
            group: agent_state.group(),
            glyph: super::super::console::status_glyph(agent_state, false),
            preview: None,
            updated_at: Some(64_000),
            run_started_at: None,
            subagent: (id != 1).then(|| SubagentRegistration {
                child_chat_id: Uuid::from_u128(id).into(),
                handle: format!("member-{id}"),
                description: "work".into(),
                created_at: 1_000,
                color: Some(AgentColor::ALL[id as usize % 8]),
            }),
            ended: None,
        };
        let mut area = StatusArea::default();
        area.update("Auto".into(), "model".into(), None, None);
        area.update_agents(
            vec![
                row(1, LoopState::Idle, AgentState::Idle),
                row(2, LoopState::Idle, AgentState::Idle),
                row(3, LoopState::Failed, AgentState::Failed),
                row(4, LoopState::Interrupted, AgentState::Completed),
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
        for (index, line) in lines.iter().enumerate().take(4).skip(1) {
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
            let expected = ["idle", "Failed", "Interrupted"][index - 1];
            assert!(line.text().ends_with(expected), "{line:?}");
            let expected_role = [Role::MutedText, Role::Error, Role::Warning][index - 1];
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
    fn working_task_block_has_one_connector_and_reserves_its_columns() {
        let mut area = StatusArea::default();
        area.update(
            "Auto".into(),
            "model".into(),
            None,
            Some(test_task_board(&["Inspect", "Verify"])),
        );
        let display = DisplayProfile::default();
        let rows = area.task_lines_in_viewport(80, 24, 20, display, true);
        assert!(rows[0].text().starts_with("  ⎿  2 tasks"));
        assert_eq!(
            rows.iter()
                .map(|line| line.text().matches('⎿').count())
                .sum::<usize>(),
            1
        );
        assert_eq!(rows[1].text(), "     ◼ Inspect");
        assert_eq!(rows[2].text(), "     ◼ Verify");
        assert!(area.task_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL)));
        let focused = area.task_lines_in_viewport(80, 24, 20, display, true);
        let selected = focused
            .iter()
            .find(|line| line.fill == Some(Role::Selection))
            .unwrap();
        assert_eq!(selected.fill_start, 3);
        for width in [0, 1, 3, 6, 8, 24] {
            let rows = area.task_lines_in_viewport(width, 24, 20, display, true);
            assert!(rows.iter().all(|row| row.text().width() <= width));
            if width <= 6 {
                assert!(!area.tasks_can_take_focus());
            }
        }
        let idle = area.task_lines_in_viewport(80, 24, 20, display, false);
        assert!(idle[0].text().starts_with("  2 tasks"));
        assert!(idle.iter().all(|row| !row.text().contains('⎿')));
        area.update("Auto".into(), "model".into(), None, None);
        assert!(
            area.task_lines_in_viewport(80, 24, 20, display, true)
                .is_empty()
        );
    }

    #[test]
    fn task_list_uses_inline_owners_and_accurate_overflow_counts() {
        let mut area = StatusArea::default();
        let mut board = test_task_board(&[
            "Finished",
            "Active",
            "Ready",
            "Blocked",
            "Later",
            "Needs check",
            "Cancelled",
            "More",
        ]);
        board.is_team = true;
        for task in &mut board.tasks {
            task.owner = None;
            task.status = SharedTaskStatus::Pending;
        }
        board.tasks[0].status = SharedTaskStatus::Completed;
        board.tasks[1].status = SharedTaskStatus::InProgress;
        board.tasks[5].status = SharedTaskStatus::NeedsAttention;
        board.tasks[6].status = SharedTaskStatus::Cancelled;
        board.tasks[3].dependencies = vec![board.tasks[2].id];
        let owner_chat = Uuid::from_u128(9000);
        for index in [0, 1] {
            board.tasks[index].owner = Some(tickloop_harness::protocol::TaskOwner {
                chat_id: owner_chat.into(),
                name: "reader".into(),
            });
        }
        area.update_agents(
            vec![AgentRow {
                agent_id: Uuid::from_u128(9001),
                parent_id: Some(Uuid::from_u128(9002)),
                title: "Read notes".into(),
                status: "Idle".into(),
                state: LoopState::Idle,
                project: String::new(),
                workspace: None,
                approvals: 0,
                questions: 0,
                agent_state: crate::agent_console::AgentState::Idle,
                group: crate::agent_console::AttentionGroup::Completed,
                glyph: Glyph::AgentIdle,
                preview: None,
                updated_at: None,
                run_started_at: None,
                subagent: Some(tickloop_harness::protocol::SubagentRegistration {
                    child_chat_id: owner_chat.into(),
                    handle: "reader".into(),
                    description: "Read notes".into(),
                    created_at: 0,
                    color: Some(tickloop_harness::protocol::AgentColor::Blue),
                }),
                ended: None,
            }],
            None,
        );
        area.update("Auto".into(), "model".into(), None, Some(board));
        let lines = area.task_lines(180, 24, DisplayProfile::default());
        assert!(
            lines[0].text().starts_with(
                "  8 tasks (1 done, 1 in progress, 4 open, 1 needs attention, 1 cancelled)"
            ),
            "{lines:?}"
        );
        assert_eq!(lines.len(), 7);
        assert_eq!(
            lines.last().unwrap().text(),
            "  … +1 pending, 1 completed, 1 cancelled"
        );
        let active = lines
            .iter()
            .find(|line| line.text().contains("Active"))
            .unwrap();
        assert_eq!(active.text(), "  ◼ Active (@reader)");
        assert!(
            active
                .spans
                .iter()
                .any(|span| span.text == "@reader" && span.role == Some(Role::AgentBlue))
        );
        assert!(
            lines
                .iter()
                .any(|line| line.text() == "  ◻ Blocked ▸ blocked by #3")
        );
        assert!(lines.iter().any(|line| line.text() == "  ! Needs check"));
        area.task_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        let expanded = area.task_lines(180, 24, DisplayProfile::default());
        assert_eq!(expanded[1].text(), "  ✔ Finished (@reader)");
        assert!(
            expanded[1]
                .spans
                .iter()
                .any(|span| span.text == "Finished" && span.role == Some(Role::CompletedTodo))
        );
        for _ in 0..6 {
            area.task_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        }
        let cancelled = area.task_lines(180, 24, DisplayProfile::default());
        assert!(cancelled.iter().any(|line| line.text() == "  ✖ Cancelled"));
        area.task_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let details = area.task_lines(180, 40, DisplayProfile::default());
        assert!(
            details
                .iter()
                .any(|line| line.text().contains("Task #7 · cancelled"))
        );
        for width in [1, 8, 20, 60] {
            let rows = area.task_lines(width, 24, DisplayProfile::default());
            assert!(rows.iter().all(|row| row.text().width() <= width));
        }
    }

    #[test]
    fn task_dependencies_use_stable_display_numbers_and_clear_when_completed() {
        let mut board = test_task_board(&["First", "Second", "Third", "Dependent"]);
        for task in &mut board.tasks {
            task.status = SharedTaskStatus::Pending;
            task.owner = None;
        }
        board.tasks[1].status = SharedTaskStatus::Completed;
        board.tasks[3].dependencies = vec![board.tasks[0].id, board.tasks[1].id, board.tasks[2].id];
        let dependent_id = board.tasks[3].id;
        let mut area = StatusArea::default();
        assert_eq!(
            area.task_line(&board, 3, 100, DisplayProfile::default(), false)
                .text(),
            "  ◻ Dependent ▸ blocked by #1, #3"
        );
        let mut added = board.tasks[0].clone();
        added.id = Uuid::new_v4().into();
        added.title = "Added later".into();
        board.tasks.push(added);
        board.tasks[0].status = SharedTaskStatus::Completed;
        assert_eq!(
            area.task_line(&board, 3, 100, DisplayProfile::default(), false)
                .text(),
            "  ◻ Dependent ▸ blocked by #3"
        );
        for width in [1, 8, 20, 30] {
            assert!(
                area.task_line(&board, 3, width, DisplayProfile::default(), false)
                    .text()
                    .width()
                    <= width
            );
        }
        board.tasks[2].status = SharedTaskStatus::Completed;
        assert_eq!(
            area.task_line(&board, 3, 100, DisplayProfile::default(), false)
                .text(),
            "  ◻ Dependent"
        );
        area.update("Auto".into(), "model".into(), None, Some(board));
        area.task_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        for _ in 0..3 {
            area.task_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        }
        area.task_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let lines = area.task_lines(160, 40, DisplayProfile::default());
        assert!(lines.iter().any(|line| {
            line.text()
                .contains(&format!("Task #4 · pending · {}", dependent_id.public()))
        }));
        assert!(lines.iter().any(|line| {
            line.text()
                .contains("Dependencies: #1 First, #2 Second, #3 Third")
        }));
    }

    #[test]
    fn task_numbers_continue_after_a_completed_list_is_cleared() {
        let mut board = test_task_board(&["Prerequisite", "Dependent"]);
        board.task_number_offset = 11;
        for task in &mut board.tasks {
            task.status = SharedTaskStatus::Pending;
            task.owner = None;
        }
        board.tasks[1].dependencies = vec![board.tasks[0].id];
        let mut area = StatusArea::default();
        assert_eq!(
            area.task_line(&board, 1, 100, DisplayProfile::default(), false)
                .text(),
            "  ◻ Dependent ▸ blocked by #12"
        );
        area.update("Auto".into(), "model".into(), None, Some(board));
        area.task_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        area.task_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        area.task_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let lines = area.task_lines(160, 40, DisplayProfile::default());
        assert!(
            lines
                .iter()
                .any(|line| line.text().contains("Task #13 · pending"))
        );
        assert!(
            lines
                .iter()
                .any(|line| line.text().contains("Dependencies: #12 Prerequisite"))
        );
    }

    #[test]
    fn task_list_omits_zero_running_count_and_keeps_a_long_title_on_one_line() {
        let mut area = StatusArea::default();
        let mut board =
            test_task_board(&["A long task title that cannot fit on a narrow terminal"]);
        board.tasks[0].status = SharedTaskStatus::Pending;
        board.tasks[0].owner = None;
        area.update("Auto".into(), "model".into(), None, Some(board));
        let lines = area.task_lines(24, 24, DisplayProfile::default());
        assert_eq!(lines.len(), 2);
        assert!(lines[1].text().ends_with('…'));
        assert!(!lines[1].text().contains("pending"));
        let wide = area.task_lines(100, 24, DisplayProfile::default());
        assert!(wide[0].text().starts_with("  1 tasks (0 done, 1 open)"));
        assert!(!wide[0].text().contains("in progress"));
    }

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
        assert!(compact[1].text().contains("Verify"));
        assert!(compact[2].text().contains("Inspect"));
        assert!(
            compact[0]
                .text()
                .contains("2 tasks (1 done, 1 in progress, 0 open)")
        );
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
                .any(|line| line.text().contains("Dependencies: #1 Inspect"))
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
        assert_eq!(compact.len(), 5);
        assert!(compact[1].text().contains("Inspect"));
        assert!(compact[1].text().contains("◼"));
        assert!(!compact[1].text().contains("in progress"));
        assert!(compact[2].text().contains("blocked by #1"));
        assert!(compact.iter().all(|line| !line.text().contains("main")));
        assert!(compact.iter().any(|line| line.text().contains("Later")));
        assert_eq!(area.task_lines(80, 13, DisplayProfile::default()).len(), 3);
        assert_eq!(area.task_lines(80, 10, DisplayProfile::default()).len(), 1);
        area.task_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        area.task_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
        area.task_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            area.task_lines(80, 24, DisplayProfile::default())
                .iter()
                .any(|line| line.text().contains("Dependencies: #1 Inspect"))
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
    fn unassigned_team_tasks_show_no_owner_or_repeated_status() {
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
        assert_eq!(unassigned, "  ◻ Inspect");
        assert!(
            !unassigned.contains("unassigned") && !unassigned.contains('·'),
            "{unassigned}"
        );
        let assigned = lines
            .iter()
            .find(|line| line.text().contains("Verify"))
            .unwrap()
            .text();
        assert_eq!(assigned, "  ◻ Verify (@main)");
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
        other.root_loop_id = Uuid::from_u128(9999).into();
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
        let compact = area.task_lines(100, 17, DisplayProfile::default());
        assert!(compact[1].text().contains("Attention"));
        assert!(compact[2].text().contains("Running"));
        assert!(compact[3].text().contains("Pending"));
        area.task_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        let expanded = area.task_lines(80, 40, DisplayProfile::default());
        assert!(expanded[1].text().contains("Finished"));
        assert!(expanded[2].text().contains("Cancelled"));
    }

    #[test]
    fn task_focus_requires_room_for_the_selected_single_row_and_hints() {
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
        area.task_lines_in_viewport(24, 24, 2, DisplayProfile::default(), false);
        assert!(!area.tasks_can_take_focus());
        assert!(!area.task_key(toggle));
        area.task_lines_in_viewport(24, 24, 3, DisplayProfile::default(), false);
        assert!(area.tasks_can_take_focus());
        assert!(area.task_key(toggle));
        area.task_lines_in_viewport(24, 24, 2, DisplayProfile::default(), false);
        assert!(!area.task_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)));
        assert!(area.task_view.presentation == TaskPresentation::Expanded);
    }
}
