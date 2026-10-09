use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tickhost_connect::naming::short_directory;
use unicode_width::UnicodeWidthStr;
use uuid::Uuid;

use super::{
    console::{truncate_width, wrap_text},
    conversation_picker::ellipsize,
    display::{DisplayProfile, Glyph},
    list_row::{format_elapsed, state_mark},
    monitor::{relative_time, wrap_words},
    safe_line,
    style::{Role, RoleLine},
    task_surface::{
        TaskFrameLineKind, TaskOutputUpdate, TaskSurface, TaskSurfaceAction, TaskSurfaceEffect,
        TaskViewport,
    },
};
use crate::monitor_rows::{key_command, shell_tone, signal_name};
use tickloop_agent_protocol::{ShellTask, ShellTaskAction, ShellTaskSnapshot};

/// The key hint row under the output.
const HINT_ROWS: usize = 1;
/// The full command's rows at most; the last ends in an ellipsis when more does not fit.
const COMMAND_ROWS: usize = 3;
/// The narrowest view whose first row holds the state beside the key command; a narrower one
/// gives the state its own row.
const STATE_BESIDE_COMMAND_MIN_COLUMNS: usize = 40;
const STATE_GAP_COLUMNS: usize = 2;
const MILLISECONDS_PER_SECOND: u64 = 1000;
const COMMAND_PROMPT: &str = "$ ";
const ELLIPSIS: &str = "…";
const RULE: &str = "─";
const LOADING_TEXT: &str = "Loading…";
const STOP_HINT: &str = "s stop";
const SCROLL_HINT: &str = "↑↓ scroll · home/end top/bottom";
const BACK_HINT: &str = "esc back";
const HINT_SEPARATOR: &str = " · ";
const STATE_SEPARATOR: &str = " · ";

