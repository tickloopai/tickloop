//! The Console's side of full-screen views: Enter or a double click on a Terminal Tick's or a
//! Shell's row lends this whole terminal to the view until the user hides it, while Agent events
//! keep updating the state and nothing is drawn.

use super::*;
use crate::terminal::attach::{AttachEnd, AttachedView, ViewInput};
use tick_protocol::InstanceRef;

/// Space on an ended Terminal Tick's row, which has no card to show.
pub(super) const ENDED_TERMINAL_NOTICE: &str = "This Tick ended; enter reopens it";
/// Rings the terminal's bell once, for a question that waits behind a full-screen view.
const BELL: &[u8] = b"\x07";

/// A view of `chat` waiting to take this terminal.
pub(super) struct FullScreenAttach {
    pub(super) chat: Uuid,
    pub(super) instance: InstanceRef,
    pub(super) grant: tickloop_harness::protocol::HostedViewGrant,
}

impl State {
    /// The event that carries out a Monitor row's `intent` outside a key on the list, such as the
    /// stop Ctrl+C asks of a Shell shown full screen.
    fn row_action(&mut self, intent: &crate::monitor_rows::TickIntent) -> Option<InputEvent> {
        let chat = self.chat?;
        let executor = super::super::tick_actions::builtin::executor(intent)?;
        match executor.prepare(&mut super::super::tick_actions::TickActionUi { state: self }) {
            Ok(action) => Some(InputEvent::TickAction { chat, action }),
            Err(error) => {
                self.monitor.action_error = Some(super::super::safe_line(&error));
                None
            }
        }
    }

    /// The row action that stops `instance`, a running Shell this Chat may stop.
    fn shell_stop(&self, instance: &InstanceRef) -> Option<crate::monitor_rows::TickIntent> {
        self.monitor
            .entries()
            .iter()
            .find(|entry| &entry.instance == instance)?
            .actions
            .iter()
            .find(|action| {
                matches!(
                    action.intent,
                    crate::monitor_rows::TickIntent::StopShell { .. }
                )
            })
            .map(|action| action.intent.clone())
    }

    pub(super) fn full_screen_pending(&self) -> bool {
        self.full_screen_attach.is_some() || self.terminal_lent
    }

    /// While the terminal is lent, a permission request or a question the Agent asks waits for
    /// the view to be hidden; the terminal rings once for each.
    pub(super) fn ring_for_waiting_question(&mut self) {
        let waiting = self.menu.as_ref().map(|menu| menu.id);
        if waiting.is_some() && waiting != self.rung_for {
            let mut output = io::stdout().lock();
            let _ = output.write_all(BELL).and_then(|()| output.flush());
        }
        self.rung_for = waiting;
    }

    /// Hands the terminal to a full-screen view: from now on nothing is drawn until it is taken
    /// back, while Agent events keep updating the state.
    fn lend_terminal(&mut self) -> io::Result<()> {
        self.terminal_lent = true;
        self.rung_for = self.menu.as_ref().map(|menu| menu.id);
        self.schedule_file_index();
        let mut output = io::stdout().lock();
        self.surface
            .release(self.renderer, &mut output)
            .and_then(|()| write!(output, "{RESET_SCROLL_REGION}"))
            .and_then(|()| execute!(output, Show))
            .and_then(|()| output.flush())
    }

    /// Takes the terminal back from a full-screen view; the Monitor list is shown as it was, with
    /// `failure` when the view did not end with the user hiding it.
    fn reclaim_terminal(&mut self, failure: Option<&str>) {
        let mut output = io::stdout().lock();
        let restored = self.surface.take(self.renderer, &mut output);
        drop(output);
        self.terminal_lent = false;
        self.screen_initialized = false;
        self.rendered_region = None;
        match restored {
            Err(error) => {
                self.monitor.action_error = Some(super::super::safe_line(&format!(
                    "Cannot restore the terminal: {error}"
                )));
            }
            Ok(()) => {
                if let Some(failure) = failure {
                    self.monitor.action_error = Some(super::super::safe_line(failure));
                }
            }
        }
    }
}

