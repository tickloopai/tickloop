use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use uuid::Uuid;

use super::{
    monitor::relative_time,
    style::{Role, RoleLine},
    task_surface::{
        TaskFrameLineKind, TaskOutputUpdate, TaskSurface, TaskSurfaceAction, TaskSurfaceEffect,
        TaskViewport,
    },
};
use tickloop_agent_protocol::{ShellTask, ShellTaskAction, ShellTaskSnapshot};

pub(super) struct TickShell {
    pub(super) id: Uuid,
    pub(super) chat: Uuid,
    pub(super) task_id: Uuid,
    task: Option<ShellTask>,
    started_at_ms: Option<u64>,
    surface: Option<TaskSurface>,
    confirm_stop: bool,
    error: Option<String>,
    last_refresh_ms: Option<u64>,
}

pub(super) enum TickShellKey {
    None,
    Back,
    Effect(TaskSurfaceEffect),
}

impl TickShell {
    pub(super) fn loading(chat: Uuid, task_id: Uuid) -> Self {
        Self {
            id: Uuid::new_v4(),
            chat,
            task_id,
            task: None,
            started_at_ms: None,
            surface: None,
            confirm_stop: false,
            error: None,
            last_refresh_ms: None,
        }
    }
    pub(super) fn is_loaded(&self) -> bool {
        self.surface.is_some()
    }
    pub(super) fn should_refresh(&mut self, now: u64) -> bool {
        const REFRESH_INTERVAL_MS: u64 = 250;
        if self
            .last_refresh_ms
            .is_some_and(|last| now.saturating_sub(last) < REFRESH_INTERVAL_MS)
        {
            return false;
        }
        self.last_refresh_ms = Some(now);
        true
    }
    pub(super) fn load(
        &mut self,
        mut snapshot: ShellTaskSnapshot,
        started_at_ms: Option<u64>,
    ) -> Option<TaskSurfaceEffect> {
        snapshot
            .tasks
            .retain(|task| task.id == self.task_id && task.chat_id == self.chat);
        self.task = snapshot.tasks.first().cloned();
        self.task.as_ref()?;
        self.error = None;
        self.started_at_ms = self.started_at_ms.or(started_at_ms);
        if self.surface.is_some() {
            return self.update(snapshot);
        }
        let mut surface = TaskSurface::open(snapshot);
        let effect = surface.initial_effect();
        self.surface = Some(surface);
        Some(effect)
    }
    pub(super) fn update(&mut self, mut snapshot: ShellTaskSnapshot) -> Option<TaskSurfaceEffect> {
        snapshot
            .tasks
            .retain(|task| task.id == self.task_id && task.chat_id == self.chat);
        self.task = snapshot.tasks.first().cloned();
        self.task.as_ref()?;
        self.error = None;
        if !self.can_stop() {
            self.confirm_stop = false;
        }
        Some(
            self.surface
                .as_mut()
                .map_or(TaskSurfaceEffect::None, |surface| {
                    let effect = surface.update_snapshot(snapshot.clone());
                    if effect == TaskSurfaceEffect::None {
                        surface.reopen(snapshot)
                    } else {
                        effect
                    }
                }),
        )
    }
    pub(super) fn output(&mut self, update: TaskOutputUpdate) -> TaskSurfaceEffect {
        if matches!(update, TaskOutputUpdate::Chunk { .. })
            && self
                .surface
                .as_ref()
                .is_some_and(|surface| surface.accepts_output_update(&update))
        {
            self.error = None;
        }
        self.surface
            .as_mut()
            .map_or(TaskSurfaceEffect::None, |surface| {
                surface.update_output(update)
            })
    }
    pub(super) fn report(&mut self, error: String) {
        self.error = Some(error);
    }

    pub(super) fn report_control(&mut self, error: String) {
        if let Some(surface) = &mut self.surface {
            surface.report_action_error(self.task_id, error);
        } else {
            self.report(error);
        }
    }

    pub(super) fn clear_control_error(&mut self) {
        if let Some(surface) = &mut self.surface {
            surface.clear_action_error();
        }
    }
    pub(super) fn is_confirming_stop(&self) -> bool {
        self.confirm_stop
    }

