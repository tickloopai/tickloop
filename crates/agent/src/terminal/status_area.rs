use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use tickloop_agent::runtime::{TodoItem, TodoStatus};
use unicode_width::UnicodeWidthStr;
use uuid::Uuid;

use super::{
    console::{AgentTreeRow, MIN_REQUEST_AGENT_TITLE_COLUMNS, RowExtras, truncate_width},
    display::{DisplayProfile, Glyph},
    footer::{FooterFrame, FooterItemId},
    style::{Role, RoleLine},
};

const MAX_PANEL_ITEMS: usize = 8;
const TODO_CONTINUATION_INDENT: &str = "     ";
const TODO_ROW_PREFIX_COLUMNS: usize = 7;
const TODO_STATUS_MARK_COLUMNS: usize = 2;
const TODO_SUBJECT_MARGIN_COLUMNS: usize = 15;
const TODO_LAYOUT_RESERVED_ROWS: usize = 14;
const TODO_COMPACT_TERMINAL_ROWS: usize = 10;
const MIN_TODO_DISPLAY_ITEMS: usize = 3;
const MAX_TODO_DISPLAY_ITEMS: usize = 10;
const RECENT_TODO_COMPLETION_TTL: Duration = Duration::from_secs(30);
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

#[derive(Default)]
pub(super) struct StatusArea {
    mode: String,
    model: String,
    context: Option<(u64, usize)>,
    todos: Vec<TodoItem>,
    todo_completed_at: HashMap<(usize, String), Instant>,
    agents: Vec<AgentTreeRow>,
    /// Reading numbers shown after repeated family titles.
    agent_ordinals: HashMap<Uuid, usize>,
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

    pub(super) fn todo_refresh_pending(&self) -> bool {
        !self.todo_completed_at.is_empty()
    }

    pub(super) fn update(
        &mut self,
        mode: String,
        model: String,
        context: Option<(u64, usize)>,
        todos: Vec<TodoItem>,
    ) {
        self.mode = mode;
        self.model = model;
        self.context = context.filter(|(_, limit)| *limit > 0);
        let now = Instant::now();
        if !self.todos.is_empty() {
            for (index, todo) in todos
                .iter()
                .enumerate()
                .filter(|(_, todo)| todo.status == TodoStatus::Completed)
            {
                let was_completed = self.todos.get(index).is_some_and(|previous| {
                    previous.content == todo.content && previous.status == TodoStatus::Completed
                });
                if !was_completed {
                    self.todo_completed_at
                        .insert((index, todo.content.clone()), now);
                }
            }
        }
        self.todo_completed_at.retain(|(index, content), _| {
            todos.get(*index).is_some_and(|todo| {
                &todo.content == content && todo.status == TodoStatus::Completed
            })
        });
        self.todos = todos;

        self.reconcile_panel();
        self.clamp_offset();
    }

    pub(super) fn available_panels(&self) -> impl Iterator<Item = FooterItemId> + '_ {
        [FooterItemId::Agents]
            .into_iter()
            .filter(|_| self.family_row_count() > 0)
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

    pub(super) fn selected_panel(&self) -> Option<FooterItemId> {
        self.visible
    }

    pub(super) fn scroll(&mut self, amount: isize, capacity: usize) {
        let count = self.visible.map_or(0, |_| self.family_row_count());
        let max_offset = count.saturating_sub(capacity);
        self.offset = self.offset.saturating_add_signed(amount).min(max_offset);
    }

