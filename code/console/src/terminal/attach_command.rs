//! `tickloop attach [<instance>]`: shows one local Tick or Shell of any Chat the local daemon
//! runs full screen in this terminal, as Enter in the Console's Monitor does, until the user
//! hides it; the Tick keeps running. Without an instance it lists the ones it can show first.

use std::{ffi::OsString, io::Write};

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    terminal,
};
use tickloop_code_access::{ExecutionAvailability, LaunchSpec};
use tickloop_harness::protocol::{HostedMemberState, ShellTaskState};
use unicode_width::UnicodeWidthStr;
use uuid::Uuid;

use super::{
    attach::{AttachEnd, AttachedView, ViewInput},
    execution::ConsoleExecution,
};

const USAGE: &str = "Usage: tickloop attach [INSTANCE]

Shows a Tick or a Shell that a running Agent opened, full screen in this terminal. Without an
instance, lists them to choose from. Hiding the view (esc, or esc esc in a Tick) leaves the Tick
running.";
const NO_DAEMON: &str = "No Tickloop Agent is running";
const NOTHING_TO_ATTACH: &str = "No running Agent shows a Tick or a Shell";
const PICKER_TITLE: &str = "Attach to a Tick or a Shell";
const PICKER_HINT: &str = "↑↓ select · enter attach · esc cancel";
const CURSOR: &str = "❯ ";
const NO_CURSOR: &str = "  ";
const ELLIPSIS: char = '…';
const TICK_GLYPH: &str = "◉";
const TERMINAL_GLYPH: &str = ">";
const SHELL_GLYPH: &str = "$";
const ENTER_SCREEN: &str = "\x1b[?1049h\x1b[?25l";
const LEAVE_SCREEN: &str = "\x1b[?25h\x1b[?1049l";

/// One Tick or Shell this command can show, and the Chat that opened it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Attachable {
    chat: Uuid,
    instance: tick_protocol::InstanceRef,
    glyph: &'static str,
    title: String,
    chat_title: String,
    is_shell: bool,
}

pub async fn run(arguments: &[OsString]) -> Result<(), String> {
    let wanted = match arguments {
        [] => None,
        [flag] if matches!(flag.to_str(), Some("--help" | "-h")) => {
            println!("{USAGE}");
            return Ok(());
        }
        [instance] => Some(
            instance
                .to_str()
                .and_then(|instance| instance.parse::<tick_protocol::InstanceRef>().ok())
                .ok_or_else(|| {
                    format!("Not an instance: {}\n\n{USAGE}", instance.to_string_lossy())
                })?,
        ),
        _ => return Err(USAGE.into()),
    };
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let client = tickloop_code_client::connect_to_running_daemon(executable)
        .await
        .map_err(|_| NO_DAEMON.to_owned())?;
    let folder = std::env::current_dir().unwrap_or_default();
    let agent = ConsoleExecution::connected(client, LaunchSpec::embedded(folder), None);
    let attachables = attachables(&agent).await?;
    let chosen = match wanted {
        Some(instance) => attachables
            .into_iter()
            .find(|attachable| attachable.instance == instance)
            .ok_or_else(|| format!("No running Agent shows {instance}"))?,
        None if attachables.is_empty() => return Err(NOTHING_TO_ATTACH.into()),
        None => match pick(attachables)? {
            Some(chosen) => chosen,
            None => return Ok(()),
        },
    };
    let grant = agent
        .attach_hosted_tick(chosen.chat, chosen.instance.clone())
        .await
        .map_err(|error| format!("Cannot attach to {}: {error}", chosen.instance))?;
    let input = if chosen.is_shell {
        ViewInput::ReadOnly { stoppable: false }
    } else {
        // Reopening takes the Chat's Monitor row, so only the Console offers it.
        ViewInput::Interactive { reopenable: false }
    };
    let view = AttachedView::attach(&grant, input)
        .await
        .map_err(|error| format!("Cannot attach to {}: {error}", chosen.instance))?;
    terminal::enable_raw_mode().map_err(|error| error.to_string())?;
    let ended = view.run(|| {}).await;
    let _ = terminal::disable_raw_mode();
    match ended {
        Ok(AttachEnd::Hidden | AttachEnd::Reopen) => Ok(()),
        Ok(AttachEnd::Gone) => Err(format!("{} ended while shown", chosen.title)),
        Ok(AttachEnd::Lost(reason)) => Err(format!("The view ended: {reason}")),
        Err(error) => Err(format!("The view ended: {error}")),
    }
}