/// A background Shell's full-screen detail: a fixed header about the command over its output,
/// which follows the tail and scrolls.
pub(super) struct TickShell {
    pub(super) id: Uuid,
    pub(super) chat: Uuid,
    pub(super) task_id: Uuid,
    display: DisplayProfile,
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
    pub(super) fn loading(chat: Uuid, task_id: Uuid, display: DisplayProfile) -> Self {
        Self {
            id: Uuid::new_v4(),
            chat,
            task_id,
            display,
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
        // The header tells the task's facts, so the surface shows only its output.
        surface.set_detail_metadata(self.task_id, Vec::new());
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
        self.navigate(action, width, height)
    }

    /// Scrolls the output by `lines`, up when negative, unless the stop awaits confirmation.
    pub(super) fn scroll(&mut self, lines: i32, width: usize, height: usize) -> TickShellKey {
        if self.confirm_stop {
            return TickShellKey::None;
        }
        self.navigate(TaskSurfaceAction::ScrollLines(lines), width, height)
    }

    /// Moves the output; moving down past its end follows the tail again.
    fn navigate(&mut self, action: TaskSurfaceAction, width: usize, height: usize) -> TickShellKey {
        let viewport = self.output_viewport(width, height);
        let Some(surface) = self.surface.as_mut() else {
            return TickShellKey::None;
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
    /// The view over `width` × `height`: the header, the output in the rows between, and the
    /// key hints in the last row.
    pub(super) fn lines(&mut self, width: usize, height: usize, now: u64) -> Vec<RoleLine> {
        let Some(content_rows) = height.checked_sub(HINT_ROWS) else {
            return Vec::new();
        };
        let viewport = self.output_viewport(width, height);
        let mut scrollable = false;
        let mut lines = match (&self.task, &mut self.surface) {
            (Some(task), Some(surface)) => {
                let mut lines = header(task, self.started_at_ms, self.display, width, now);
                let frame = surface.frame(viewport);
                scrollable = frame.can_move_up || frame.can_move_down;
                lines.extend(frame.lines.into_iter().map(|line| {
                    let role = match line.kind {
                        TaskFrameLineKind::Output => Role::Text,
                        TaskFrameLineKind::Unavailable => Role::Warning,
                        _ => Role::MutedText,
                    };
                    RoleLine::role(line.text, role)
                }));
                lines
            }
            _ => vec![RoleLine::role(LOADING_TEXT, Role::MutedText)],
        };
        lines.truncate(content_rows);
        lines.resize_with(content_rows, RoleLine::default);
        let hint = if self.confirm_stop {
            // The input dialog below asks to confirm the stop.
            String::new()
        } else {
            self.hints(scrollable.then(|| self.display.navigation_hint(SCROLL_HINT)))
        };
        lines.push(RoleLine::role(hint, Role::MutedText));
        for line in &mut lines {
            line.truncate(width);
        }
        lines
    }

    pub(super) fn hint(&self) -> String {
        self.hints(None)
    }

    /// The keys this view takes, `scroll` among them while the output scrolls. A failure takes
    /// the stop key's place.
    fn hints(&self, scroll: Option<String>) -> String {
        if self.confirm_stop {
            return "Stop this Shell? enter confirm · esc cancel".into();
        }
        let lead = self
            .error
            .as_deref()
            .or_else(|| self.surface.as_ref().and_then(TaskSurface::visible_error))
            .map(safe_line)
            .or_else(|| self.can_stop().then(|| STOP_HINT.to_owned()));
        lead.into_iter()
            .chain(scroll)
            .chain([BACK_HINT.to_owned()])
            .collect::<Vec<_>>()
            .join(HINT_SEPARATOR)
    }

    /// Where the output shows: the full width, in the rows the header and the hint row leave.
    fn output_viewport(&self, width: usize, height: usize) -> TaskViewport {
        let header_rows = self.task.as_ref().map_or(0, |task| {
            header(task, self.started_at_ms, self.display, width, 0).len()
        });
        TaskViewport {
            width,
            height: height.saturating_sub(header_rows + HINT_ROWS),
        }
    }
}

/// The rows that stay above the output wherever it scrolls: the key command with the state at its
/// right, the description, the full command when it says more, the working directory and a rule.
/// How many there are depends on `width` alone.
fn header(
    task: &ShellTask,
    started_at_ms: Option<u64>,
    display: DisplayProfile,
    width: usize,
    now: u64,
) -> Vec<RoleLine> {
    let key_command = key_command(&task.command);
    let title = format!("{COMMAND_PROMPT}{}", safe_line(&key_command));
    let mut state = RoleLine::role(
        display.glyph(Glyph::ToolCall),
        state_mark(shell_tone(task, started_at_ms), display, now),
    );
    state.push(" ", None);
    state.push(state_text(task, started_at_ms, now), Some(Role::MutedText));
    let mut lines = Vec::new();
    if width >= STATE_BESIDE_COMMAND_MIN_COLUMNS {
        let state_columns = state.text().width();
        let mut first = RoleLine::role(
            ellipsize(
                &title,
                width.saturating_sub(state_columns + STATE_GAP_COLUMNS),
            ),
            Role::Text,
        );
        first.push(
            " ".repeat(width.saturating_sub(first.text().width() + state_columns)),
            None,
        );
        for span in state.spans {
            first.push(span.text, span.role);
        }
        lines.push(first);
    } else {
        lines.push(RoleLine::role(ellipsize(&title, width), Role::Text));
        lines.push(state);
    }
    if let Some(description) = task.description.as_deref().and_then(|description| {
        description
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
    }) {
        lines.push(RoleLine::role(
            ellipsize(&safe_line(description), width),
            Role::MutedText,
        ));
    }
    let command = task.command.trim();
    if command != key_command {
        lines.extend(
            command_rows(command, width)
                .into_iter()
                .map(|row| RoleLine::role(row, Role::MutedText)),
        );
    }
    lines.push(RoleLine::role(
        ellipsize(
            &safe_line(&short_directory(&task.cwd.to_string_lossy())),
            width,
        ),
        Role::MutedText,
    ));
    lines.push(RoleLine::role(RULE.repeat(width), Role::MutedText));
    lines
}

/// The state in words beside its glyph, as a Monitor row tells it: how long a running Shell has
/// run, or how it ended and how long ago, such as `running 2m 3s` or `SIGTERM · 3m ago`.
fn state_text(task: &ShellTask, started_at_ms: Option<u64>, now: u64) -> String {
    if !task.state.is_terminal() {
        return started_at_ms.map_or_else(
            || task.status_label(),
            |since| {
                format!(
                    "{} {}",
                    task.status_label(),
                    format_elapsed(now.saturating_sub(since))
                )
            },
        );
    }
    let outcome = task
        .signal
        .and_then(signal_name)
        .map_or_else(|| task.status_label(), str::to_owned);
    let Some(ended) = task.ended_at_ms.or(task.updated_at_ms) else {
        return outcome;
    };
    let age = relative_time(Some(ended), now);
    if now.saturating_sub(ended) < MILLISECONDS_PER_SECOND {
        format!("{outcome}{STATE_SEPARATOR}{age}")
    } else {
        format!("{outcome}{STATE_SEPARATOR}{age} ago")
    }
}

/// `command` broken between words into at most [`COMMAND_ROWS`] rows of `width` columns, a word
/// wider than a row cut across rows.
fn command_rows(command: &str, width: usize) -> Vec<String> {
    let mut rows = command
        .lines()
        .flat_map(|line| wrap_words(&safe_line(line), width))
        .flat_map(|row| {
            if row.width() > width {
                wrap_text(&row, width)
            } else {
                vec![row]
            }
        });
    let mut kept = rows.by_ref().take(COMMAND_ROWS).collect::<Vec<_>>();
    if rows.next().is_some()
        && let Some(last) = kept.last_mut()
    {
        *last = format!(
            "{}{ELLIPSIS}",
            truncate_width(last, width.saturating_sub(ELLIPSIS.width())).trim_end()
        );
    }
    kept
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
    const NOW: u64 = 10_000_000;
    const LOGGED_TEST_RUN: &str = "cd /srv/work/tickloop/feat && (make test > target/x.log 2>&1; echo \"EXIT $?\" >> target/x.log)";

    /// A detail of a running `command`, started 2m 3s before [`NOW`], showing `output`.
    fn detail_showing(command: &str, description: Option<&str>, output: &str) -> TickShell {
        let chat = Uuid::new_v4();
        let task = Uuid::new_v4();
        let mut shell = snapshot(chat, task, 1, output.len(), true);
        shell.tasks[0].command = command.into();
        shell.tasks[0].description = description.map(Into::into);
        shell.tasks[0].cwd = "/srv/work/tickloop/feat".into();
        let mut detail = TickShell::loading(chat, task, DisplayProfile::default());
        let effect = detail.load(shell, Some(NOW - 123_000)).unwrap();
        deliver(&mut detail, effect, output);
        detail
    }

    fn texts(lines: &[RoleLine]) -> Vec<String> {
        lines.iter().map(RoleLine::text).collect()
    }

    #[test]
    fn the_header_names_the_key_command_and_stays_while_the_output_scrolls() {
        let output = (0..40)
            .map(|row| format!("line-{row}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut detail = detail_showing(
            LOGGED_TEST_RUN,
            Some("Rerun full test suite in background to verify flakiness"),
            &output,
        );
        let rows = texts(&detail.lines(60, 12, NOW));
        let state = "⏺ running 2m 3s";
        assert_eq!(
            rows,
            [
                format!("$ make test{}{state}", " ".repeat(60 - 11 - state.width())),
                "Rerun full test suite in background to verify flakiness".into(),
                "cd /srv/work/tickloop/feat && (make test > target/x.log".into(),
                "2>&1; echo \"EXIT $?\" >> target/x.log)".into(),
                "/…/tickloop/feat".into(),
                "─".repeat(60),
                "line-35".into(),
                "line-36".into(),
                "line-37".into(),
                "line-38".into(),
                "line-39".into(),
                "s stop · ↑↓ scroll · home/end top/bottom · esc back".into(),
            ]
        );
        let header = rows[..6].to_vec();
        for _ in 0..3 {
            detail.key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE), 60, 12);
        }
        let scrolled = texts(&detail.lines(60, 12, NOW));
        assert_eq!(scrolled[..6], header, "the header does not scroll");
        assert_eq!(scrolled[6], "line-20");
        detail.key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE), 60, 12);
        assert_eq!(texts(&detail.lines(60, 12, NOW))[6], "line-0");
    }