    pub(super) fn row_count(&self, height: usize) -> usize {
        if self.todos.is_empty() && self.agents.is_empty() {
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

    pub(super) fn todo_lines(
        &mut self,
        width: usize,
        height: usize,
        display: DisplayProfile,
        has_activity: bool,
        now: Instant,
    ) -> Vec<RoleLine> {
        self.todo_completed_at.retain(|_, completed| {
            now.saturating_duration_since(*completed) < RECENT_TODO_COMPLETION_TTL
        });
        if self.todos.is_empty() {
            return Vec::new();
        }
        let capacity = if height <= TODO_COMPACT_TERMINAL_ROWS {
            1
        } else {
            height
                .saturating_sub(TODO_LAYOUT_RESERVED_ROWS)
                .clamp(MIN_TODO_DISPLAY_ITEMS, MAX_TODO_DISPLAY_ITEMS)
        };
        let mut todos = self.todos.iter().enumerate().collect::<Vec<_>>();
        if todos.len() > capacity {
            todos.sort_by_key(|(index, todo)| {
                let rank = if self
                    .todo_completed_at
                    .contains_key(&(*index, todo.content.clone()))
                {
                    0
                } else {
                    match todo.status {
                        TodoStatus::InProgress => 1,
                        TodoStatus::Pending => 2,
                        TodoStatus::Completed => 3,
                    }
                };
                (rank, *index)
            });
        }
        let completed = self
            .todos
            .iter()
            .filter(|todo| todo.status == TodoStatus::Completed)
            .count();
        let pending = self
            .todos
            .iter()
            .filter(|todo| todo.status == TodoStatus::Pending)
            .count();
        let in_progress = self.todos.len() - completed - pending;
        let mut lines = Vec::new();
        if !has_activity {
            let mut header = RoleLine::role("  ", Role::MutedText);
            header.push(self.todos.len().to_string(), Some(Role::Strong));
            header.push(" tasks (", Some(Role::MutedText));
            header.push(completed.to_string(), Some(Role::Strong));
            header.push(" done, ", Some(Role::MutedText));
            if in_progress > 0 {
                header.push(in_progress.to_string(), Some(Role::Strong));
                header.push(" in progress, ", Some(Role::MutedText));
            }
            header.push(pending.to_string(), Some(Role::Strong));
            header.push(" open)", Some(Role::MutedText));
            header.truncate(width);
            lines.push(header);
        }
        let has_tree_indent = width > TODO_ROW_PREFIX_COLUMNS;
        let prefix_columns = if has_tree_indent {
            TODO_ROW_PREFIX_COLUMNS
        } else {
            TODO_STATUS_MARK_COLUMNS
        };
        let subject_columns = width
            .saturating_sub(TODO_SUBJECT_MARGIN_COLUMNS)
            .max(TODO_SUBJECT_MARGIN_COLUMNS)
            .min(width.saturating_sub(prefix_columns));
        for (index, (_, todo)) in todos.iter().take(capacity).enumerate() {
            let (glyph, icon_role, text_role) = match todo.status {
                TodoStatus::Completed => {
                    (Glyph::TodoCompleted, Role::MutedText, Role::CompletedTodo)
                }
                TodoStatus::InProgress => (
                    Glyph::TodoInProgress,
                    Role::InProgressTodo,
                    Role::InProgressTodo,
                ),
                TodoStatus::Pending => (Glyph::TodoPending, Role::Text, Role::Text),
            };
            let mut title = super::safe_text(&todo.content).replace(['\n', '\t'], " ");
            if UnicodeWidthStr::width(title.as_str()) > subject_columns {
                title = format!(
                    "{}…",
                    truncate_width(&title, subject_columns.saturating_sub(1))
                );
            }
            let prefix = if !has_tree_indent {
                String::new()
            } else if index == 0 {
                format!("  {}  ", display.glyph(Glyph::TodoBranch))
            } else {
                TODO_CONTINUATION_INDENT.to_owned()
            };
            let mut line = RoleLine::role(prefix, Role::MutedText);
            line.push(format!("{} ", display.glyph(glyph)), Some(icon_role));
            line.push(title, Some(text_role));
            line.truncate(width);
            lines.push(line);
        }
        let hidden = &todos[capacity.min(todos.len())..];
        if !hidden.is_empty() {
            let parts = [
                (TodoStatus::InProgress, "in progress"),
                (TodoStatus::Pending, "pending"),
                (TodoStatus::Completed, "completed"),
            ]
            .into_iter()
            .filter_map(|(status, label)| {
                let count = hidden
                    .iter()
                    .filter(|(_, todo)| todo.status == status)
                    .count();
                (count > 0).then(|| format!("{count} {label}"))
            })
            .collect::<Vec<_>>();
            let indent = if has_tree_indent {
                TODO_CONTINUATION_INDENT
            } else {
                ""
            };
            let mut summary =
                RoleLine::role(format!("{indent}… +{}", parts.join(", ")), Role::MutedText);
            summary.truncate(width);
            lines.push(summary);
        }
        lines
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
        let render_header = !footer.items.is_empty();
        header.truncate(width);
        let mut lines = if render_header {
            vec![header.filled(Role::StatusPanel)]
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
        let status = |row: &AgentTreeRow| {
            let mut status = vec![(row.status.clone(), row.status_role())];
            if Some(row.agent_id) == self.visible_agent {
                status.push((" · ".into(), Role::MutedText));
                status.push(("viewing".into(), Role::Text));
            }
            row.own_status_variants(status, width + 1)
        };
        for row in agents.iter().skip(offset).take(capacity) {
            let selected = footer.list_focused
                && footer.selected == Some(FooterItemId::Agents)
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
        lines
    }

    fn family_row_count(&self) -> usize {
        if self.agents.iter().any(|row| row.parent_id.is_some()) {
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
                    .filter(|row| row.parent_id.is_some())
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
mod tests {
    use super::*;

    fn todo(content: &str) -> TodoItem {
        TodoItem {
            content: content.into(),
            status: TodoStatus::InProgress,
            active_form: "Working".into(),
        }
    }

    #[test]
    fn todo_rows_use_each_glyph_set_with_a_first_row_branch_and_aligned_continuations() {
        use tickloop_agent::settings::{DisplayPreferences, GlyphSet};

        for (glyph_set, expected) in [
            (
                GlyphSet::Unicode,
                ["  ⎿  ◻ pending", "     ◼ active", "     ✔ done"],
            ),
            (
                GlyphSet::NerdFont,
                [
                    "  \u{f060d}  \u{f0131} pending",
                    "     \u{f012e} active",
                    "     \u{f00c} done",
                ],
            ),
        ] {
            let mut area = StatusArea::default();
            let mut todos = vec![todo("pending"), todo("active"), todo("done")];
            for (todo, status) in todos.iter_mut().zip([
                TodoStatus::Pending,
                TodoStatus::InProgress,
                TodoStatus::Completed,
            ]) {
                todo.status = status;
            }
            area.update("Auto".into(), "fixture".into(), None, todos);
            let display = DisplayProfile::new(DisplayPreferences {
                glyph_set,
                prefers_reduced_motion: false,
            });
            let rows = area.todo_lines(80, 24, display, true, Instant::now());
            assert_eq!(
                rows.iter().map(RoleLine::text).collect::<Vec<_>>(),
                expected
            );
            for row in &rows {
                assert_eq!(
                    UnicodeWidthStr::width(row.text().as_str()),
                    7 + row.text().chars().skip(7).count()
                );
            }
        }
    }

    #[test]
    fn todo_status_colours_and_decorations_follow_the_active_theme_in_both_glyph_sets() {
        use super::super::style::{ColourDepth, ThemeName};
        use tickloop_agent::settings::{DisplayPreferences, GlyphSet};

        for glyph_set in [GlyphSet::Unicode, GlyphSet::NerdFont] {
            let mut area = StatusArea::default();
            let mut todos = vec![todo("pending"), todo("active"), todo("done")];
            for (todo, status) in todos.iter_mut().zip([
                TodoStatus::Pending,
                TodoStatus::InProgress,
                TodoStatus::Completed,
            ]) {
                todo.status = status;
            }
            area.update("Auto".into(), "fixture".into(), None, todos);
            let mut display = DisplayProfile::new(DisplayPreferences {
                glyph_set,
                prefers_reduced_motion: false,
            });
            let mut themed_rows = Vec::new();
            for name in ThemeName::ALL {
                display.set_theme(name);
                let theme = display.theme();
                let rows = area.todo_lines(80, 24, display, true, Instant::now());
                let mut rendered_rows = Vec::new();
                for (row, (title, glyph, colour, bold, crossed_out)) in rows.iter().zip([
                    ("pending", Glyph::TodoPending, Role::Text, false, false),
                    ("active", Glyph::TodoInProgress, Role::Accent, true, false),
                    ("done", Glyph::TodoCompleted, Role::MutedText, false, true),
                ]) {
                    let resolved = row.resolve(theme, 80);
                    let icon = resolved
                        .spans
                        .iter()
                        .find(|span| span.text.contains(display.glyph(glyph)))
                        .unwrap();
                    let text = resolved
                        .spans
                        .iter()
                        .find(|span| span.text.contains(title))
                        .unwrap();
                    assert_eq!(
                        icon.style.foreground,
                        theme.style(colour).foreground,
                        "{glyph_set:?} {name:?} {title} icon"
                    );
                    assert_eq!(
                        text.style.foreground,
                        theme.style(colour).foreground,
                        "{glyph_set:?} {name:?} {title} text"
                    );
                    assert_eq!(icon.style.bold, bold);
                    assert!(!icon.style.crossed_out);
                    assert_eq!(text.style.bold, bold);
                    assert_eq!(text.style.crossed_out, crossed_out);
                    assert!(!icon.style.dim && !text.style.dim);
                    rendered_rows.push(resolved.render(ColourDepth::TrueColour));
                }
                let branch = rows[0].resolve(theme, 80);
                assert_eq!(
                    branch.spans[0].style.foreground,
                    theme.style(Role::MutedText).foreground
                );
                themed_rows.push(rendered_rows);
            }
            assert_ne!(
                themed_rows[0][0], themed_rows[1][0],
                "pending row must recolour when switching themes"
            );
            assert_ne!(
                themed_rows[0][2], themed_rows[1][2],
                "completed row must recolour when switching themes"
            );
        }
    }

    #[test]
    fn narrow_todo_rows_keep_status_marks_and_truncate_subjects_by_display_columns() {
        use tickloop_agent::settings::{DisplayPreferences, GlyphSet};

        for glyph_set in [GlyphSet::Unicode, GlyphSet::NerdFont] {
            let mut area = StatusArea::default();
            let mut todos = vec![
                todo("中文ab中ab"),
                todo("e\u{301}中ab"),
                todo("done\nne\txt\r\u{1b}"),
            ];
            for (todo, status) in todos.iter_mut().zip([
                TodoStatus::Pending,
                TodoStatus::InProgress,
                TodoStatus::Completed,
            ]) {
                todo.status = status;
            }
            area.update("Auto".into(), "fixture".into(), None, todos);
            let display = DisplayProfile::new(DisplayPreferences {
                glyph_set,
                prefers_reduced_motion: false,
            });
            for width in 0..=40 {
                let rows = area.todo_lines(width, 24, display, true, Instant::now());
                assert_eq!(rows.len(), 3);
                for row in &rows {
                    assert!(
                        UnicodeWidthStr::width(row.text().as_str()) <= width,
                        "{glyph_set:?} width={width}: {row:?}"
                    );
                    assert!(!row.text().contains(['\n', '\t', '\r', '\u{1b}']));
                }
                if width > 0 {
                    for (row, glyph) in rows.iter().zip([
                        Glyph::TodoPending,
                        Glyph::TodoInProgress,
                        Glyph::TodoCompleted,
                    ]) {
                        assert!(
                            row.text().contains(display.glyph(glyph)),
                            "{glyph_set:?} width={width}: status mark must survive"
                        );
                    }
                }
            }
            let rows = area.todo_lines(12, 24, display, true, Instant::now());
            assert_eq!(
                rows[0].text(),
                format!(
                    "  {}  {} 中文…",
                    display.glyph(Glyph::TodoBranch),
                    display.glyph(Glyph::TodoPending)
                )
            );
            assert_eq!(
                rows[1].text(),
                format!("     {} e\u{301}中ab", display.glyph(Glyph::TodoInProgress))
            );
            assert_eq!(
                rows[2].text(),
                format!("     {} done…", display.glyph(Glyph::TodoCompleted))
            );
            let short = area.todo_lines(40, 10, display, true, Instant::now());
            assert!(
                short[0]
                    .text()
                    .contains(display.glyph(Glyph::TodoInProgress))
            );
            assert_eq!(short[1].text(), "     … +1 pending, 1 completed");
        }
    }

    #[test]
    fn todo_truncation_prioritizes_live_work_and_accounts_for_hidden_statuses() {
        let mut area = StatusArea::default();
        let todos = (1..=13)
            .map(|index| {
                let mut todo = todo(&format!("todo {index}"));
                todo.status = if index == 1 {
                    TodoStatus::Completed
                } else if index == 13 {
                    TodoStatus::InProgress
                } else {
                    TodoStatus::Pending
                };
                todo
            })
            .collect();
        area.update("Auto".into(), "fixture".into(), None, todos);
        let rows = area.todo_lines(80, 24, DisplayProfile::default(), true, Instant::now());
        assert!(
            rows.iter().any(|row| row.text().contains("todo 13")),
            "{rows:?}"
        );
        assert!(
            !rows.iter().any(|row| row.text().ends_with("todo 1")),
            "{rows:?}"
        );
        assert_eq!(
            rows.last().unwrap().text().trim(),
            "… +2 pending, 1 completed"
        );
    }

    #[test]
    fn todo_order_and_recent_completion_expiry_match_the_reference() {
        let mut area = StatusArea::default();
        let initial = (1..=13)
            .map(|index| {
                let mut todo = todo(&format!("todo {index}"));
                todo.status = if index == 13 {
                    TodoStatus::InProgress
                } else {
                    TodoStatus::Pending
                };
                todo
            })
            .collect::<Vec<_>>();
        area.update("Auto".into(), "fixture".into(), None, initial.clone());
        let mut completed = initial;
        completed[0].status = TodoStatus::Completed;
        area.update("Auto".into(), "fixture".into(), None, completed);
        let now = Instant::now();
        assert!(area.todo_refresh_pending());
        let recent = area.todo_lines(80, 24, DisplayProfile::default(), true, now);
        assert_eq!(recent[0].text(), "  ⎿  ✔ todo 1");
        assert_eq!(recent[1].text(), "     ◼ todo 13");
        assert_eq!(recent.last().unwrap().text().trim(), "… +3 pending");
        let expired = area.todo_lines(
            80,
            24,
            DisplayProfile::default(),
            true,
            now + Duration::from_secs(31),
        );
        assert_eq!(expired[0].text(), "  ⎿  ◼ todo 13");
        assert_eq!(
            expired.last().unwrap().text().trim(),
            "… +2 pending, 1 completed"
        );
        assert!(!area.todo_refresh_pending());
        let idle = area.todo_lines(
            32,
            24,
            DisplayProfile::default(),
            false,
            now + Duration::from_secs(31),
        );
        assert!(
            idle.iter()
                .all(|row| UnicodeWidthStr::width(row.text().as_str()) <= 32)
        );
        area.update(
            "Auto".into(),
            "fixture".into(),
            None,
            vec![todo("third"), todo("first"), todo("second")],
        );
        let ordered = area.todo_lines(80, 24, DisplayProfile::default(), true, now);
        assert!(ordered[0].text().ends_with("third"));
        assert!(ordered[1].text().ends_with("first"));
        assert!(ordered[2].text().ends_with("second"));
        area.update("Auto".into(), "fixture".into(), None, vec![]);
        assert!(
            area.todo_lines(80, 24, DisplayProfile::default(), false, now)
                .is_empty()
        );
    }

    #[test]
    fn context_segment_shows_zero_before_usage_with_a_known_window() {
        let mut area = StatusArea::default();
        area.update("Auto".into(), "model".into(), Some((0, 200_000)), vec![]);
        assert_eq!(
            area.status_line(80, "").text(),
            "⏵⏵ Auto · model · Context 0% used"
        );
    }

    #[test]
    fn context_segment_shows_rounded_zero_usage() {
        let mut area = StatusArea::default();
        area.update("Auto".into(), "model".into(), Some((999, 200_000)), vec![]);
        assert_eq!(
            area.status_line(80, "").text(),
            "⏵⏵ Auto · model · Context 0% used"
        );
    }

    #[test]
    fn context_segment_shows_unknown_when_usage_or_window_is_unavailable() {
        let mut area = StatusArea::default();
        for context in [None, Some((42, 0))] {
            area.update("Auto".into(), "model".into(), context, vec![]);
            assert_eq!(
                area.status_line(80, "").text(),
                "⏵⏵ Auto · model · Context unknown"
            );
        }
    }

    #[test]
    fn context_segment_shows_rounded_usage_and_keeps_the_existing_fit_priority() {
        let mut area = StatusArea::default();
        area.update("Auto".into(), "model".into(), Some((1000, 200_000)), vec![]);
        assert_eq!(
            area.status_line(80, "").text(),
            "⏵⏵ Auto · model · Context 1% used"
        );
        assert_eq!(area.status_line(16, "").text(), "⏵⏵ Auto · model");
        area.update("Auto".into(), "model".into(), None, vec![]);
        assert_eq!(
            area.status_line(80, "Not saved").text(),
            "⏵⏵ Auto · model · Context unknown · Not saved"
        );
    }

    #[test]
    fn status_keeps_mode_and_notice_when_narrow_and_never_invents_usage() {
        let mut area = StatusArea::default();
        area.update("Auto".into(), "gpt-5.6".into(), None, vec![]);
        assert_eq!(
            area.status_line(80, "").text(),
            "⏵⏵ Auto · gpt-5.6 · Context unknown"
        );
        let narrow = area.status_line(24, "Not saved").text();
        assert!(narrow.starts_with("⏵⏵ Auto · Not saved"));
        assert!(!narrow.contains("gpt-5.6"));
        area.update("Auto".into(), "gpt-5.6".into(), Some((0, 200_000)), vec![]);
        assert_eq!(
            area.status_line(80, "").text(),
            "⏵⏵ Auto · gpt-5.6 · Context 0% used"
        );
        area.update(
            "Auto".into(),
            "gpt-5.6".into(),
            Some((42_000, 200_000)),
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
        area.update("Auto".into(), "model".into(), None, vec![]);
        assert_eq!(context_role(&area), Some(Role::MutedText));
        assert_eq!(area.status_line(80, "").spans[0].role, Some(Role::Warning));
        area.update(
            "Auto".into(),
            "model".into(),
            Some((42_000, 200_000)),
            vec![],
        );
        assert_eq!(context_role(&area), Some(Role::Success));
        assert!(
            area.status_line(80, "")
                .spans
                .iter()
                .all(|span| span.role != Some(Role::Error))
        );
        area.update("Plan".into(), "model".into(), None, vec![]);
        assert_eq!(area.status_line(80, "").spans[0].role, Some(Role::PlanMode));
    }

    #[test]
    fn background_waiting_facts_and_agent_labels_use_separate_semantic_roles() {
        let mut area = StatusArea::default();
        area.update("Auto".into(), "model".into(), None, vec![]);
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
        area.update("Ask".into(), "model".into(), None, vec![]);
        assert_eq!(
            area.status_line(80, "").spans[0].role,
            Some(Role::AutoAccept)
        );

        area.update("Full Access".into(), "model".into(), None, vec![]);
        assert_eq!(area.status_line(80, "").spans[0].role, Some(Role::Error));
    }
}
