use std::{collections::HashMap, ops::Range};

use unicode_width::UnicodeWidthChar;
use uuid::Uuid;

pub use crate::runtime::{
    ShellTask as ShellTaskView, ShellTaskAction as TaskControlAction,
    ShellTaskCapabilities as TaskCapabilities, ShellTaskOrigin as TaskOrigin,
    ShellTaskOutputChunk as TaskOutputChunk, ShellTaskSnapshot as TaskSurfaceSnapshot,
    ShellTaskState as TaskStatus,
};

const TAB_COLUMNS: usize = 4;
const MAX_CONSECUTIVE_ZERO_WIDTH_CHARACTERS: usize = 16;
const OUTPUT_READ_LIMIT: usize = 64 * 1024;
const OUTPUT_DISPLAY_MAX_BYTES: usize = OUTPUT_READ_LIMIT * 3;
const ACTION_ERROR_TITLE_COLUMNS: usize = 40;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskViewport {
    pub width: usize,
    pub height: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskOutputUpdate {
    Chunk {
        generation: u64,
        chunk: TaskOutputChunk,
    },
    Unavailable {
        generation: u64,
        task_id: Uuid,
        total_bytes: u64,
        message: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskSurfaceFocus {
    List,
    Detail { task_id: Uuid },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskSurfaceAction {
    Previous,
    Next,
    Open,
    Back,
    Close,
    Background,
    Stop,
    ScrollLines(i32),
    Page(i32),
    Home,
    End,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskSurfaceEffect {
    None,
    Close,
    Control {
        task_id: Uuid,
        action: TaskControlAction,
    },
    ReadOutput {
        task_id: Uuid,
        cursor: u64,
        limit: usize,
        generation: u64,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskFrameLineKind {
    Empty,
    ActiveTask,
    CompletedTask,
    FailedTask,
    UnknownTask,
    Metadata,
    Output,
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskFrameLine {
    pub kind: TaskFrameLineKind,
    pub text: String,
    pub task_id: Option<Uuid>,
    pub selected: bool,
    pub continuation: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskSurfaceFrame {
    pub focus: TaskSurfaceFocus,
    pub lines: Vec<TaskFrameLine>,
    pub selected_task_id: Option<Uuid>,
    pub visible_tasks: Range<usize>,
    pub top_row: usize,
    pub total_rows: usize,
    pub can_move_up: bool,
    pub can_move_down: bool,
    pub selected_capabilities: TaskCapabilities,
    pub follow_tail: bool,
    pub unread_bytes: u64,
    pub unavailable_task_id: Option<Uuid>,
    pub action_error: Option<String>,
}

pub struct TaskSurface<Restore = ()> {
    restore: Restore,
    /// 打开任务中心的前台 Chat；其他 Chat 的任务在行内标注所属 Chat。
    chat_id: Uuid,
    revision: u64,
    tasks: Vec<ShellTaskView>,
    focus: TaskSurfaceFocus,
    selected_task_id: Option<Uuid>,
    fallback_task_index: usize,
    list_top: usize,
    unavailable_task_id: Option<Uuid>,
    detail_states: HashMap<Uuid, DetailState>,
    navigation_generation: u64,
    action_error: Option<String>,
}

#[derive(Default)]
struct DetailState {
    navigation: DetailNavigation,
    output: Option<StoredOutputChunk>,
    output_error: Option<String>,
    known_total_bytes: u64,
    unread_bytes: u64,
    pending_output_position: Option<PendingOutputPosition>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct DetailNavigation {
    anchor: Option<DetailAnchor>,
    follow_tail: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PendingOutputPosition {
    Waiting {
        generation: u64,
        cursor: u64,
        position: OutputPosition,
    },
    Ready(OutputPosition),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OutputPosition {
    MetadataStart { rows_after_start: usize },
    WindowStart { rows_after_start: usize },
    WindowEnd { rows_before_end: usize },
}

impl OutputPosition {
    fn shifted(self, delta: isize) -> Self {
        match self {
            Self::MetadataStart { rows_after_start } => Self::MetadataStart {
                rows_after_start: shift_forward(rows_after_start, delta),
            },
            Self::WindowStart { rows_after_start } => Self::WindowStart {
                rows_after_start: shift_forward(rows_after_start, delta),
            },
            Self::WindowEnd { rows_before_end } => Self::WindowEnd {
                rows_before_end: if delta.is_negative() {
                    rows_before_end.saturating_add(delta.unsigned_abs())
                } else {
                    rows_before_end.saturating_sub(delta as usize)
                },
            },
        }
    }
}

fn shift_forward(position: usize, delta: isize) -> usize {
    if delta.is_negative() {
        position.saturating_sub(delta.unsigned_abs())
    } else {
        position.saturating_add(delta as usize)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum DetailAnchor {
    Metadata {
        row: usize,
        cell_offset: usize,
    },
    Output {
        chunk_offset: u64,
        display_byte: usize,
        cell_offset: usize,
    },
    Unavailable {
        row: usize,
        cell_offset: usize,
    },
}

struct StoredOutputChunk {
    offset: u64,
    bytes: Vec<u8>,
    confirmed_offset: u64,
    next_offset: Option<u64>,
    truncated: bool,
    encoding: String,
    total_bytes: u64,
}

impl<Restore> TaskSurface<Restore> {
    pub fn open(snapshot: TaskSurfaceSnapshot, restore: Restore) -> Self {
        let selected_task_id = snapshot.tasks.first().map(|task| task.id);
        let focus = if snapshot.tasks.len() == 1 {
            TaskSurfaceFocus::Detail {
                task_id: snapshot.tasks[0].id,
            }
        } else {
            TaskSurfaceFocus::List
        };
        let mut detail_states = HashMap::new();
        if let TaskSurfaceFocus::Detail { task_id } = focus {
            detail_states.insert(
                task_id,
                DetailState {
                    navigation: DetailNavigation {
                        anchor: None,
                        follow_tail: true,
                    },
                    known_total_bytes: snapshot.tasks[0].output_bytes,
                    ..DetailState::default()
                },
            );
        }
        Self {
            restore,
            chat_id: snapshot.chat_id,
            revision: snapshot.live_revision,
            tasks: snapshot.tasks,
            focus,
            selected_task_id,
            fallback_task_index: 0,
            list_top: 0,
            unavailable_task_id: None,
            detail_states,
            navigation_generation: 0,
            action_error: None,
        }
    }

    pub fn focus(&self) -> TaskSurfaceFocus {
        self.focus
    }

    pub fn open_detail(&mut self, task_id: Uuid, viewport: TaskViewport) -> TaskSurfaceEffect {
        if self.task(task_id).is_none() {
            return TaskSurfaceEffect::None;
        }
        self.selected_task_id = Some(task_id);
        self.focus = TaskSurfaceFocus::List;
        self.apply(TaskSurfaceAction::Open, viewport)
    }

    pub fn restore(&self) -> &Restore {
        &self.restore
    }

    pub fn initial_effect(&mut self) -> TaskSurfaceEffect {
        match self.focus {
            TaskSurfaceFocus::Detail { task_id } => {
                let Some(task) = self.task(task_id) else {
                    return TaskSurfaceEffect::None;
                };
                let (total_bytes, can_read_output) =
                    (task.output_bytes, task.capabilities.can_read_output);
                if can_read_output
                    && let Some(detail) = self.detail_states.get_mut(&task_id)
                    && let Some(PendingOutputPosition::Waiting {
                        cursor, position, ..
                    }) = detail.pending_output_position
                {
                    detail.pending_output_position = Some(PendingOutputPosition::Waiting {
                        generation: self.navigation_generation,
                        cursor,
                        position,
                    });
                    return TaskSurfaceEffect::ReadOutput {
                        task_id,
                        cursor,
                        limit: OUTPUT_READ_LIMIT,
                        generation: self.navigation_generation,
                    };
                }
                tail_read_effect(
                    task_id,
                    total_bytes,
                    can_read_output,
                    self.navigation_generation,
                )
            }
            TaskSurfaceFocus::List => TaskSurfaceEffect::None,
        }
    }

    pub fn update_snapshot(&mut self, snapshot: TaskSurfaceSnapshot) -> TaskSurfaceEffect {
        self.update_snapshot_with_visibility(snapshot, true)
    }

    pub fn reopen(&mut self, snapshot: TaskSurfaceSnapshot) -> TaskSurfaceEffect {
        self.update_snapshot_hidden(snapshot);
        if let TaskSurfaceFocus::Detail { task_id } = self.focus
            && self.detail_states.get(&task_id).is_none_or(|detail| {
                detail.output.is_none()
                    || matches!(
                        detail.pending_output_position,
                        Some(PendingOutputPosition::Waiting { .. })
                    )
            })
        {
            self.initial_effect()
        } else {
            TaskSurfaceEffect::None
        }
    }

    pub fn update_snapshot_hidden(&mut self, snapshot: TaskSurfaceSnapshot) {
        let _ = self.update_snapshot_with_visibility(snapshot, false);
    }

    fn update_snapshot_with_visibility(
        &mut self,
        snapshot: TaskSurfaceSnapshot,
        is_visible: bool,
    ) -> TaskSurfaceEffect {
        if snapshot.live_revision < self.revision {
            return TaskSurfaceEffect::None;
        }
        let previous_selected_index = self.selected_index().unwrap_or(self.fallback_task_index);
        let previous_selected_id = self.selected_task_id;
        let focused_detail = match self.focus {
            TaskSurfaceFocus::Detail { task_id } => Some(task_id),
            TaskSurfaceFocus::List => None,
        };
        let previous_totals = self
            .tasks
            .iter()
            .map(|task| (task.id, task.output_bytes))
            .collect::<HashMap<_, _>>();

        self.revision = snapshot.live_revision;
        self.tasks = snapshot.tasks;
        self.fallback_task_index = previous_selected_index;
        if let Some(task_id) = focused_detail
            && let Some(task_index) = self.tasks.iter().position(|task| task.id == task_id)
        {
            self.selected_task_id = Some(task_id);
            self.fallback_task_index = task_index;
        }
        self.normalize_selection(None);

        if self
            .unavailable_task_id
            .is_some_and(|task_id| self.tasks.iter().any(|task| task.id == task_id))
        {
            self.unavailable_task_id = None;
        }
        if let Some(selected_id) = previous_selected_id
            && !self.tasks.iter().any(|task| task.id == selected_id)
        {
            self.unavailable_task_id = Some(selected_id);
        }

        let current_tasks = self
            .tasks
            .iter()
            .map(|task| {
                (
                    task.id,
                    (task.output_bytes, task.capabilities.can_read_output),
                )
            })
            .collect::<HashMap<_, _>>();
        let mut effect = TaskSurfaceEffect::None;
        for (task_id, detail) in &mut self.detail_states {
            let Some((current_total, can_read_output)) = current_tasks.get(task_id).copied() else {
                continue;
            };
            let known_total = detail
                .known_total_bytes
                .max(previous_totals.get(task_id).copied().unwrap_or(0));
            let growth = current_total.saturating_sub(known_total);
            detail.known_total_bytes = detail.known_total_bytes.max(current_total);
            if growth == 0 {
                continue;
            }
            if is_visible && focused_detail == Some(*task_id) && detail.navigation.follow_tail {
                detail.unread_bytes = 0;
                effect = tail_read_effect(
                    *task_id,
                    current_total,
                    can_read_output,
                    self.navigation_generation,
                );
            } else {
                detail.unread_bytes = detail.unread_bytes.saturating_add(growth);
                detail.navigation.follow_tail = false;
            }
        }
        effect
    }

    pub fn report_action_error(&mut self, task_id: Uuid, message: String) {
        let target = self
            .tasks
            .iter()
            .find(|task| task.id == task_id)
            .map_or_else(
                || "Shell task".to_owned(),
                |task| {
                    fit_visible_fragments(&[task.command.as_bytes()], ACTION_ERROR_TITLE_COLUMNS)
                },
            );
        self.action_error = Some(format!("{target}: {message}"));
    }

    pub fn report_update_error(&mut self, message: String) {
        self.action_error = Some(format!("Task updates: {message} · reopen /tasks to retry"));
    }

    pub fn clear_action_error(&mut self) {
        self.action_error = None;
    }

    pub fn update_output(&mut self, update: TaskOutputUpdate) -> TaskSurfaceEffect {
        let (generation, task_id, total_bytes) = match &update {
            TaskOutputUpdate::Chunk { generation, chunk } => {
                (*generation, chunk.task_id, chunk.total_bytes)
            }
            TaskOutputUpdate::Unavailable {
                generation,
                task_id,
                total_bytes,
                ..
            } => (*generation, *task_id, *total_bytes),
        };
        if generation != self.navigation_generation {
            return TaskSurfaceEffect::None;
        }
        let can_read_output = self
            .task(task_id)
            .is_some_and(|task| task.capabilities.can_read_output);
        let detail = self.detail_states.entry(task_id).or_default();
        let growth = total_bytes.saturating_sub(detail.known_total_bytes);
        detail.known_total_bytes = detail.known_total_bytes.max(total_bytes);
        if detail.navigation.follow_tail {
            detail.unread_bytes = 0;
        } else {
            detail.unread_bytes = detail.unread_bytes.saturating_add(growth);
        }
        let needs_new_tail = match update {
            TaskOutputUpdate::Chunk { chunk, .. } => {
                let needs_new_tail =
                    detail.navigation.follow_tail && chunk.confirmed_offset < chunk.total_bytes;
                let chunk_offset = chunk.offset;
                detail.output = Some(store_bounded_chunk(chunk));
                detail.output_error = None;
                if detail.navigation.follow_tail {
                    detail.navigation.anchor = None;
                    detail.pending_output_position = None;
                } else if let Some(PendingOutputPosition::Waiting {
                    generation: pending_generation,
                    position,
                    ..
                }) = detail.pending_output_position
                    && pending_generation == generation
                {
                    detail.pending_output_position = Some(PendingOutputPosition::Ready(position));
                } else if !matches!(
                    detail.navigation.anchor,
                    Some(DetailAnchor::Metadata { .. })
                ) {
                    detail.navigation.anchor = Some(DetailAnchor::Output {
                        chunk_offset,
                        display_byte: 0,
                        cell_offset: 0,
                    });
                }
                needs_new_tail
            }
            TaskOutputUpdate::Unavailable { message, .. } => {
                detail.output = None;
                detail.output_error = Some(message);
                detail.pending_output_position = None;
                false
            }
        };
        if needs_new_tail {
            tail_read_effect(
                task_id,
                total_bytes,
                can_read_output,
                self.navigation_generation,
            )
        } else {
            TaskSurfaceEffect::None
        }
    }

    pub fn apply(
        &mut self,
        action: TaskSurfaceAction,
        viewport: TaskViewport,
    ) -> TaskSurfaceEffect {
        let previous_generation = self.navigation_generation;
        let previous_focus = self.focus;
        let previous_selected_task_id = self.selected_task_id;
        let previous_navigation = match self.focus {
            TaskSurfaceFocus::Detail { task_id } => self
                .detail_states
                .get(&task_id)
                .map(|detail| detail.navigation.clone()),
            TaskSurfaceFocus::List => None,
        };
        let previous_pending_output_position = match self.focus {
            TaskSurfaceFocus::Detail { task_id } => self
                .detail_states
                .get(&task_id)
                .and_then(|detail| detail.pending_output_position),
            TaskSurfaceFocus::List => None,
        };
        self.navigation_generation = self.navigation_generation.wrapping_add(1);
        self.normalize_selection(Some(viewport.height));
        let effect = match self.focus {
            TaskSurfaceFocus::List => self.apply_list(action, viewport),
            TaskSurfaceFocus::Detail { task_id } => self.apply_detail(task_id, action, viewport),
        };
        let current_navigation = match self.focus {
            TaskSurfaceFocus::Detail { task_id } => self
                .detail_states
                .get(&task_id)
                .map(|detail| detail.navigation.clone()),
            TaskSurfaceFocus::List => None,
        };
        let current_pending_output_position = match self.focus {
            TaskSurfaceFocus::Detail { task_id } => self
                .detail_states
                .get(&task_id)
                .and_then(|detail| detail.pending_output_position),
            TaskSurfaceFocus::List => None,
        };
        if matches!(
            effect,
            TaskSurfaceEffect::None | TaskSurfaceEffect::Control { .. }
        ) && self.focus == previous_focus
            && self.selected_task_id == previous_selected_task_id
            && current_navigation == previous_navigation
            && current_pending_output_position == previous_pending_output_position
        {
            self.navigation_generation = previous_generation;
        }
        effect
    }

    pub fn frame(&mut self, viewport: TaskViewport) -> TaskSurfaceFrame {
        self.normalize_selection(Some(viewport.height));
        match self.focus {
            TaskSurfaceFocus::List => self.list_frame(viewport),
            TaskSurfaceFocus::Detail { task_id } => self.detail_frame(task_id, viewport),
        }
    }

    fn apply_list(
        &mut self,
        action: TaskSurfaceAction,
        viewport: TaskViewport,
    ) -> TaskSurfaceEffect {
        match action {
            TaskSurfaceAction::Previous => self.move_selection(false, viewport.height),
            TaskSurfaceAction::Next => self.move_selection(true, viewport.height),
            TaskSurfaceAction::Open => {
                let Some(task_id) = self.selected_task_id else {
                    return TaskSurfaceEffect::None;
                };
                self.unavailable_task_id = None;
                self.focus = TaskSurfaceFocus::Detail { task_id };
                let output_bytes = self.task(task_id).map_or(0, |task| task.output_bytes);
                let detail = self.detail_states.entry(task_id).or_default();
                detail.known_total_bytes = detail.known_total_bytes.max(output_bytes);
                if matches!(
                    detail.pending_output_position,
                    Some(PendingOutputPosition::Waiting { .. })
                ) {
                    return self.initial_effect();
                }
                if detail.output.is_none() {
                    detail.navigation.follow_tail = true;
                    return self.initial_effect();
                }
            }
            TaskSurfaceAction::Back | TaskSurfaceAction::Close => {
                return TaskSurfaceEffect::Close;
            }
            TaskSurfaceAction::Background => {
                return self.control_effect(TaskControlAction::Background);
            }
            TaskSurfaceAction::Stop => return self.control_effect(TaskControlAction::Stop),
            TaskSurfaceAction::ScrollLines(_)
            | TaskSurfaceAction::Page(_)
            | TaskSurfaceAction::Home
            | TaskSurfaceAction::End => {}
        }
        TaskSurfaceEffect::None
    }

    fn apply_detail(
        &mut self,
        task_id: Uuid,
        action: TaskSurfaceAction,
        viewport: TaskViewport,
    ) -> TaskSurfaceEffect {
        match action {
            TaskSurfaceAction::Back => {
                self.focus = TaskSurfaceFocus::List;
                TaskSurfaceEffect::None
            }
            TaskSurfaceAction::Close => TaskSurfaceEffect::Close,
            TaskSurfaceAction::Background => {
                self.control_effect_for(task_id, TaskControlAction::Background)
            }
            TaskSurfaceAction::Stop => self.control_effect_for(task_id, TaskControlAction::Stop),
            TaskSurfaceAction::Previous | TaskSurfaceAction::Next | TaskSurfaceAction::Open => {
                TaskSurfaceEffect::None
            }
            TaskSurfaceAction::ScrollLines(lines) => {
                self.navigate_detail(task_id, viewport, lines as isize, false, false)
            }
            TaskSurfaceAction::Page(pages) => {
                let rows = (viewport.height.max(1) as isize).saturating_mul(pages as isize);
                self.navigate_detail(task_id, viewport, rows, false, false)
            }
            TaskSurfaceAction::Home => self.navigate_detail(task_id, viewport, 0, true, false),
            TaskSurfaceAction::End => self.navigate_detail(task_id, viewport, 0, false, true),
        }
    }

    fn navigate_detail(
        &mut self,
        task_id: Uuid,
        viewport: TaskViewport,
        delta: isize,
        home: bool,
        end: bool,
    ) -> TaskSurfaceEffect {
        let Some(task) = self.tasks.iter().find(|task| task.id == task_id) else {
            return TaskSurfaceEffect::None;
        };
        let can_read_output = task.capabilities.can_read_output;
        let detail = self.detail_states.entry(task_id).or_default();
        if let Some(PendingOutputPosition::Waiting {
            cursor, position, ..
        }) = detail.pending_output_position
            && !end
            && can_read_output
        {
            let cursor = if home { 0 } else { cursor };
            let position = if home {
                OutputPosition::MetadataStart {
                    rows_after_start: 0,
                }
            } else {
                position.shifted(delta)
            };
            detail.navigation.follow_tail = false;
            detail.pending_output_position = Some(PendingOutputPosition::Waiting {
                generation: self.navigation_generation,
                cursor,
                position,
            });
            return TaskSurfaceEffect::ReadOutput {
                task_id,
                cursor,
                limit: OUTPUT_READ_LIMIT,
                generation: self.navigation_generation,
            };
        }
        let layout = DetailLayout::new(task, self.chat_id, detail, viewport.width);
        let measurement = layout.measure(detail.navigation.anchor.as_ref());
        let maximum_top = measurement
            .total_rows
            .saturating_sub(viewport.height.max(1));
        let current = if detail.navigation.follow_tail {
            maximum_top
        } else {
            measurement.anchor_row.min(maximum_top)
        };
        let target = if home {
            0
        } else if end {
            maximum_top
        } else if delta.is_negative() {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current.saturating_add(delta as usize).min(maximum_top)
        };
        let target_anchor = layout.anchor_at(target);
        let remains_at_followed_tail = detail.navigation.follow_tail
            && !home
            && !end
            && !delta.is_negative()
            && target == maximum_top;
        if remains_at_followed_tail {
            return TaskSurfaceEffect::None;
        }
        detail.navigation.follow_tail = end || remains_at_followed_tail;
        detail.navigation.anchor = target_anchor;
        detail.pending_output_position = None;
        if end {
            detail.unread_bytes = 0;
            return tail_read_effect(
                task_id,
                detail.known_total_bytes,
                can_read_output,
                self.navigation_generation,
            );
        }

        if detail.output.is_none() && can_read_output {
            let cursor = if home {
                0
            } else {
                detail
                    .known_total_bytes
                    .saturating_sub(OUTPUT_READ_LIMIT as u64)
            };
            let position = if home {
                OutputPosition::MetadataStart {
                    rows_after_start: 0,
                }
            } else {
                OutputPosition::WindowEnd {
                    rows_before_end: if delta.is_negative() {
                        delta.unsigned_abs()
                    } else {
                        0
                    },
                }
            };
            detail.pending_output_position = Some(PendingOutputPosition::Waiting {
                generation: self.navigation_generation,
                cursor,
                position,
            });
            return TaskSurfaceEffect::ReadOutput {
                task_id,
                cursor,
                limit: OUTPUT_READ_LIMIT,
                generation: self.navigation_generation,
            };
        }

        if target == 0
            && let Some(output) = &detail.output
            && output.offset > 0
        {
            let cursor = if home {
                0
            } else {
                output.offset.saturating_sub(OUTPUT_READ_LIMIT as u64)
            };
            detail.pending_output_position = Some(PendingOutputPosition::Waiting {
                generation: self.navigation_generation,
                cursor,
                position: if home {
                    OutputPosition::MetadataStart {
                        rows_after_start: 0,
                    }
                } else {
                    OutputPosition::WindowEnd { rows_before_end: 0 }
                },
            });
            return TaskSurfaceEffect::ReadOutput {
                task_id,
                cursor,
                limit: OUTPUT_READ_LIMIT,
                generation: self.navigation_generation,
            };
        }
        if target == maximum_top
            && delta.is_positive()
            && let Some(output) = &detail.output
        {
            let next_cursor = output.next_offset.unwrap_or(output.confirmed_offset);
            if next_cursor < output.total_bytes {
                detail.pending_output_position = Some(PendingOutputPosition::Waiting {
                    generation: self.navigation_generation,
                    cursor: next_cursor,
                    position: OutputPosition::WindowStart {
                        rows_after_start: 0,
                    },
                });
                return TaskSurfaceEffect::ReadOutput {
                    task_id,
                    cursor: next_cursor,
                    limit: OUTPUT_READ_LIMIT,
                    generation: self.navigation_generation,
                };
            }
        }
        TaskSurfaceEffect::None
    }

    fn control_effect(&self, action: TaskControlAction) -> TaskSurfaceEffect {
        self.selected_task_id
            .map_or(TaskSurfaceEffect::None, |task_id| {
                self.control_effect_for(task_id, action)
            })
    }

    fn control_effect_for(&self, task_id: Uuid, action: TaskControlAction) -> TaskSurfaceEffect {
        let Some(task) = self.task(task_id) else {
            return TaskSurfaceEffect::None;
        };
        let allowed = match action {
            TaskControlAction::Background => task.capabilities.can_background,
            TaskControlAction::Stop => task.capabilities.can_stop,
        };
        if allowed {
            TaskSurfaceEffect::Control { task_id, action }
        } else {
            TaskSurfaceEffect::None
        }
    }

    fn list_frame(&mut self, viewport: TaskViewport) -> TaskSurfaceFrame {
        if viewport.width == 0 || viewport.height == 0 {
            return empty_frame(self.focus, self.selected_task_id, self.unavailable_task_id);
        }
        let mut lines = Vec::with_capacity(viewport.height);
        if let Some(task_id) = self.unavailable_task_id {
            lines.push(TaskFrameLine {
                kind: TaskFrameLineKind::Unavailable,
                text: fit_visible(
                    &format!("Task {task_id} is no longer available"),
                    viewport.width,
                ),
                task_id: Some(task_id),
                selected: false,
                continuation: false,
            });
        }
        let available_height = viewport.height.saturating_sub(lines.len());
        if self.tasks.is_empty() && available_height > 0 {
            lines.push(TaskFrameLine {
                kind: TaskFrameLineKind::Empty,
                text: fit_visible("No Shell tasks", viewport.width),
                task_id: None,
                selected: false,
                continuation: false,
            });
        }
        let end = self
            .list_top
            .saturating_add(available_height)
            .min(self.tasks.len());
        for task in &self.tasks[self.list_top..end] {
            lines.push(TaskFrameLine {
                kind: task_line_kind(task.state),
                text: render_task_row(task, self.chat_id, viewport.width),
                task_id: Some(task.id),
                selected: self.selected_task_id == Some(task.id),
                continuation: false,
            });
        }
        let selected_capabilities = self
            .selected_task_id
            .and_then(|task_id| self.task(task_id))
            .map_or_else(TaskCapabilities::default, |task| task.capabilities);
        TaskSurfaceFrame {
            focus: self.focus,
            lines,
            selected_task_id: self.selected_task_id,
            visible_tasks: self.list_top..end,
            top_row: self.list_top,
            total_rows: self.tasks.len(),
            can_move_up: self.fallback_task_index > 0,
            can_move_down: !self.tasks.is_empty()
                && self.fallback_task_index < self.tasks.len().saturating_sub(1),
            selected_capabilities,
            follow_tail: false,
            unread_bytes: 0,
            unavailable_task_id: self.unavailable_task_id,
            action_error: self.action_error.clone(),
        }
    }

    fn detail_frame(&mut self, task_id: Uuid, viewport: TaskViewport) -> TaskSurfaceFrame {
        if viewport.width == 0 || viewport.height == 0 {
            return empty_frame(self.focus, self.selected_task_id, self.unavailable_task_id);
        }
        let Some(task) = self.tasks.iter().find(|task| task.id == task_id) else {
            let lines = unavailable_detail_lines(task_id, viewport);
            return TaskSurfaceFrame {
                focus: self.focus,
                total_rows: lines.len(),
                lines,
                selected_task_id: self.selected_task_id,
                visible_tasks: 0..0,
                top_row: 0,
                can_move_up: false,
                can_move_down: false,
                selected_capabilities: TaskCapabilities::default(),
                follow_tail: false,
                unread_bytes: 0,
                unavailable_task_id: Some(task_id),
                action_error: self.action_error.clone(),
            };
        };
        let detail = self.detail_states.entry(task_id).or_default();
        let rendered = render_detail(task, self.chat_id, detail, viewport);
        detail.navigation = rendered.navigation;
        TaskSurfaceFrame {
            focus: self.focus,
            lines: rendered.lines,
            selected_task_id: self.selected_task_id,
            visible_tasks: 0..0,
            top_row: rendered.top_row,
            total_rows: rendered.total_rows,
            can_move_up: rendered.top_row > 0,
            can_move_down: rendered.top_row < rendered.maximum_top,
            selected_capabilities: task.capabilities,
            follow_tail: detail.navigation.follow_tail,
            unread_bytes: detail.unread_bytes,
            unavailable_task_id: self.unavailable_task_id,
            action_error: self.action_error.clone(),
        }
    }

    fn task(&self, task_id: Uuid) -> Option<&ShellTaskView> {
        self.tasks.iter().find(|task| task.id == task_id)
    }

    fn selected_index(&self) -> Option<usize> {
        self.selected_task_id
            .and_then(|task_id| self.tasks.iter().position(|task| task.id == task_id))
    }

    fn normalize_selection(&mut self, viewport_height: Option<usize>) {
        if self.tasks.is_empty() {
            self.selected_task_id = None;
            self.fallback_task_index = 0;
            self.list_top = 0;
            return;
        }
        let selected_index = self
            .selected_index()
            .unwrap_or_else(|| self.fallback_task_index.min(self.tasks.len() - 1));
        self.fallback_task_index = selected_index;
        self.selected_task_id = Some(self.tasks[selected_index].id);
        if let Some(viewport_height) = viewport_height {
            let warning_rows = usize::from(self.unavailable_task_id.is_some());
            keep_selected_visible(
                &mut self.list_top,
                selected_index,
                self.tasks.len(),
                viewport_height.saturating_sub(warning_rows),
            );
        }
    }

    fn move_selection(&mut self, forward: bool, viewport_height: usize) {
        if self.tasks.is_empty() {
            return;
        }
        self.fallback_task_index = if forward {
            self.fallback_task_index
                .saturating_add(1)
                .min(self.tasks.len() - 1)
        } else {
            self.fallback_task_index.saturating_sub(1)
        };
        self.selected_task_id = Some(self.tasks[self.fallback_task_index].id);
        self.unavailable_task_id = None;
        keep_selected_visible(
            &mut self.list_top,
            self.fallback_task_index,
            self.tasks.len(),
            viewport_height,
        );
    }
}

fn tail_read_effect(
    task_id: Uuid,
    total_bytes: u64,
    can_read_output: bool,
    generation: u64,
) -> TaskSurfaceEffect {
    if !can_read_output {
        return TaskSurfaceEffect::None;
    }
    TaskSurfaceEffect::ReadOutput {
        task_id,
        cursor: total_bytes.saturating_sub(OUTPUT_READ_LIMIT as u64),
        limit: OUTPUT_READ_LIMIT,
        generation,
    }
}

fn store_bounded_chunk(chunk: TaskOutputChunk) -> StoredOutputChunk {
    let mut output = chunk.output;
    if output.len() > OUTPUT_DISPLAY_MAX_BYTES {
        let mut end = OUTPUT_DISPLAY_MAX_BYTES;
        while !output.is_char_boundary(end) {
            end -= 1;
        }
        output.truncate(end);
    }
    StoredOutputChunk {
        offset: chunk.offset,
        bytes: output.into_bytes(),
        confirmed_offset: chunk.confirmed_offset,
        next_offset: chunk.next_offset,
        truncated: chunk.truncated,
        encoding: chunk.encoding,
        total_bytes: chunk.total_bytes,
    }
}

fn keep_selected_visible(top: &mut usize, selected: usize, count: usize, height: usize) {
    if height == 0 || count == 0 {
        *top = 0;
        return;
    }
    *top = (*top).min(count.saturating_sub(height));
    if selected < *top {
        *top = selected;
    } else if selected >= top.saturating_add(height) {
        *top = selected + 1 - height;
    }
}

fn empty_frame(
    focus: TaskSurfaceFocus,
    selected_task_id: Option<Uuid>,
    unavailable_task_id: Option<Uuid>,
) -> TaskSurfaceFrame {
    TaskSurfaceFrame {
        focus,
        lines: Vec::new(),
        selected_task_id,
        visible_tasks: 0..0,
        top_row: 0,
        total_rows: 0,
        can_move_up: false,
        can_move_down: false,
        selected_capabilities: TaskCapabilities::default(),
        follow_tail: false,
        unread_bytes: 0,
        unavailable_task_id,
        action_error: None,
    }
}

fn task_line_kind(status: TaskStatus) -> TaskFrameLineKind {
    match status {
        TaskStatus::Queued
        | TaskStatus::Running
        | TaskStatus::Background
        | TaskStatus::Cancelling => TaskFrameLineKind::ActiveTask,
        TaskStatus::Completed => TaskFrameLineKind::CompletedTask,
        TaskStatus::Failed => TaskFrameLineKind::FailedTask,
        TaskStatus::Unknown => TaskFrameLineKind::UnknownTask,
    }
}

fn render_task_row(task: &ShellTaskView, view_chat_id: Uuid, width: usize) -> String {
    let origin = match task.origin {
        TaskOrigin::Agent => "Agent",
        TaskOrigin::User => "User",
    };
    let status = status_label(task.state);
    let owner = if task.chat_id == view_chat_id {
        String::new()
    } else {
        format!("{OTHER_AGENT} · ")
    };
    let prefix = format!("{origin} {status} · {owner}{} B · ", task.output_bytes);
    let cwd = task.cwd.to_string_lossy();
    fit_visible_fragments(
        &[
            prefix.as_bytes(),
            task.command.as_bytes(),
            " · ".as_bytes(),
            cwd.as_bytes(),
        ],
        width,
    )
}

/// A task this View's Agent does not own; its owner is named in the Agents overview, not by a
/// generated identifier here.
const OTHER_AGENT: &str = "other Agent";

fn status_label(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Queued => "queued",
        TaskStatus::Running => "running",
        TaskStatus::Background => "background",
        TaskStatus::Cancelling => "stopping",
        TaskStatus::Completed => "completed",
        TaskStatus::Failed => "failed",
        TaskStatus::Unknown => "unknown",
    }
}

struct RenderedDetail {
    lines: Vec<TaskFrameLine>,
    navigation: DetailNavigation,
    top_row: usize,
    total_rows: usize,
    maximum_top: usize,
}

fn render_detail(
    task: &ShellTaskView,
    view_chat_id: Uuid,
    detail: &mut DetailState,
    viewport: TaskViewport,
) -> RenderedDetail {
    let ready_position = detail
        .pending_output_position
        .and_then(|pending| match pending {
            PendingOutputPosition::Ready(position) => Some(position),
            PendingOutputPosition::Waiting { .. } => None,
        });
    let layout = DetailLayout::new(task, view_chat_id, detail, viewport.width);
    let measurement = layout.measure(detail.navigation.anchor.as_ref());
    let maximum_top = measurement
        .total_rows
        .saturating_sub(viewport.height.max(1));
    let top_row = match ready_position {
        Some(OutputPosition::MetadataStart { rows_after_start }) => {
            rows_after_start.min(maximum_top)
        }
        Some(OutputPosition::WindowStart { rows_after_start }) => {
            let window_start = detail
                .output
                .as_ref()
                .map(|output| DetailAnchor::Output {
                    chunk_offset: output.offset,
                    display_byte: 0,
                    cell_offset: 0,
                })
                .as_ref()
                .map_or(0, |anchor| layout.measure(Some(anchor)).anchor_row);
            window_start
                .saturating_add(rows_after_start)
                .min(maximum_top)
        }
        Some(OutputPosition::WindowEnd { rows_before_end }) => {
            maximum_top.saturating_sub(rows_before_end)
        }
        None if detail.navigation.follow_tail => maximum_top,
        None => measurement.anchor_row.min(maximum_top),
    };
    let lines = layout.materialize(top_row..top_row.saturating_add(viewport.height));
    let navigation = DetailNavigation {
        anchor: lines.first().and_then(|line| line.anchor.clone()),
        follow_tail: detail.navigation.follow_tail,
    };
    if ready_position.is_some() {
        detail.pending_output_position = None;
    }
    RenderedDetail {
        lines: lines.into_iter().map(|line| line.line).collect(),
        navigation,
        top_row,
        total_rows: measurement.total_rows,
        maximum_top,
    }
}

fn unavailable_detail_lines(task_id: Uuid, viewport: TaskViewport) -> Vec<TaskFrameLine> {
    [
        format!("Shell task {task_id}"),
        "Task is no longer available".to_owned(),
    ]
    .into_iter()
    .take(viewport.height)
    .enumerate()
    .map(|(row, text)| TaskFrameLine {
        kind: if row == 0 {
            TaskFrameLineKind::Metadata
        } else {
            TaskFrameLineKind::Unavailable
        },
        text: fit_visible(&text, viewport.width),
        task_id: Some(task_id),
        selected: false,
        continuation: false,
    })
    .collect()
}

struct DetailLayout<'a> {
    task: &'a ShellTaskView,
    view_chat_id: Uuid,
    detail: &'a DetailState,
    width: usize,
}

struct DetailMeasurement {
    total_rows: usize,
    anchor_row: usize,
}

struct MaterializedLine {
    line: TaskFrameLine,
    anchor: Option<DetailAnchor>,
}

#[derive(Clone, Copy)]
enum DetailLocation {
    Metadata {
        row: usize,
    },
    Output {
        chunk_offset: u64,
        display_byte: usize,
    },
    Unavailable {
        row: usize,
    },
}

impl DetailLocation {
    fn with_cell_offset(self, cell_offset: usize) -> DetailAnchor {
        match self {
            Self::Metadata { row } => DetailAnchor::Metadata { row, cell_offset },
            Self::Output {
                chunk_offset,
                display_byte,
            } => DetailAnchor::Output {
                chunk_offset,
                display_byte,
                cell_offset,
            },
            Self::Unavailable { row } => DetailAnchor::Unavailable { row, cell_offset },
        }
    }

    fn matches(self, anchor: &DetailAnchor) -> bool {
        match (self, anchor) {
            (Self::Metadata { row: left }, DetailAnchor::Metadata { row: right, .. })
            | (Self::Unavailable { row: left }, DetailAnchor::Unavailable { row: right, .. }) => {
                left == *right
            }
            (
                Self::Output {
                    chunk_offset: left_chunk,
                    display_byte: left_byte,
                },
                DetailAnchor::Output {
                    chunk_offset: right_chunk,
                    display_byte: right_byte,
                    ..
                },
            ) => left_chunk == *right_chunk && left_byte == *right_byte,
            _ => false,
        }
    }
}

fn anchor_cell_offset(anchor: &DetailAnchor) -> usize {
    match anchor {
        DetailAnchor::Metadata { cell_offset, .. }
        | DetailAnchor::Output { cell_offset, .. }
        | DetailAnchor::Unavailable { cell_offset, .. } => *cell_offset,
    }
}

impl<'a> DetailLayout<'a> {
    fn new(
        task: &'a ShellTaskView,
        view_chat_id: Uuid,
        detail: &'a DetailState,
        width: usize,
    ) -> Self {
        Self {
            task,
            view_chat_id,
            detail,
            width: width.max(1),
        }
    }

    fn visit(&self, visitor: &mut dyn FnMut(DetailLogicalLine<'_>) -> bool) {
        let origin = match self.task.origin {
            TaskOrigin::Agent => "Agent",
            TaskOrigin::User => "User",
        };
        let status = format!("Status: {}", status_label(self.task.state));
        let cwd = self.task.cwd.to_string_lossy();
        let bytes = self.detail.output.as_ref().map_or_else(
            || format!("Output: {} bytes", self.task.output_bytes),
            |output| {
                let paging = if output.truncated { " · paged" } else { "" };
                format!(
                    "Output: {} bytes · {}{paging}",
                    output.total_bytes.max(self.task.output_bytes),
                    output.encoding
                )
            },
        );
        let controls = match (
            self.task.capabilities.can_background,
            self.task.capabilities.can_stop,
        ) {
            (true, true) => "Actions: background, stop",
            (true, false) => "Action: background",
            (false, true) => "Action: stop",
            (false, false) => "Actions: none",
        };
        let owner = (self.task.chat_id != self.view_chat_id).then(|| OTHER_AGENT.to_owned());
        let mut metadata_row = 0;
        for text in [origin.as_bytes()]
            .into_iter()
            .chain(owner.as_ref().map(String::as_bytes))
        {
            if !visitor(DetailLogicalLine::utf8(
                TaskFrameLineKind::Metadata,
                text,
                DetailLocation::Metadata { row: metadata_row },
            )) {
                return;
            }
            metadata_row += 1;
        }
        if !visitor(DetailLogicalLine::utf8(
            task_line_kind(self.task.state),
            status.as_bytes(),
            DetailLocation::Metadata { row: metadata_row },
        )) {
            return;
        }
        metadata_row += 1;
        for text in [
            b"Command".as_slice(),
            self.task.command.as_bytes(),
            b"Working directory".as_slice(),
            cwd.as_bytes(),
            bytes.as_bytes(),
            controls.as_bytes(),
        ] {
            if !visitor(DetailLogicalLine::utf8(
                TaskFrameLineKind::Metadata,
                text,
                DetailLocation::Metadata { row: metadata_row },
            )) {
                return;
            }
            metadata_row += 1;
        }
        for value in [
            self.task.exit_code.map(|value| format!("Exit: {value}")),
            self.task.signal.map(|value| format!("Signal: {value}")),
            self.task
                .reason
                .as_ref()
                .map(|value| format!("Reason: {value}")),
        ]
        .into_iter()
        .flatten()
        {
            if !visitor(DetailLogicalLine::utf8(
                TaskFrameLineKind::Metadata,
                value.as_bytes(),
                DetailLocation::Metadata { row: metadata_row },
            )) {
                return;
            }
            metadata_row += 1;
        }

        if let Some(error) = &self.detail.output_error {
            visitor(DetailLogicalLine::utf8(
                TaskFrameLineKind::Unavailable,
                error.as_bytes(),
                DetailLocation::Unavailable { row: 0 },
            ));
            return;
        }
        let Some(output) = &self.detail.output else {
            visitor(DetailLogicalLine::utf8(
                TaskFrameLineKind::Unavailable,
                b"Output not loaded",
                DetailLocation::Unavailable { row: 0 },
            ));
            return;
        };
        if output.bytes.is_empty() {
            visitor(DetailLogicalLine::utf8(
                TaskFrameLineKind::Empty,
                b"No output",
                DetailLocation::Output {
                    chunk_offset: output.offset,
                    display_byte: 0,
                },
            ));
            return;
        }
        let mut local_start = 0usize;
        loop {
            let remaining = &output.bytes[local_start..];
            let line_len = remaining
                .iter()
                .position(|byte| *byte == b'\n')
                .unwrap_or(remaining.len());
            let location = DetailLocation::Output {
                chunk_offset: output.offset,
                display_byte: local_start,
            };
            if !visitor(DetailLogicalLine {
                kind: TaskFrameLineKind::Output,
                bytes: &remaining[..line_len],
                location,
            }) {
                return;
            }
            if line_len == remaining.len() {
                break;
            }
            local_start += line_len + 1;
            if local_start == output.bytes.len() {
                visitor(DetailLogicalLine {
                    kind: TaskFrameLineKind::Output,
                    bytes: b"",
                    location: DetailLocation::Output {
                        chunk_offset: output.offset,
                        display_byte: local_start,
                    },
                });
                break;
            }
        }
    }

    fn measure(&self, anchor: Option<&DetailAnchor>) -> DetailMeasurement {
        let mut total_rows = 0usize;
        let mut anchor_row = 0usize;
        let mut found_anchor = false;
        self.visit(&mut |line| {
            let target_cell = anchor
                .filter(|anchor| line.location.matches(anchor))
                .map(anchor_cell_offset);
            let measurement = measure_wrapped(line.bytes, self.width, target_cell);
            if target_cell.is_some() {
                anchor_row = total_rows.saturating_add(measurement.anchor_segment);
                found_anchor = true;
            }
            total_rows = total_rows.saturating_add(measurement.segment_count);
            true
        });
        if let Some(anchor) = anchor.filter(|_| !found_anchor) {
            anchor_row = closest_anchor_row(self, anchor);
        }
        DetailMeasurement {
            total_rows,
            anchor_row,
        }
    }

    fn anchor_at(&self, target_row: usize) -> Option<DetailAnchor> {
        let mut current_row = 0usize;
        let mut result = None;
        self.visit(&mut |line| {
            let measurement = measure_wrapped(line.bytes, self.width, None);
            let end = current_row.saturating_add(measurement.segment_count);
            if target_row < end {
                let segment = target_row.saturating_sub(current_row);
                result = Some(
                    line.location
                        .with_cell_offset(segment_start_cell(line.bytes, self.width, segment)),
                );
                return false;
            }
            current_row = end;
            true
        });
        result
    }

    fn materialize(&self, range: Range<usize>) -> Vec<MaterializedLine> {
        if range.is_empty() {
            return Vec::new();
        }
        let mut current_row = 0usize;
        let mut lines = Vec::with_capacity(range.end.saturating_sub(range.start));
        self.visit(&mut |line| {
            let measurement = measure_wrapped(line.bytes, self.width, None);
            let line_end = current_row.saturating_add(measurement.segment_count);
            if line_end > range.start && current_row < range.end {
                let first = range.start.saturating_sub(current_row);
                let last = range
                    .end
                    .saturating_sub(current_row)
                    .min(measurement.segment_count);
                for segment in first..last {
                    let rendered = render_segment(line.bytes, self.width, segment);
                    lines.push(MaterializedLine {
                        line: TaskFrameLine {
                            kind: line.kind,
                            text: rendered.text,
                            task_id: Some(self.task.id),
                            selected: false,
                            continuation: segment > 0,
                        },
                        anchor: Some(line.location.with_cell_offset(rendered.start_cell)),
                    });
                }
            }
            current_row = line_end;
            current_row < range.end
        });
        lines
    }
}

struct DetailLogicalLine<'a> {
    kind: TaskFrameLineKind,
    bytes: &'a [u8],
    location: DetailLocation,
}

impl<'a> DetailLogicalLine<'a> {
    fn utf8(kind: TaskFrameLineKind, bytes: &'a [u8], location: DetailLocation) -> Self {
        Self {
            kind,
            bytes,
            location,
        }
    }
}

fn closest_anchor_row(layout: &DetailLayout<'_>, anchor: &DetailAnchor) -> usize {
    let DetailAnchor::Output {
        chunk_offset: target_chunk,
        display_byte: target_byte,
        ..
    } = anchor
    else {
        return 0;
    };
    let mut current_row = 0usize;
    let mut closest = 0usize;
    layout.visit(&mut |line| {
        if let DetailLocation::Output {
            chunk_offset,
            display_byte,
        } = line.location
        {
            if chunk_offset == *target_chunk && display_byte <= *target_byte {
                closest = current_row;
            } else if chunk_offset > *target_chunk
                || (chunk_offset == *target_chunk && display_byte > *target_byte)
            {
                return false;
            }
        }
        current_row = current_row
            .saturating_add(measure_wrapped(line.bytes, layout.width, None).segment_count);
        true
    });
    closest
}

struct WrappedMeasurement {
    segment_count: usize,
    anchor_segment: usize,
}

struct RenderedSegment {
    start_cell: usize,
    text: String,
}

fn measure_wrapped(bytes: &[u8], width: usize, anchor_cell: Option<usize>) -> WrappedMeasurement {
    let width = width.max(1);
    let mut output_width = 0usize;
    let mut logical_cell = 0usize;
    let mut segment_count = 1usize;
    let mut anchor_segment = 0usize;
    visit_visible_bytes(bytes, &mut |_, character_width| {
        if output_width > 0 && output_width.saturating_add(character_width) > width {
            output_width = 0;
            segment_count = segment_count.saturating_add(1);
            if anchor_cell.is_some_and(|anchor| logical_cell <= anchor) {
                anchor_segment = segment_count.saturating_sub(1);
            }
        }
        output_width = output_width.saturating_add(character_width);
        logical_cell = logical_cell.saturating_add(character_width);
        true
    });
    WrappedMeasurement {
        segment_count,
        anchor_segment,
    }
}

fn segment_start_cell(bytes: &[u8], width: usize, target_segment: usize) -> usize {
    let width = width.max(1);
    if target_segment == 0 {
        return 0;
    }
    let mut output_width = 0usize;
    let mut logical_cell = 0usize;
    let mut segment = 0usize;
    let mut result = 0usize;
    visit_visible_bytes(bytes, &mut |_, character_width| {
        if output_width > 0 && output_width.saturating_add(character_width) > width {
            output_width = 0;
            segment += 1;
            if segment == target_segment {
                result = logical_cell;
                return false;
            }
        }
        output_width = output_width.saturating_add(character_width);
        logical_cell = logical_cell.saturating_add(character_width);
        true
    });
    result
}

fn render_segment(bytes: &[u8], width: usize, target_segment: usize) -> RenderedSegment {
    let width = width.max(1);
    let mut output = String::new();
    let mut output_width = 0usize;
    let mut logical_cell = 0usize;
    let mut start_cell = 0usize;
    let mut segment = 0usize;
    visit_visible_bytes(bytes, &mut |character, character_width| {
        if output_width > 0 && output_width.saturating_add(character_width) > width {
            output_width = 0;
            segment += 1;
            start_cell = logical_cell;
            if segment > target_segment {
                return false;
            }
        }
        if segment == target_segment {
            output.push(character);
        }
        output_width = output_width.saturating_add(character_width);
        logical_cell = logical_cell.saturating_add(character_width);
        true
    });
    RenderedSegment {
        start_cell,
        text: output,
    }
}

fn visit_visible_bytes(bytes: &[u8], visitor: &mut dyn FnMut(char, usize) -> bool) -> bool {
    let mut index = 0usize;
    let mut column = 0usize;
    let mut consecutive_zero_width = 0usize;
    while index < bytes.len() {
        let (character, consumed) = decode_next(&bytes[index..]);
        index += consumed;
        let visible = control_picture(character).unwrap_or(character);
        if visible == '\t' {
            let spaces = TAB_COLUMNS - column % TAB_COLUMNS;
            for _ in 0..spaces {
                if !visitor(' ', 1) {
                    return false;
                }
            }
            column += spaces;
            continue;
        }
        let width = visible.width().unwrap_or(0);
        let exceeds_zero_width_limit = if width == 0 {
            consecutive_zero_width += 1;
            consecutive_zero_width > MAX_CONSECUTIVE_ZERO_WIDTH_CHARACTERS
        } else {
            consecutive_zero_width = 0;
            false
        };
        if exceeds_zero_width_limit {
            for escaped in visible.escape_unicode() {
                let escaped_width = escaped.width().unwrap_or(0);
                if !visitor(escaped, escaped_width) {
                    return false;
                }
                column += escaped_width;
            }
        } else {
            if !visitor(visible, width) {
                return false;
            }
            column += width;
        }
    }
    true
}

fn decode_next(bytes: &[u8]) -> (char, usize) {
    let maximum = bytes.len().min(4);
    for length in 1..=maximum {
        if let Ok(text) = std::str::from_utf8(&bytes[..length])
            && let Some(character) = text.chars().next()
            && character.len_utf8() == length
        {
            return (character, length);
        }
    }
    ('�', 1)
}

fn control_picture(character: char) -> Option<char> {
    match character as u32 {
        0x09 => None,
        0x00..=0x08 | 0x0b..=0x1f => char::from_u32(0x2400 + character as u32),
        0x7f => Some('␡'),
        0x80..=0x9f => Some('�'),
        _ if character.is_control() => Some('�'),
        _ => None,
    }
}

fn fit_visible(text: &str, width: usize) -> String {
    fit_visible_fragments(&[text.as_bytes()], width)
}

fn fit_visible_fragments(fragments: &[&[u8]], width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let mut output = String::new();
    let mut cells = 0usize;
    let mut truncated = false;
    for part in fragments {
        if !visit_visible_bytes(part, &mut |character, character_width| {
            if cells.saturating_add(character_width) > width {
                truncated = true;
                return false;
            }
            output.push(character);
            cells = cells.saturating_add(character_width);
            true
        }) {
            break;
        }
    }
    if truncated {
        let ellipsis_width = '…'.width().unwrap_or(1);
        while cells.saturating_add(ellipsis_width) > width {
            let Some(character) = output.pop() else {
                break;
            };
            cells = cells.saturating_sub(character.width().unwrap_or(0));
        }
        if ellipsis_width <= width {
            output.push('…');
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::ShellTaskOwner;

    fn viewport(width: usize, height: usize) -> TaskViewport {
        TaskViewport { width, height }
    }

    fn task(id: u128, command: &str, status: TaskStatus, bytes: u64) -> ShellTaskView {
        let origin = if id.is_multiple_of(2) {
            TaskOrigin::User
        } else {
            TaskOrigin::Agent
        };
        ShellTaskView {
            id: Uuid::from_u128(id),
            chat_id: Uuid::from_u128(100),
            origin,
            owner: match origin {
                TaskOrigin::Agent => ShellTaskOwner::Agent {
                    loop_id: Uuid::from_u128(id + 100),
                },
                TaskOrigin::User => ShellTaskOwner::User {
                    chat_id: Uuid::from_u128(100),
                },
            },
            command: command.into(),
            cwd: "/tmp/work".into(),
            state: status,
            pid: None,
            output_bytes: bytes,
            exit_code: None,
            signal: None,
            reason: None,
            was_background: false,
            capabilities: TaskCapabilities {
                can_background: status == TaskStatus::Running,
                can_stop: matches!(
                    status,
                    TaskStatus::Queued | TaskStatus::Running | TaskStatus::Background
                ),
                can_read_output: true,
            },
        }
    }

    fn snapshot(revision: u64, tasks: Vec<ShellTaskView>) -> TaskSurfaceSnapshot {
        TaskSurfaceSnapshot {
            chat_id: Uuid::from_u128(100),
            live_revision: revision,
            tasks,
        }
    }

    fn output_chunk(
        task_id: u128,
        offset: u64,
        raw: &[u8],
        total_bytes: u64,
        next_offset: Option<u64>,
    ) -> TaskOutputChunk {
        TaskOutputChunk {
            task_id: Uuid::from_u128(task_id),
            offset,
            confirmed_offset: next_offset.unwrap_or(total_bytes),
            total_bytes,
            next_offset,
            truncated: next_offset.is_some(),
            encoding: "utf-8-lossy".into(),
            output: String::from_utf8_lossy(raw).into_owned(),
        }
    }

    fn deliver_output(surface: &mut TaskSurface, chunk: TaskOutputChunk) -> TaskSurfaceEffect {
        surface.update_output(TaskOutputUpdate::Chunk {
            generation: surface.navigation_generation,
            chunk,
        })
    }

    #[test]
    fn zero_and_multiple_tasks_open_list_while_one_opens_detail_with_a_latent_list() {
        let mut empty = TaskSurface::open(snapshot(1, Vec::new()), "restore");
        let frame = empty.frame(viewport(80, 4));
        assert_eq!(frame.lines[0].kind, TaskFrameLineKind::Empty);
        assert_eq!(frame.selected_task_id, None);
        assert_eq!(
            empty.apply(TaskSurfaceAction::Open, viewport(80, 4)),
            TaskSurfaceEffect::None
        );

        let mut one = TaskSurface::open(
            snapshot(1, vec![task(1, "printf one", TaskStatus::Running, 0)]),
            "restore",
        );
        assert_eq!(
            one.focus(),
            TaskSurfaceFocus::Detail {
                task_id: Uuid::from_u128(1)
            }
        );
        let effect = one.initial_effect();
        assert!(
            matches!(effect, TaskSurfaceEffect::ReadOutput { task_id, .. } if task_id == Uuid::from_u128(1))
        );
        assert_eq!(
            one.apply(TaskSurfaceAction::Back, viewport(80, 4)),
            TaskSurfaceEffect::None
        );
        assert_eq!(one.focus(), TaskSurfaceFocus::List);
        assert_eq!(
            one.frame(viewport(80, 4)).selected_task_id,
            Some(Uuid::from_u128(1))
        );
        one.apply(TaskSurfaceAction::Open, viewport(80, 4));
        assert_eq!(
            one.focus(),
            TaskSurfaceFocus::Detail {
                task_id: Uuid::from_u128(1)
            }
        );

        let mut many = TaskSurface::open(
            snapshot(
                1,
                vec![
                    task(1, "one", TaskStatus::Running, 0),
                    task(2, "two", TaskStatus::Completed, 0),
                    task(3, "three", TaskStatus::Failed, 0),
                ],
            ),
            "restore",
        );
        many.apply(TaskSurfaceAction::Next, viewport(80, 2));
        let frame = many.frame(viewport(160, 2));
        assert_eq!(frame.selected_task_id, Some(Uuid::from_u128(2)));
        assert!(frame.lines[0].text.contains("0 B"));
        assert!(frame.lines[0].text.contains("/tmp/work"));
    }

    #[test]
    fn selection_tracks_task_id_across_reorder_and_nearest_neighbor_after_removal() {
        let mut surface = TaskSurface::open(
            snapshot(
                1,
                vec![
                    task(1, "one", TaskStatus::Running, 0),
                    task(2, "two", TaskStatus::Running, 0),
                    task(3, "three", TaskStatus::Running, 0),
                ],
            ),
            (),
        );
        surface.apply(TaskSurfaceAction::Next, viewport(80, 2));
        surface.update_snapshot(snapshot(
            2,
            vec![
                task(3, "three", TaskStatus::Running, 0),
                task(2, "two", TaskStatus::Running, 0),
                task(1, "one", TaskStatus::Running, 0),
            ],
        ));
        assert_eq!(
            surface.frame(viewport(80, 2)).selected_task_id,
            Some(Uuid::from_u128(2))
        );

        surface.update_snapshot(snapshot(
            3,
            vec![
                task(3, "three", TaskStatus::Running, 0),
                task(1, "one", TaskStatus::Running, 0),
            ],
        ));
        let frame = surface.frame(viewport(80, 2));
        assert_eq!(frame.selected_task_id, Some(Uuid::from_u128(1)));
        assert_eq!(frame.unavailable_task_id, Some(Uuid::from_u128(2)));
        assert_eq!(frame.lines[0].kind, TaskFrameLineKind::Unavailable);
    }

    #[test]
    fn disappearing_detail_stays_unavailable_instead_of_retargeting_actions() {
        let mut surface = TaskSurface::open(
            snapshot(
                1,
                vec![
                    task(1, "one", TaskStatus::Running, 0),
                    task(2, "two", TaskStatus::Running, 0),
                ],
            ),
            (),
        );
        surface.apply(TaskSurfaceAction::Open, viewport(80, 10));
        surface.update_snapshot(snapshot(2, vec![task(2, "two", TaskStatus::Running, 0)]));
        let frame = surface.frame(viewport(80, 10));
        assert_eq!(
            frame.focus,
            TaskSurfaceFocus::Detail {
                task_id: Uuid::from_u128(1)
            }
        );
        assert!(
            frame
                .lines
                .iter()
                .any(|line| line.kind == TaskFrameLineKind::Unavailable)
        );
        assert_eq!(
            surface.apply(TaskSurfaceAction::Stop, viewport(80, 10)),
            TaskSurfaceEffect::None
        );

        surface.update_snapshot(snapshot(
            3,
            vec![
                task(2, "two", TaskStatus::Running, 0),
                task(1, "one", TaskStatus::Background, 0),
            ],
        ));
        let restored = surface.frame(viewport(80, 10));
        assert_eq!(restored.unavailable_task_id, None);
        assert_eq!(restored.selected_task_id, Some(Uuid::from_u128(1)));
        assert_eq!(
            surface.apply(TaskSurfaceAction::Stop, viewport(80, 10)),
            TaskSurfaceEffect::Control {
                task_id: Uuid::from_u128(1),
                action: TaskControlAction::Stop,
            }
        );
    }

    #[test]
    fn control_effects_preserve_task_id_and_capabilities() {
        let mut surface = TaskSurface::open(
            snapshot(1, vec![task(1, "one", TaskStatus::Running, 0)]),
            (),
        );
        assert_eq!(
            surface.apply(TaskSurfaceAction::Stop, viewport(80, 10)),
            TaskSurfaceEffect::Control {
                task_id: Uuid::from_u128(1),
                action: TaskControlAction::Stop
            }
        );
        surface.update_snapshot(snapshot(2, vec![task(1, "one", TaskStatus::Completed, 0)]));
        assert_eq!(
            surface.apply(TaskSurfaceAction::Stop, viewport(80, 10)),
            TaskSurfaceEffect::None
        );
    }

    #[test]
    fn refreshing_or_reopening_a_scrolled_list_preserves_its_viewport() {
        let tasks = (1..=12)
            .map(|id| task(id, &format!("task {id}"), TaskStatus::Completed, 0))
            .collect::<Vec<_>>();
        let mut surface = TaskSurface::open(snapshot(1, tasks.clone()), ());
        let size = viewport(80, 4);
        for _ in 0..8 {
            surface.apply(TaskSurfaceAction::Next, size);
        }
        for _ in 0..2 {
            surface.apply(TaskSurfaceAction::Previous, size);
        }
        let before = surface.frame(size);
        assert!(before.visible_tasks.start > 0);
        surface.update_snapshot(snapshot(2, tasks.clone()));
        let refreshed = surface.frame(size);
        assert_eq!(refreshed.visible_tasks, before.visible_tasks);
        assert_eq!(refreshed.selected_task_id, before.selected_task_id);
        surface.apply(TaskSurfaceAction::Close, size);
        surface.reopen(snapshot(3, tasks));
        let reopened = surface.frame(size);
        assert_eq!(reopened.visible_tasks, before.visible_tasks);
        assert_eq!(reopened.selected_task_id, before.selected_task_id);
    }

    #[test]
    fn list_frame_exposes_the_selected_tasks_capabilities() {
        let mut surface = TaskSurface::open(
            snapshot(
                1,
                vec![
                    task(1, "one", TaskStatus::Running, 0),
                    task(2, "two", TaskStatus::Completed, 0),
                ],
            ),
            (),
        );

        let running = surface.frame(viewport(160, 10));
        assert_eq!(
            running.selected_capabilities,
            TaskCapabilities {
                can_background: true,
                can_stop: true,
                can_read_output: true,
            }
        );
        surface.apply(TaskSurfaceAction::Next, viewport(160, 10));
        let completed = surface.frame(viewport(160, 10));
        assert_eq!(
            completed.selected_capabilities,
            TaskCapabilities {
                can_background: false,
                can_stop: false,
                can_read_output: true,
            }
        );
    }

    #[test]
    fn scrolling_down_at_the_followed_tail_keeps_follow_enabled() {
        let output = b"one\ntwo\nthree\nfour\n";
        let mut surface = TaskSurface::open(
            snapshot(
                1,
                vec![task(1, "one", TaskStatus::Background, output.len() as u64)],
            ),
            (),
        );
        deliver_output(
            &mut surface,
            output_chunk(1, 0, output, output.len() as u64, None),
        );
        let before = surface.frame(viewport(12, 3));
        assert!(before.follow_tail);

        assert_eq!(
            surface.apply(TaskSurfaceAction::ScrollLines(1), viewport(12, 3)),
            TaskSurfaceEffect::None
        );
        let after_line = surface.frame(viewport(12, 3));
        assert!(after_line.follow_tail);
        assert_eq!(after_line.top_row, before.top_row);

        assert_eq!(
            surface.apply(TaskSurfaceAction::Page(1), viewport(12, 3)),
            TaskSurfaceEffect::None
        );
        let after_page = surface.frame(viewport(12, 3));
        assert!(after_page.follow_tail);
        assert_eq!(after_page.top_row, before.top_row);
    }

    #[test]
    fn upward_navigation_before_output_load_replaces_the_pending_read() {
        for action in [
            TaskSurfaceAction::ScrollLines(-1),
            TaskSurfaceAction::Page(-1),
        ] {
            let mut surface = TaskSurface::open(
                snapshot(1, vec![task(1, "one", TaskStatus::Background, 8)]),
                (),
            );
            let initial = surface.initial_effect();
            let effect = surface.apply(action, viewport(80, 22));
            let TaskSurfaceEffect::ReadOutput {
                generation, cursor, ..
            } = effect
            else {
                panic!("expected replacement read, got {effect:?}");
            };
            assert_eq!(cursor, 0);
            assert_ne!(effect, initial);
            surface.update_output(TaskOutputUpdate::Chunk {
                generation,
                chunk: output_chunk(1, 0, b"accepted", 8, None),
            });
            let frame = surface.frame(viewport(80, 22));
            assert!(!frame.follow_tail);
            assert!(
                frame
                    .lines
                    .iter()
                    .any(|line| line.text.contains("accepted"))
            );
            assert!(
                !frame
                    .lines
                    .iter()
                    .any(|line| line.text.contains("Output not loaded"))
            );
        }
    }

    #[test]
    fn repeated_pending_tail_navigation_survives_frames_resize_and_stale_output() {
        let total_bytes = 200_000_u64;
        let tail_offset = total_bytes - OUTPUT_READ_LIMIT as u64;
        let tail = (0..1024)
            .flat_map(|_| std::iter::repeat_n(b'x', 63).chain(std::iter::once(b'\n')))
            .collect::<Vec<_>>();
        assert_eq!(tail.len(), OUTPUT_READ_LIMIT);
        let initial_viewport = viewport(40, 5);
        let mut surface = TaskSurface::open(
            snapshot(
                1,
                vec![task(1, "large output", TaskStatus::Background, total_bytes)],
            ),
            (),
        );
        let mut generations = Vec::new();
        for action in [
            TaskSurfaceAction::ScrollLines(-1),
            TaskSurfaceAction::ScrollLines(-1),
            TaskSurfaceAction::Page(-1),
            TaskSurfaceAction::Page(-1),
        ] {
            let effect = surface.apply(action, initial_viewport);
            let TaskSurfaceEffect::ReadOutput {
                cursor, generation, ..
            } = effect
            else {
                panic!("expected replacement read, got {effect:?}");
            };
            assert_eq!(cursor, tail_offset);
            generations.push(generation);
            surface.frame(initial_viewport);
        }
        assert!(matches!(
            surface.apply(TaskSurfaceAction::Stop, initial_viewport),
            TaskSurfaceEffect::Control {
                action: TaskControlAction::Stop,
                ..
            }
        ));
        assert_eq!(surface.navigation_generation, *generations.last().unwrap());
        surface.frame(initial_viewport);

        assert_eq!(
            surface.update_output(TaskOutputUpdate::Chunk {
                generation: generations[0],
                chunk: output_chunk(1, tail_offset, b"stale", total_bytes, None),
            }),
            TaskSurfaceEffect::None
        );
        assert!(
            surface
                .detail_states
                .get(&Uuid::from_u128(1))
                .unwrap()
                .output
                .is_none()
        );

        let resized_viewport = viewport(20, 7);
        surface.frame(resized_viewport);
        surface.update_output(TaskOutputUpdate::Chunk {
            generation: *generations.last().unwrap(),
            chunk: output_chunk(1, tail_offset, &tail, total_bytes, None),
        });
        let frame = surface.frame(resized_viewport);
        let maximum_top = frame.total_rows.saturating_sub(resized_viewport.height);
        let requested_rows_before_end = 2 + initial_viewport.height * 2;
        assert_eq!(
            frame.top_row,
            maximum_top.saturating_sub(requested_rows_before_end)
        );
        assert!(!frame.follow_tail);
    }

    #[test]
    fn task_controls_do_not_invalidate_a_pending_output_read() {
        for action in [TaskSurfaceAction::Background, TaskSurfaceAction::Stop] {
            let mut surface = TaskSurface::open(
                snapshot(1, vec![task(1, "one", TaskStatus::Running, 8)]),
                (),
            );
            let TaskSurfaceEffect::ReadOutput { generation, .. } = surface.initial_effect() else {
                unreachable!()
            };
            assert!(matches!(
                surface.apply(action, viewport(80, 22)),
                TaskSurfaceEffect::Control { .. }
            ));
            surface.update_output(TaskOutputUpdate::Chunk {
                generation,
                chunk: output_chunk(1, 0, b"accepted", 8, None),
            });
            assert!(
                surface
                    .frame(viewport(80, 22))
                    .lines
                    .iter()
                    .any(|line| line.text.contains("accepted"))
            );
        }
    }

    #[test]
    fn no_op_tail_scroll_does_not_cancel_the_initial_output_read() {
        let mut surface = TaskSurface::open(
            snapshot(1, vec![task(1, "one", TaskStatus::Background, 8)]),
            (),
        );
        let initial = surface.initial_effect();
        let initial_generation = match initial {
            TaskSurfaceEffect::ReadOutput { generation, .. } => generation,
            other => panic!("expected initial output read, got {other:?}"),
        };

        assert_eq!(
            surface.apply(TaskSurfaceAction::ScrollLines(1), viewport(20, 4)),
            TaskSurfaceEffect::None
        );
        assert_eq!(surface.navigation_generation, initial_generation);
        assert_eq!(
            surface.update_output(TaskOutputUpdate::Chunk {
                generation: initial_generation,
                chunk: output_chunk(1, 0, b"accepted", 8, None),
            }),
            TaskSurfaceEffect::None
        );
        let detail = surface.detail_states.get(&Uuid::from_u128(1)).unwrap();
        assert_eq!(detail.output.as_ref().unwrap().bytes, b"accepted");
    }

    #[test]
    fn tail_follow_and_paused_unread_are_independent_per_task() {
        let mut surface = TaskSurface::open(
            snapshot(1, vec![task(1, "one", TaskStatus::Background, 12)]),
            (),
        );
        surface.apply(TaskSurfaceAction::Open, viewport(12, 3));
        deliver_output(
            &mut surface,
            output_chunk(1, 0, b"one\ntwo\nthree\n", 14, None),
        );
        let tail = surface.frame(viewport(12, 3));
        assert!(tail.follow_tail);
        assert_eq!(tail.unread_bytes, 0);

        surface.apply(TaskSurfaceAction::ScrollLines(-1), viewport(12, 3));
        surface.update_snapshot(snapshot(
            2,
            vec![task(1, "one", TaskStatus::Background, 24)],
        ));
        let paused = surface.frame(viewport(12, 3));
        assert!(!paused.follow_tail);
        assert_eq!(paused.unread_bytes, 10);

        let effect = surface.apply(TaskSurfaceAction::End, viewport(12, 3));
        assert!(matches!(effect, TaskSurfaceEffect::ReadOutput { cursor, .. } if cursor == 0));
        let resumed = surface.frame(viewport(12, 3));
        assert!(resumed.follow_tail);
        assert_eq!(resumed.unread_bytes, 0);
    }

    #[test]
    fn visited_detail_accumulates_unread_while_list_or_surface_is_hidden() {
        let mut surface = TaskSurface::open(
            snapshot(
                1,
                vec![
                    task(1, "one", TaskStatus::Background, 10),
                    task(2, "two", TaskStatus::Background, 20),
                ],
            ),
            (),
        );
        surface.apply(TaskSurfaceAction::Open, viewport(40, 10));
        surface.apply(TaskSurfaceAction::Back, viewport(40, 10));
        surface.update_snapshot(snapshot(
            2,
            vec![
                task(1, "one", TaskStatus::Background, 15),
                task(2, "two", TaskStatus::Background, 20),
            ],
        ));
        let detail = surface.detail_states.get(&Uuid::from_u128(1)).unwrap();
        assert_eq!(detail.unread_bytes, 5);
        assert!(!detail.navigation.follow_tail);

        surface.update_snapshot_hidden(snapshot(
            3,
            vec![
                task(1, "one", TaskStatus::Background, 22),
                task(2, "two", TaskStatus::Background, 20),
            ],
        ));
        let detail = surface.detail_states.get(&Uuid::from_u128(1)).unwrap();
        assert_eq!(detail.unread_bytes, 12);
        assert!(!detail.navigation.follow_tail);
    }

    #[test]
    fn reopen_with_loaded_output_preserves_anchor_and_records_unseen_growth() {
        let output = b"one\ntwo\nthree\nfour\nfive\nsix\n";
        let mut surface = TaskSurface::open(
            snapshot(
                1,
                vec![task(1, "one", TaskStatus::Background, output.len() as u64)],
            ),
            (),
        );
        deliver_output(
            &mut surface,
            output_chunk(1, 0, output, output.len() as u64, None),
        );
        surface.apply(TaskSurfaceAction::ScrollLines(-1), viewport(12, 3));
        let before = surface.frame(viewport(12, 3));
        assert!(!before.follow_tail);

        let growth = 9;
        let mut updated = task(
            1,
            "one",
            TaskStatus::Background,
            output.len() as u64 + growth,
        );
        updated.was_background = true;
        assert_eq!(
            surface.reopen(snapshot(2, vec![updated])),
            TaskSurfaceEffect::None
        );

        let reopened = surface.frame(viewport(12, 3));
        assert_eq!(reopened.top_row, before.top_row);
        assert_eq!(reopened.unread_bytes, growth);
        assert!(!reopened.follow_tail);
        let expanded = surface.frame(viewport(160, 20));
        assert!(expanded.lines.iter().any(|line| {
            line.text
                .contains(&format!("Output: {} bytes", output.len() as u64 + growth))
        }));
    }

    #[test]
    fn action_errors_are_visible_without_changing_task_selection() {
        let mut surface = TaskSurface::open(
            snapshot(1, vec![task(1, "one", TaskStatus::Running, 0)]),
            (),
        );
        surface.report_action_error(Uuid::from_u128(1), "owner mismatch".into());
        let frame = surface.frame(viewport(80, 10));
        assert_eq!(frame.selected_task_id, Some(Uuid::from_u128(1)));
        assert_eq!(frame.action_error.as_deref(), Some("one: owner mismatch"));
        surface.clear_action_error();
        assert!(surface.frame(viewport(80, 10)).action_error.is_none());
    }

    #[test]
    fn detail_anchor_survives_resize_and_frame_memory_is_viewport_bounded() {
        let mut surface = TaskSurface::open(
            snapshot(
                1,
                vec![task(1, "one", TaskStatus::Completed, 8 * 1024 * 1024)],
            ),
            (),
        );
        surface.apply(TaskSurfaceAction::Open, viewport(80, 5));
        deliver_output(
            &mut surface,
            output_chunk(1, 0, &vec![b'x'; 8 * 1024 * 1024], 8 * 1024 * 1024, None),
        );
        surface.apply(TaskSurfaceAction::Home, viewport(80, 5));
        surface.apply(TaskSurfaceAction::Page(1), viewport(80, 5));
        let before = surface.frame(viewport(80, 5));
        let resized = surface.frame(viewport(7, 3));
        assert_eq!(before.top_row, 5);
        assert_eq!(resized.lines.len(), 3);
        assert!(resized.total_rows > before.total_rows);
        let stored = surface.detail_states.get(&Uuid::from_u128(1)).unwrap();
        assert!(stored.output.as_ref().unwrap().bytes.len() <= OUTPUT_READ_LIMIT * 3);
    }

    #[test]
    fn paging_uses_artifact_offsets_and_can_move_backward_forward_and_to_tail() {
        let total_bytes = 200_000;
        let mut surface = TaskSurface::open(
            snapshot(
                1,
                vec![task(1, "long output", TaskStatus::Background, total_bytes)],
            ),
            (),
        );
        let lossy_page = vec![0xff; OUTPUT_READ_LIMIT];
        deliver_output(
            &mut surface,
            output_chunk(1, 131_072, &lossy_page, total_bytes, Some(196_608)),
        );

        let previous = surface.apply(TaskSurfaceAction::Home, viewport(20, 4));
        assert_eq!(
            previous,
            TaskSurfaceEffect::ReadOutput {
                task_id: Uuid::from_u128(1),
                cursor: 0,
                limit: OUTPUT_READ_LIMIT,
                generation: 1,
            }
        );
        deliver_output(
            &mut surface,
            output_chunk(1, 65_536, &lossy_page, total_bytes, Some(131_072)),
        );
        let forward = surface.apply(TaskSurfaceAction::Page(50_000), viewport(20, 4));
        assert_eq!(
            forward,
            TaskSurfaceEffect::ReadOutput {
                task_id: Uuid::from_u128(1),
                cursor: 131_072,
                limit: OUTPUT_READ_LIMIT,
                generation: 2,
            }
        );
        let tail = surface.apply(TaskSurfaceAction::End, viewport(20, 4));
        assert_eq!(
            tail,
            TaskSurfaceEffect::ReadOutput {
                task_id: Uuid::from_u128(1),
                cursor: total_bytes - OUTPUT_READ_LIMIT as u64,
                limit: OUTPUT_READ_LIMIT,
                generation: 3,
            }
        );
    }

    #[test]
    fn paging_places_previous_window_at_its_end_and_next_window_at_its_start() {
        let total_bytes = (OUTPUT_READ_LIMIT * 3) as u64;
        let page = (0..(OUTPUT_READ_LIMIT / 2))
            .flat_map(|_| [b'x', b'\n'])
            .collect::<Vec<_>>();
        let viewport = viewport(40, 5);
        let mut surface = TaskSurface::open(
            snapshot(
                1,
                vec![task(1, "paged output", TaskStatus::Background, total_bytes)],
            ),
            (),
        );
        deliver_output(
            &mut surface,
            output_chunk(
                1,
                OUTPUT_READ_LIMIT as u64,
                &page,
                total_bytes,
                Some((OUTPUT_READ_LIMIT * 2) as u64),
            ),
        );

        let previous = surface.apply(TaskSurfaceAction::Page(-50_000), viewport);
        let TaskSurfaceEffect::ReadOutput {
            cursor, generation, ..
        } = previous
        else {
            panic!("expected previous-page read, got {previous:?}");
        };
        assert_eq!(cursor, 0);
        surface.frame(viewport);
        surface.update_output(TaskOutputUpdate::Chunk {
            generation,
            chunk: output_chunk(1, 0, &page, total_bytes, Some(OUTPUT_READ_LIMIT as u64)),
        });
        let previous_frame = surface.frame(viewport);
        assert_eq!(
            previous_frame.top_row,
            previous_frame.total_rows.saturating_sub(viewport.height)
        );

        let next = surface.apply(TaskSurfaceAction::Page(50_000), viewport);
        let TaskSurfaceEffect::ReadOutput {
            cursor, generation, ..
        } = next
        else {
            panic!("expected next-page read, got {next:?}");
        };
        assert_eq!(cursor, OUTPUT_READ_LIMIT as u64);
        surface.frame(viewport);
        surface.update_output(TaskOutputUpdate::Chunk {
            generation,
            chunk: output_chunk(
                1,
                OUTPUT_READ_LIMIT as u64,
                &page,
                total_bytes,
                Some((OUTPUT_READ_LIMIT * 2) as u64),
            ),
        });
        let next_frame = surface.frame(viewport);
        assert_eq!(next_frame.lines[0].kind, TaskFrameLineKind::Output);
        assert_eq!(next_frame.lines[0].text, "x");
    }

    #[test]
    fn reopening_resumes_the_requested_page_instead_of_replacing_it_with_the_tail() {
        for (has_loaded_tail, via_list) in
            [(false, false), (true, false), (false, true), (true, true)]
        {
            let total = OUTPUT_READ_LIMIT as u64 + 8;
            let snapshot = snapshot(
                1,
                vec![task(1, "large output", TaskStatus::Completed, total)],
            );
            let mut surface = TaskSurface::open(snapshot.clone(), ());
            if has_loaded_tail {
                deliver_output(
                    &mut surface,
                    output_chunk(1, OUTPUT_READ_LIMIT as u64, b"tail-end", total, None),
                );
            }
            let initial = surface.apply(TaskSurfaceAction::Home, viewport(80, 22));
            surface.frame(viewport(80, 22));
            let resumed = if via_list {
                surface.apply(TaskSurfaceAction::Back, viewport(80, 22));
                surface.frame(viewport(80, 22));
                surface.apply(TaskSurfaceAction::Open, viewport(80, 22))
            } else {
                surface.apply(TaskSurfaceAction::Close, viewport(80, 22));
                surface.reopen(snapshot)
            };
            let TaskSurfaceEffect::ReadOutput {
                cursor, generation, ..
            } = resumed
            else {
                panic!("reopen must resume pending read")
            };
            assert_eq!(cursor, 0);
            assert_ne!(initial, resumed);
            surface.update_output(TaskOutputUpdate::Chunk {
                generation,
                chunk: output_chunk(1, 0, b"first-page\n", total, Some(11)),
            });
            let frame = surface.frame(viewport(80, 22));
            assert_eq!(frame.top_row, 0);
            assert!(
                frame
                    .lines
                    .iter()
                    .any(|line| line.text.contains("first-page"))
            );
        }
    }

    #[test]
    fn home_keeps_metadata_visible_after_the_first_output_page_arrives() {
        for has_loaded_tail in [false, true] {
            let total = OUTPUT_READ_LIMIT as u64 + 8;
            let mut surface = TaskSurface::open(
                snapshot(
                    1,
                    vec![task(1, "large output", TaskStatus::Completed, total)],
                ),
                (),
            );
            if has_loaded_tail {
                deliver_output(
                    &mut surface,
                    output_chunk(1, OUTPUT_READ_LIMIT as u64, b"tail-end", total, None),
                );
            }
            let TaskSurfaceEffect::ReadOutput {
                generation, cursor, ..
            } = surface.apply(TaskSurfaceAction::Home, viewport(80, 22))
            else {
                panic!("Home must load the first page")
            };
            assert_eq!(cursor, 0);
            let pending = surface.frame(viewport(80, 22));
            assert_eq!(pending.top_row, 0);
            surface.update_output(TaskOutputUpdate::Chunk {
                generation,
                chunk: output_chunk(1, 0, b"first-page\n", total, Some(11)),
            });
            let frame = surface.frame(viewport(80, 22));
            assert_eq!(frame.top_row, 0);
            assert!(
                frame
                    .lines
                    .iter()
                    .any(|line| line.text.contains("Status: completed"))
            );
            assert!(
                frame
                    .lines
                    .iter()
                    .any(|line| line.text.contains("first-page"))
            );
        }
    }

    #[test]
    fn stale_output_does_not_replace_a_new_navigation_request() {
        let mut surface = TaskSurface::open(
            snapshot(
                1,
                vec![task(1, "growing output", TaskStatus::Background, 8)],
            ),
            (),
        );
        let stale_generation = surface.navigation_generation;
        let replacement = surface.apply(TaskSurfaceAction::Home, viewport(20, 4));
        assert!(matches!(
            replacement,
            TaskSurfaceEffect::ReadOutput {
                cursor: 0,
                generation,
                ..
            } if generation != stale_generation
        ));
        let effect = surface.update_output(TaskOutputUpdate::Chunk {
            generation: stale_generation,
            chunk: output_chunk(1, 0, b"stale", 5, None),
        });
        assert_eq!(effect, TaskSurfaceEffect::None);
        assert!(
            surface
                .detail_states
                .get(&Uuid::from_u128(1))
                .is_none_or(|detail| detail.output.is_none())
        );
    }

    #[test]
    fn tail_follow_retargets_when_output_grows_during_a_read() {
        let mut surface = TaskSurface::open(
            snapshot(
                1,
                vec![task(1, "growing output", TaskStatus::Background, 8)],
            ),
            (),
        );
        let effect = deliver_output(
            &mut surface,
            output_chunk(
                1,
                0,
                &vec![b'x'; OUTPUT_READ_LIMIT],
                (OUTPUT_READ_LIMIT * 2) as u64,
                Some(OUTPUT_READ_LIMIT as u64),
            ),
        );
        assert!(matches!(
            effect,
            TaskSurfaceEffect::ReadOutput { cursor, .. }
                if cursor == OUTPUT_READ_LIMIT as u64
        ));
        let effect = deliver_output(
            &mut surface,
            output_chunk(
                1,
                OUTPUT_READ_LIMIT as u64,
                &vec![b'y'; OUTPUT_READ_LIMIT],
                (OUTPUT_READ_LIMIT * 2) as u64,
                None,
            ),
        );
        assert_eq!(effect, TaskSurfaceEffect::None);
    }

    #[test]
    fn output_escapes_controls_tabs_and_invalid_utf8_without_ansi() {
        let mut surface = TaskSurface::open(
            snapshot(1, vec![task(1, "one", TaskStatus::Completed, 9)]),
            (),
        );
        surface.apply(TaskSurfaceAction::Open, viewport(40, 20));
        deliver_output(
            &mut surface,
            output_chunk(1, 0, b"a\tb\x1b]x\x07\xff\n\xe4\xb8\xad", 12, None),
        );
        surface.apply(TaskSurfaceAction::Home, viewport(40, 20));
        let frame = surface.frame(viewport(40, 20));
        let output = frame
            .lines
            .iter()
            .filter(|line| line.kind == TaskFrameLineKind::Output)
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(output.contains("a   b␛]x␇�"), "{output:?}");
        assert!(output.contains('中'));
        assert!(!output.contains('\x1b'));
    }

    #[test]
    fn lossy_utf8_expansion_keeps_the_complete_raw_output_window() {
        let mut surface = TaskSurface::open(
            snapshot(
                1,
                vec![task(
                    1,
                    "invalid bytes",
                    TaskStatus::Completed,
                    OUTPUT_READ_LIMIT as u64,
                )],
            ),
            (),
        );
        let raw = vec![0xff; OUTPUT_READ_LIMIT];
        deliver_output(
            &mut surface,
            output_chunk(1, 0, &raw, OUTPUT_READ_LIMIT as u64, None),
        );
        let stored = surface
            .detail_states
            .get(&Uuid::from_u128(1))
            .and_then(|detail| detail.output.as_ref())
            .unwrap();
        assert_eq!(stored.bytes.len(), OUTPUT_DISPLAY_MAX_BYTES);
        assert_eq!(stored.confirmed_offset, OUTPUT_READ_LIMIT as u64);
    }

    #[test]
    fn unavailable_output_and_tiny_viewports_are_explicit_and_safe() {
        let mut surface = TaskSurface::open(
            snapshot(1, vec![task(1, "\nlong\tcommand", TaskStatus::Unknown, 5)]),
            (),
        );
        assert!(surface.frame(viewport(0, 0)).lines.is_empty());
        assert_eq!(surface.frame(viewport(1, 1)).lines.len(), 1);
        surface.apply(TaskSurfaceAction::Open, viewport(1, 1));
        surface.update_output(TaskOutputUpdate::Unavailable {
            generation: surface.navigation_generation,
            task_id: Uuid::from_u128(1),
            total_bytes: 5,
            message: "artifact\x1b missing".into(),
        });
        surface.apply(TaskSurfaceAction::End, viewport(1, 1));
        let frame = surface.frame(viewport(1, 1));
        assert_eq!(frame.lines.len(), 1);
        assert!(
            frame.lines[0]
                .text
                .chars()
                .all(|character| !character.is_control())
        );
    }

    #[test]
    fn stale_snapshots_do_not_rewind_selection_or_output_counts() {
        let mut surface = TaskSurface::open(
            snapshot(2, vec![task(1, "one", TaskStatus::Running, 20)]),
            (),
        );
        surface.apply(TaskSurfaceAction::Open, viewport(80, 10));
        surface.update_snapshot(snapshot(1, vec![task(2, "stale", TaskStatus::Running, 1)]));
        assert_eq!(
            surface.focus(),
            TaskSurfaceFocus::Detail {
                task_id: Uuid::from_u128(1)
            }
        );
        assert_eq!(
            surface.frame(viewport(80, 10)).selected_task_id,
            Some(Uuid::from_u128(1))
        );
    }

    #[test]
    fn close_preserves_the_opaque_chat_restore_token() {
        #[derive(Debug, Eq, PartialEq)]
        struct Restore {
            draft: String,
            cursor: usize,
            follow: bool,
        }
        let restore = Restore {
            draft: "draft".into(),
            cursor: 3,
            follow: false,
        };
        let mut surface = TaskSurface::open(snapshot(1, Vec::new()), restore);
        assert_eq!(
            surface.apply(TaskSurfaceAction::Close, viewport(80, 10)),
            TaskSurfaceEffect::Close
        );
        assert_eq!(
            surface.restore(),
            &Restore {
                draft: "draft".into(),
                cursor: 3,
                follow: false
            }
        );
    }
}