    #[test]
    fn output_wraps_at_the_full_width_without_cutting_wide_characters() {
        let long = "word ".repeat(30);
        let wide = "界".repeat(45);
        let mut detail = detail_showing("make test", None, &format!("{long}\n{wide}\nfinal"));
        let rows = texts(&detail.lines(100, 20, NOW));
        assert_eq!(rows[0].width(), 100, "{rows:#?}");
        let output = &rows[3..rows.len() - 1];
        assert_eq!(output[0], "word ".repeat(20));
        assert_eq!(output[1], "word ".repeat(10));
        assert_eq!(output[2], "界".repeat(45));
        assert_eq!(output[3], "final");
        let narrow = texts(&detail.lines(60, 20, NOW));
        let output = &narrow[3..narrow.len() - 1];
        assert_eq!(
            output[..3],
            ["word ".repeat(12), "word ".repeat(12), "word ".repeat(6)]
        );
        assert_eq!(output[3], "界".repeat(30));
        assert_eq!(output[4], "界".repeat(15));
        assert!(narrow.iter().all(|row| row.width() <= 60), "{narrow:#?}");
    }

    #[test]
    fn a_narrow_view_gives_the_state_its_own_row_and_hides_nothing_else() {
        let mut detail = detail_showing("make test", None, "done");
        let rows = texts(&detail.lines(24, 8, NOW));
        assert_eq!(
            rows,
            [
                "$ make test",
                "⏺ running 2m 3s",
                "/…/tickloop/feat",
                "─".repeat(24).as_str(),
                "done",
                "",
                "",
                "s stop · esc back",
            ]
        );
    }