/// The Ticks and Shells every running Chat can show: its hosted Ticks that still have a screen
/// and the Shells it ran in the background.
async fn attachables(agent: &ConsoleExecution) -> Result<Vec<Attachable>, String> {
    let chats = agent
        .catalog()
        .await
        .map_err(|error| format!("Cannot list the running Agents: {error}"))?;
    let mut attachables = Vec::new();
    for chat in chats
        .into_iter()
        .filter(|chat| chat.availability == ExecutionAvailability::Live)
    {
        // A Chat whose Worker cannot answer now has nothing to show; the others still do.
        let ticks = agent.hosted_ticks(chat.chat_id).await.unwrap_or_default();
        let shells = agent.task_rows(chat.chat_id).await.unwrap_or_default();
        // The names its Monitor shows, such as a Tick's declared title.
        let monitor_titles: std::collections::HashMap<_, _> = agent
            .status(chat.chat_id)
            .await
            .map(|status| {
                status
                    .monitor
                    .members
                    .into_iter()
                    .filter_map(|member| Some((member.instance, member.title?)))
                    .collect()
            })
            .unwrap_or_default();
        attachables.extend(
            ticks
                .into_iter()
                .filter(|tick| tick.state != HostedMemberState::Lost)
                .map(|tick| Attachable {
                    chat: chat.chat_id,
                    glyph: if tick.is_terminal {
                        TERMINAL_GLYPH
                    } else {
                        TICK_GLYPH
                    },
                    title: monitor_titles
                        .get(&tick.instance)
                        .cloned()
                        .or(tick.title)
                        .unwrap_or_else(|| tick.instance.to_string()),
                    instance: tick.instance,
                    chat_title: chat.title.clone(),
                    is_shell: false,
                }),
        );
        attachables.extend(
            shells
                .into_iter()
                .filter(|shell| shell.was_background || shell.state == ShellTaskState::Running)
                .map(|shell| Attachable {
                    chat: chat.chat_id,
                    instance: tick_protocol::InstanceRef::shell(shell.id),
                    glyph: SHELL_GLYPH,
                    title: shell
                        .description
                        .filter(|description| !description.trim().is_empty())
                        .unwrap_or(shell.command),
                    chat_title: chat.title.clone(),
                    is_shell: true,
                }),
        );
    }
    Ok(attachables)
}

/// What a key does in the picker.
#[derive(Debug, PartialEq, Eq)]
enum PickerKey {
    Moved,
    Chosen,
    Cancelled,
    Ignored,
}

/// The list the user chooses an instance from: rows without numbers, the current one marked
/// with the cursor.
struct Picker {
    rows: Vec<Attachable>,
    selected: usize,
}

impl Picker {
    fn key(&mut self, key: KeyEvent) -> PickerKey {
        match (key.code, key.modifiers) {
            (KeyCode::Down, _) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                self.selected = (self.selected + 1).min(self.rows.len().saturating_sub(1));
                PickerKey::Moved
            }
            (KeyCode::Up, _) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                self.selected = self.selected.saturating_sub(1);
                PickerKey::Moved
            }
            (KeyCode::Enter, _) => PickerKey::Chosen,
            (KeyCode::Esc, _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => PickerKey::Cancelled,
            _ => PickerKey::Ignored,
        }
    }

    /// The picker on a screen of `width` × `height`: the title, the rows that fit around the
    /// selected one, and the key hints last.
    fn lines(&self, width: usize, height: usize) -> Vec<String> {
        let room = height.saturating_sub(3).max(1);
        let first = self.selected.saturating_sub(room - 1);
        let mut lines = vec![fit(PICKER_TITLE, width), String::new()];
        for (index, row) in self.rows.iter().enumerate().skip(first).take(room) {
            let cursor = if index == self.selected {
                CURSOR
            } else {
                NO_CURSOR
            };
            lines.push(fit(
                &format!("{cursor}{} {} · {}", row.glyph, row.title, row.chat_title),
                width,
            ));
        }
        lines.push(fit(PICKER_HINT, width));
        lines
    }
}