    fn can_stop(&self) -> bool {
        self.task
            .as_ref()
            .is_some_and(|task| task.capabilities.can_stop && !task.state.is_terminal())
    }
    pub(super) fn key(&mut self, key: KeyEvent, width: usize, height: usize) -> TickShellKey {
        if self.confirm_stop {
            match key.code {
                KeyCode::Esc | KeyCode::Left => self.confirm_stop = false,
                KeyCode::Enter => {
                    self.confirm_stop = false;
                    if self.can_stop() {
                        return TickShellKey::Effect(TaskSurfaceEffect::Control {
                            task_id: self.task_id,
                            action: ShellTaskAction::Stop,
                        });
                    }
                }
                _ => {}
            }
            return TickShellKey::None;
        }
        let action = match (key.code, key.modifiers) {
            (KeyCode::Esc | KeyCode::Left, _) => return TickShellKey::Back,
            (KeyCode::Char('s'), KeyModifiers::NONE) if self.can_stop() => {
                self.confirm_stop = true;
                return TickShellKey::None;
            }
            (KeyCode::Up | KeyCode::Char('k'), KeyModifiers::NONE)
            | (KeyCode::Char('p'), KeyModifiers::CONTROL) => TaskSurfaceAction::ScrollLines(-1),
            (KeyCode::Down | KeyCode::Char('j'), KeyModifiers::NONE)
            | (KeyCode::Char('n'), KeyModifiers::CONTROL) => TaskSurfaceAction::ScrollLines(1),
            (KeyCode::PageUp, _) => TaskSurfaceAction::Page(-1),
            (KeyCode::PageDown, _) => TaskSurfaceAction::Page(1),
            (KeyCode::Home, _) => TaskSurfaceAction::Home,
            (KeyCode::End, _) => TaskSurfaceAction::End,
            _ => return TickShellKey::None,
        };
        let Some(surface) = self.surface.as_mut() else {
            return TickShellKey::None;
        };
        let viewport = TaskViewport {
            width,
            height: height.saturating_sub(2),
        };
        let effect = surface.apply(action, viewport);
        let moving_down = matches!(action, TaskSurfaceAction::ScrollLines(lines) | TaskSurfaceAction::Page(lines) if lines > 0);
        let frame = surface.frame(viewport);
        if moving_down
            && !frame.can_move_down
            && !frame.follow_tail
            && effect == TaskSurfaceEffect::None
        {
            return TickShellKey::Effect(surface.apply(TaskSurfaceAction::End, viewport));
        }
        TickShellKey::Effect(effect)
    }
    pub(super) fn lines(&mut self, width: usize, height: usize, now: u64) -> Vec<RoleLine> {
        let mut lines = vec![RoleLine::role("Shell", Role::Text)];
        if let (Some(task), Some(surface)) = (&self.task, &mut self.surface) {
            let mut metadata = vec!["Command".into()];
            metadata.extend(task.command.lines().map(str::to_owned));
            metadata.extend([
                format!("State: {}", task.status_label()),
                format!(
                    "Started {} · updated {}",
                    relative_time(self.started_at_ms, now),
                    relative_time(task.updated_at_ms, now)
                ),
                "Working directory".into(),
                task.cwd.to_string_lossy().into(),
            ]);
            surface.set_detail_metadata(self.task_id, metadata);
            let frame = surface.frame(TaskViewport {
                width,
                height: height.saturating_sub(2),
            });
            lines.extend(frame.lines.into_iter().map(|line| {
                RoleLine::role(
                    line.text,
                    if line.kind == TaskFrameLineKind::Unavailable {
                        Role::Warning
                    } else {
                        Role::MutedText
                    },
                )
            }));
        } else {
            lines.push(RoleLine::role("Loading…", Role::MutedText));
        }
        lines.truncate(height.saturating_sub(1));
        lines.resize_with(height.saturating_sub(1), RoleLine::default);
        if height > 0 {
            let hint = if self.confirm_stop {
                String::new()
            } else {
                self.hint()
            };
            lines.push(RoleLine::role(hint, Role::MutedText));
        }
        for line in &mut lines {
            line.truncate(width);
        }
        lines
    }