    #[test]
    fn the_full_command_shows_when_it_says_more_in_at_most_three_rows() {
        let mut same = detail_showing("make test", None, "");
        assert!(
            !texts(&same.lines(60, 10, NOW)).contains(&"make test".to_owned()),
            "a command that is its own key command shows once"
        );
        assert_eq!(
            command_rows(
                "cargo nextest run -p console --no-fail-fast 2>&1 | tail -20",
                20
            ),
            ["cargo nextest run -p", "console", "--no-fail-fast 2>&1…"]
        );
        assert_eq!(
            command_rows("python3 - <<'EOF'\nprint(1)\nEOF", 40),
            ["python3 - <<'EOF'", "print(1)", "EOF"]
        );
        assert_eq!(
            command_rows(&format!("ls {}", "x".repeat(50)), 20),
            [
                "ls".to_owned(),
                "x".repeat(20),
                format!("{}…", "x".repeat(19))
            ]
        );
    }

    #[test]
    fn the_state_reads_as_the_monitor_row_tells_it() {
        let (chat, id) = (Uuid::new_v4(), Uuid::new_v4());
        let mut shell = snapshot(chat, id, 1, 0, true).tasks.remove(0);
        assert_eq!(state_text(&shell, None, NOW), "running");
        assert_eq!(
            state_text(&shell, Some(NOW - 3_723_000), NOW),
            "running 1h 2m 3s"
        );
        shell.state = tickloop_agent_protocol::ShellTaskState::Completed;
        shell.ended_at_ms = Some(NOW - 180_000);
        for (exit_code, signal, reason, expected) in [
            (Some(0), None, None, "exit 0 · 3m ago"),
            (Some(2), None, None, "exit 2 · 3m ago"),
            (Some(143), Some(libc::SIGTERM), None, "SIGTERM · 3m ago"),
            (None, Some(64), None, "signal 64 · 3m ago"),
            (None, None, Some("stopped"), "stopped · 3m ago"),
        ] {
            shell.exit_code = exit_code;
            shell.signal = signal;
            shell.reason = reason.map(Into::into);
            assert_eq!(state_text(&shell, None, NOW), expected);
        }
        shell.ended_at_ms = Some(NOW - 400);
        assert_eq!(state_text(&shell, None, NOW), "stopped · now");
    }
    #[test]
    fn panel_follows_tail_until_scrolled_and_pages_back_to_newest_output() {
        let chat = Uuid::new_v4();
        let task = Uuid::new_v4();
        let first = (0..30)
            .map(|row| format!("line-{row}\n"))
            .collect::<String>();
        let mut detail = TickShell::loading(chat, task, DisplayProfile::default());
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
            let mut detail = TickShell::loading(chat, task, DisplayProfile::default());
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
        let mut detail = TickShell::loading(chat, task, DisplayProfile::default());
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
        let mut detail = TickShell::loading(chat, task, DisplayProfile::default());
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
            let mut detail = TickShell::loading(chat, task, DisplayProfile::default());
            let effect = detail
                .load(snapshot(chat, task, 1, output.len(), true), None)
                .unwrap();
            deliver(&mut detail, effect, &output);
            detail.lines(40, 10, 2000);
            let viewport = detail.output_viewport(40, 10);
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
        let mut detail = TickShell::loading(chat, task, DisplayProfile::default());
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