impl Console {
    /// Hands this terminal to the view `grant` admits as soon as the input thread is between
    /// keys, provided `chat` is still shown in this interactive View.
    pub(in crate::terminal) fn begin_full_screen_attach(
        &self,
        chat: Uuid,
        instance: InstanceRef,
        grant: tickloop_harness::protocol::HostedViewGrant,
    ) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if state.chat != Some(chat) || !state.interactive {
            return Err("This View can no longer show the Tick full screen".into());
        }
        state.full_screen_attach = Some(FullScreenAttach {
            chat,
            instance,
            grant,
        });
        Ok(())
    }

    /// Whether the Terminal Tick `reopened` in a row's place goes full screen: the View still
    /// shows `chat`, and the Monitor list still has the keys on its row.
    pub(in crate::terminal) fn reopened_opens_full_screen(
        &self,
        chat: Uuid,
        reopened: &InstanceRef,
    ) -> Result<bool, String> {
        let state = self.state.lock().map_err(|error| error.to_string())?;
        Ok(state.chat == Some(chat)
            && state.monitor_owns_keys()
            && state
                .monitor
                .selected()
                .is_some_and(|entry| &entry.instance == reopened && entry.opens_full_screen()))
    }
}

/// Lends the terminal to a full-screen view until the user hides it. The input thread runs the
/// view, so no key reaches the Console, and the state stays unlocked, so Agent events keep
/// arriving while nothing is drawn. Returns whether input continues.
pub(super) fn lend_terminal_to_attached_view(
    state: &Arc<Mutex<State>>,
    attach: FullScreenAttach,
    stop: &AtomicBool,
    sender: &mpsc::Sender<InputEvent>,
) -> io::Result<bool> {
    let lock = || {
        state
            .lock()
            .map_err(|error| io::Error::other(error.to_string()))
    };
    {
        let mut state = lock()?;
        if state.chat != Some(attach.chat) {
            state.monitor_for_chat_mut(attach.chat).action_error =
                Some("Not opened: another Chat is shown now".into());
            state.draw(&mut io::stdout().lock())?;
            return Ok(!stop.load(Ordering::Acquire));
        }
    }
    let input = if attach.instance.shell_task().is_some() {
        ViewInput::ReadOnly {
            stoppable: lock()?.shell_stop(&attach.instance).is_some(),
        }
    } else {
        ViewInput::Interactive
    };
    let attached = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())
        .and_then(|runtime| {
            runtime
                .block_on(AttachedView::attach(&attach.grant, input))
                .map(|view| (runtime, view))
                .map_err(|error| error.to_string())
        });
    let (runtime, view) = match attached {
        Ok(attached) => attached,
        Err(error) => {
            let mut state = lock()?;
            state.monitor_for_chat_mut(attach.chat).action_error = Some(super::super::safe_line(
                &format!("Cannot open the Tick: {error}"),
            ));
            state.draw(&mut io::stdout().lock())?;
            return Ok(!stop.load(Ordering::Acquire));
        }
    };
    // Ctrl+C on a Shell's screen stops it as the row's stop would, while it still can.
    let interrupt = || {
        let event = state.lock().ok().and_then(|mut state| {
            let intent = state.shell_stop(&attach.instance)?;
            state.row_action(&intent)
        });
        if let Some(event) = event {
            send_input(sender, stop, event);
        }
    };
    let lent = lock()?.lend_terminal();
    let ended = lent.map(|()| runtime.block_on(view.run(interrupt)));
    let tick_ended = matches!(ended, Ok(Ok(AttachEnd::Gone)));
    let failure = match ended {
        // The selected row's status line says how it ended once Monitor reads it again.
        Ok(Ok(AttachEnd::Hidden | AttachEnd::Gone)) => None,
        Ok(Ok(AttachEnd::Lost(reason))) => Some(format!("The view ended: {reason}")),
        Ok(Err(error)) => Some(format!("The view ended: {error}")),
        Err(error) => Some(format!("Cannot hand the terminal to the Tick: {error}")),
    };
    let mut state = lock()?;
    state.reclaim_terminal(failure.as_deref());
    state.draw(&mut io::stdout().lock())?;
    let refresh = tick_ended
        .then(|| {
            let executor = state.monitor.refresh_action();
            let action = executor
                .prepare(&mut super::super::tick_actions::TickActionUi { state: &mut state })
                .ok()?;
            Some(InputEvent::TickAction {
                chat: attach.chat,
                action,
            })
        })
        .flatten();
    drop(state);
    if let Some(refresh) = refresh {
        send_input(sender, stop, refresh);
    }
    Ok(!stop.load(Ordering::Acquire))
}