fn fit(text: &str, width: usize) -> String {
    let text: String = text
        .chars()
        .filter(|character| !character.is_control())
        .collect();
    if text.width() <= width {
        return text;
    }
    let mut cut = String::new();
    for character in text.chars() {
        if (cut.clone() + &character.to_string()).width() + 1 > width {
            break;
        }
        cut.push(character);
    }
    cut.push(ELLIPSIS);
    cut
}

/// Lets the user choose one of `rows` on this terminal's own screen; `None` when they cancel.
fn pick(rows: Vec<Attachable>) -> Result<Option<Attachable>, String> {
    let mut picker = Picker { rows, selected: 0 };
    terminal::enable_raw_mode().map_err(|error| error.to_string())?;
    let mut output = std::io::stdout().lock();
    let _ = write!(output, "{ENTER_SCREEN}");
    let chosen = loop {
        let (columns, rows) = terminal::size().unwrap_or((80, 24));
        let _ = write!(output, "\x1b[H\x1b[2J");
        for (row, line) in picker
            .lines(usize::from(columns), usize::from(rows))
            .iter()
            .enumerate()
        {
            let _ = write!(output, "\x1b[{};1H{line}", row + 1);
        }
        let _ = output.flush();
        match event::read() {
            Ok(Event::Key(key)) if key.kind != KeyEventKind::Release => match picker.key(key) {
                PickerKey::Chosen => break Ok(picker.rows.get(picker.selected).cloned()),
                PickerKey::Cancelled => break Ok(None),
                PickerKey::Moved | PickerKey::Ignored => {}
            },
            Ok(_) => {}
            Err(error) => break Err(error.to_string()),
        }
    };
    let _ = write!(output, "{LEAVE_SCREEN}");
    let _ = output.flush();
    let _ = terminal::disable_raw_mode();
    chosen
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(title: &str) -> Attachable {
        Attachable {
            chat: Uuid::nil(),
            instance: tick_protocol::InstanceRef::shell(Uuid::new_v4()),
            glyph: SHELL_GLYPH,
            title: title.into(),
            chat_title: "Fix the build".into(),
            is_shell: true,
        }
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn the_picker_moves_with_arrows_and_ctrl_n_p_and_stops_at_either_end() {
        let mut picker = Picker {
            rows: vec![row("one"), row("two"), row("three")],
            selected: 0,
        };
        picker.key(key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(picker.selected, 0);
        picker.key(key(KeyCode::Char('n'), KeyModifiers::CONTROL));
        picker.key(key(KeyCode::Down, KeyModifiers::NONE));
        picker.key(key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(picker.selected, 2);
        picker.key(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert_eq!(picker.selected, 1);
        assert_eq!(
            picker.key(key(KeyCode::Char('1'), KeyModifiers::NONE)),
            PickerKey::Ignored,
            "rows have no number shortcuts"
        );
        assert_eq!(
            picker.key(key(KeyCode::Enter, KeyModifiers::NONE)),
            PickerKey::Chosen
        );
        assert_eq!(
            picker.key(key(KeyCode::Esc, KeyModifiers::NONE)),
            PickerKey::Cancelled
        );
    }

    #[test]
    fn the_picker_marks_the_current_row_with_the_cursor_and_keeps_it_in_view() {
        let picker = Picker {
            rows: (0..10).map(|index| row(&format!("task {index}"))).collect(),
            selected: 7,
        };
        let lines = picker.lines(40, 6);
        assert_eq!(lines[0], PICKER_TITLE);
        assert_eq!(lines.last().unwrap(), PICKER_HINT);
        assert_eq!(lines.len(), 6);
        assert_eq!(lines[4], "❯ $ task 7 · Fix the build");
        assert_eq!(lines[2], "  $ task 5 · Fix the build");
        assert!(lines.iter().all(|line| line.width() <= 40));
        assert!(picker.lines(12, 6)[2].ends_with(ELLIPSIS));
    }
}