    pub(super) fn hint(&self) -> String {
        if self.confirm_stop {
            "Stop this Shell? enter confirm · esc cancel".into()
        } else if let Some(error) = self
            .error
            .as_deref()
            .or_else(|| self.surface.as_ref().and_then(TaskSurface::visible_error))
        {
            format!("{} · esc back", super::safe_line(error))
        } else if self.can_stop() {
            "s stop · esc back".into()
        } else {
            "esc back".into()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickloop_agent_protocol::ShellTaskOutputChunk;
    fn snapshot(
        chat: Uuid,
        id: Uuid,
        revision: u64,
        bytes: usize,
        can_stop: bool,
    ) -> ShellTaskSnapshot {
        serde_json::from_value(serde_json::json!({"chat_id":chat,"live_revision":revision,"tasks":[{"id":id,"chat_id":chat,"origin":"User","owner":{"User":{"chat_id":chat}},"command":format!("printf {}", "好".repeat(24)),"cwd":"/tmp/测试目录","state":"Background","was_background":true,"pid":123,"exit_code":null,"signal":null,"reason":null,"output_bytes":bytes,"updated_at_ms":1000,"capabilities":{"can_background":false,"can_stop":can_stop,"can_read_output":true}}]})).unwrap()
    }
    fn deliver(detail: &mut TickShell, effect: TaskSurfaceEffect, output: &str) {
        let TaskSurfaceEffect::ReadOutput {
            task_id,
            generation,
            ..
        } = effect
        else {
            panic!("expected output read");
        };
        detail.output(TaskOutputUpdate::Chunk {
            generation,
            chunk: ShellTaskOutputChunk {
                task_id,
                offset: 0,
                confirmed_offset: output.len() as u64,
                total_bytes: output.len() as u64,
                next_offset: None,
                truncated: false,
                encoding: "utf-8".into(),
                output: output.into(),
            },
        });
    }
    #[test]
    fn panel_wraps_metadata_output_and_cjk_without_basic_navigation_hints() {
        let chat = Uuid::new_v4();
        let task = Uuid::new_v4();
        let output = format!("{}\nfinal", "界".repeat(30));
        let mut detail = TickShell::loading(chat, task);
        let effect = detail
            .load(snapshot(chat, task, 1, output.len(), true), Some(500))
            .unwrap();
        deliver(&mut detail, effect, &output);
        let lines = detail.lines(40, 30, 2000);
        let text = lines
            .iter()
            .map(RoleLine::text)
            .collect::<Vec<_>>()
            .join("\n");
        for expected in [
            "Command",
            "State: running",
            "Started 1s",
            "updated 1s",
            "Working directory",
            "/tmp/测试目录",
            "final",
            "s stop · esc back",
        ] {
            assert!(text.contains(expected), "{text}");
        }
        assert!(
            lines
                .iter()
                .all(|line| unicode_width::UnicodeWidthStr::width(line.text().as_str()) <= 40)
        );
        assert!(!text.contains("j/k") && !text.contains("background, stop"));
    }
    #[test]
    fn panel_follows_tail_until_scrolled_and_pages_back_to_newest_output() {
        let chat = Uuid::new_v4();
        let task = Uuid::new_v4();
        let first = (0..30)
            .map(|row| format!("line-{row}\n"))
            .collect::<String>();
        let mut detail = TickShell::loading(chat, task);
        let effect = detail
            .load(snapshot(chat, task, 1, first.len(), true), Some(500))
            .unwrap();
        deliver(&mut detail, effect, &first);
        assert!(
            detail
                .lines(40, 10, 2000)
                .iter()
                .any(|line| line.text().contains("line-29"))
        );
        detail.key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE), 40, 10);
        let before = detail
            .lines(40, 10, 2000)
            .iter()
            .map(RoleLine::text)
            .collect::<Vec<_>>();
        let next = format!("{first}newest\n");
        assert_eq!(
            detail.update(snapshot(chat, task, 2, next.len(), true)),
            Some(TaskSurfaceEffect::None)
        );
        assert_eq!(
            before,
            detail
                .lines(40, 10, 2000)
                .iter()
                .map(RoleLine::text)
                .collect::<Vec<_>>()
        );
        let TickShellKey::Effect(effect) =
            detail.key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE), 40, 10)
        else {
            panic!("missing tail read");
        };
        deliver(&mut detail, effect, &next);
        assert!(
            detail
                .lines(40, 10, 2000)
                .iter()
                .any(|line| line.text().contains("newest"))
        );
        let last = format!("{next}live-tail\n");
        let effect = detail
            .update(snapshot(chat, task, 3, last.len(), true))
            .unwrap();
        deliver(&mut detail, effect, &last);
        assert!(
            detail
                .lines(40, 10, 2000)
                .iter()
                .any(|line| line.text().contains("live-tail"))
        );
    }
    #[test]
    fn panel_stop_requires_confirmation_and_current_capability() {
        for can_stop in [false, true] {
            let chat = Uuid::new_v4();
            let task = Uuid::new_v4();
            let mut detail = TickShell::loading(chat, task);
            detail.load(snapshot(chat, task, 1, 0, can_stop), None);
            assert!(matches!(
                detail.key(
                    KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE),
                    40,
                    10
                ),
                TickShellKey::None
            ));
            assert_eq!(detail.hint().contains("enter confirm"), can_stop);
            let result = detail.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), 40, 10);
            assert_eq!(
                matches!(
                    result,
                    TickShellKey::Effect(TaskSurfaceEffect::Control {
                        action: ShellTaskAction::Stop,
                        ..
                    })
                ),
                can_stop
            );
            detail.key(
                KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE),
                40,
                10,
            );
            detail.update(snapshot(chat, task, 2, 0, false));
            assert!(matches!(
                detail.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), 40, 10),
                TickShellKey::None
            ));
            assert!(matches!(
                detail.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), 40, 10),
                TickShellKey::Back
            ));
        }
    }

    #[test]
    fn shell_panel_success_clears_stale_load_snapshot_and_output_errors() {
        let chat = Uuid::new_v4();
        let task = Uuid::new_v4();
        let mut detail = TickShell::loading(chat, task);
        detail.report("failed load".into());
        let effect = detail.load(snapshot(chat, task, 1, 4, true), None).unwrap();
        assert!(!detail.hint().contains("failed load"));
        detail.report("failed snapshot".into());
        detail.update(snapshot(chat, task, 2, 4, true));
        assert!(!detail.hint().contains("failed snapshot"));
        detail.report("failed output".into());
        deliver(&mut detail, effect, "tail");
        assert!(!detail.hint().contains("failed output"));
        detail.report_control("failed control".into());
        detail.update(snapshot(chat, task, 3, 4, true));
        assert!(
            detail.hint().contains("failed control"),
            "snapshot success must not hide a failed control"
        );
        detail.clear_control_error();
        assert!(!detail.hint().contains("failed control"));
    }

    #[test]
    fn reopening_a_panel_retries_failed_output_without_new_bytes() {
        let chat = Uuid::new_v4();
        let task = Uuid::new_v4();
        let mut detail = TickShell::loading(chat, task);
        let initial = snapshot(chat, task, 1, 4, true);
        let Some(TaskSurfaceEffect::ReadOutput { generation, .. }) =
            detail.load(initial.clone(), None)
        else {
            panic!("missing initial read");
        };
        detail.output(TaskOutputUpdate::Unavailable {
            generation,
            task_id: task,
            total_bytes: 4,
            message: "temporary read failure".into(),
        });
        let effect = detail.load(initial, None).unwrap();
        assert!(
            matches!(effect, TaskSurfaceEffect::ReadOutput { .. }),
            "restored panel must retry a failed tail read"
        );
        deliver(&mut detail, effect, "tail");
        assert!(!detail.hint().contains("temporary read failure"));
    }

    #[test]
    fn panel_scroll_keys_share_line_navigation_and_page_down_returns_to_tail() {
        for key in [
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
        ] {
            let chat = Uuid::new_v4();
            let task = Uuid::new_v4();
            let output = (0..40)
                .map(|row| format!("line-{row}\n"))
                .collect::<String>();
            let mut detail = TickShell::loading(chat, task);
            let effect = detail
                .load(snapshot(chat, task, 1, output.len(), true), None)
                .unwrap();
            deliver(&mut detail, effect, &output);
            detail.lines(40, 10, 2000);
            let viewport = TaskViewport {
                width: 40,
                height: 8,
            };
            let before = detail.surface.as_mut().unwrap().frame(viewport).top_row;
            detail.key(key, 40, 10);
            assert_eq!(
                detail.surface.as_mut().unwrap().frame(viewport).top_row,
                before - 1
            );
            for key in [
                KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL),
            ] {
                detail.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), 40, 10);
                let top = detail.surface.as_mut().unwrap().frame(viewport).top_row;
                detail.key(key, 40, 10);
                assert_eq!(
                    detail.surface.as_mut().unwrap().frame(viewport).top_row,
                    top + 1
                );
            }
            detail.key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE), 40, 10);
            assert!(detail.surface.as_mut().unwrap().frame(viewport).follow_tail);
        }
    }
    #[test]
    fn panel_leaves_detail_when_the_task_disappears_or_changes_chat() {
        let chat = Uuid::new_v4();
        let task = Uuid::new_v4();
        let mut detail = TickShell::loading(chat, task);
        detail.load(snapshot(chat, task, 1, 0, true), None);
        assert!(
            detail
                .update(snapshot(Uuid::new_v4(), task, 2, 0, true))
                .is_none()
        );
        let mut missing = snapshot(chat, task, 3, 0, true);
        missing.tasks.clear();
        assert!(detail.update(missing).is_none());
    }
}
