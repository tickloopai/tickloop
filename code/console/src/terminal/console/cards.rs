//! The Console's side of Monitor cards: which rows open one, the keys and clicks that open,
//! enter and leave them, the raw input a focused card hands to its Tick, and the views' attach.

use super::*;
use crate::terminal::monitor::MonitorTarget;
use crate::terminal::monitor_cards::{
    CARD_FRAME_INTERVAL, CardDemand, CardInput, CardModes, CardNote, CardScreen, CardSize,
    CardState, CardView, DEFAULT_CARD_ROWS, SgrMouse, ViewEnd,
};
use tick_protocol::InstanceRef;

/// What the frame needs of one open card; its screen is locked only while the frame is drawn.
pub(super) struct CardSource {
    pub(super) instance: InstanceRef,
    pub(super) demand: CardDemand,
    pub(super) focused: bool,
    pub(super) screen: Option<Arc<Mutex<CardScreen>>>,
    pub(super) note: Option<CardNote>,
}

const OPENING_NOTE: &str = "opening";

impl State {
    pub(super) fn card_focused(&self) -> bool {
        self.cards.focused().is_some()
    }

    /// Whether Space and a click on `instance`'s row toggle its card: its view opens, or its
    /// card is still shown after its Tick ended.
    pub(super) fn row_toggles_card(&self, instance: &InstanceRef) -> bool {
        self.has_card(instance) || self.cards.is_open(instance)
    }

    /// Whether `instance`'s row shows a live screen: a hosted Tick or a Shell whose view opens.
    pub(super) fn has_card(&self, instance: &InstanceRef) -> bool {
        self.monitor
            .entries()
            .iter()
            .find(|entry| &entry.instance == instance)
            .is_some_and(|entry| {
                entry.actions.iter().any(|action| {
                    matches!(action.intent, crate::monitor_rows::TickIntent::OpenView(_))
                })
            })
    }

    /// The fewest rows the Tick declared when it opened, or the default.
    fn card_min_rows(&self, instance: &InstanceRef) -> u16 {
        self.monitor
            .chat_status
            .as_ref()
            .and_then(|status| {
                status
                    .monitor
                    .members
                    .iter()
                    .find(|member| &member.instance == instance)
            })
            .and_then(|member| member.min_rows)
            .unwrap_or(DEFAULT_CARD_ROWS)
            .clamp(
                tick_protocol::MIN_SCREEN_ROWS,
                tick_protocol::MAX_SCREEN_ROWS,
            )
    }

    /// The size `instance`'s view attaches at: its last allotted size, or its minimum at the
    /// column's width until a frame allots one.
    pub(super) fn card_size(&self, instance: &InstanceRef) -> CardSize {
        self.card_sizes.get(instance).copied().unwrap_or(CardSize {
            rows: self.card_min_rows(instance),
            columns: u16::try_from(self.monitor_columns().saturating_sub(2).max(1))
                .unwrap_or(u16::MAX),
        })
    }

    /// Space, or a click on the row: shows the card, or hides it, which detaches its view. The
    /// focus stays on the list.
    pub(super) fn toggle_card(&mut self, instance: InstanceRef) -> Option<InputEvent> {
        self.rendered_region = None;
        if self.cards.is_open(&instance) {
            self.cards.close(&instance);
            self.card_sizes.remove(&instance);
            return None;
        }
        self.open_card(instance)
    }

    /// Enter, or a click inside a card: shows the card if it is hidden and gives it the focus,
    /// so the user's input goes to its Tick.
    pub(super) fn enter_card(&mut self, instance: InstanceRef) -> Option<InputEvent> {
        self.rendered_region = None;
        self.monitor.select(&instance);
        let attach = if self.cards.is_open(&instance) {
            None
        } else {
            self.open_card(instance.clone())
        };
        self.cards.focus(instance);
        attach
    }

    fn open_card(&mut self, instance: InstanceRef) -> Option<InputEvent> {
        let chat = self.chat?;
        if !self.cards.open(chat, instance.clone()) {
            return None;
        }
        Some(InputEvent::TickAction {
            chat,
            action: super::super::tick_actions::builtin::attach_card(instance.clone())
                .opening(instance),
        })
    }

    /// Takes up a terminal size change no resize event reported, as while a card reads the raw
    /// input; returns whether the size changed.
    pub(super) fn follow_terminal_size(&mut self) -> bool {
        let Ok((width, height)) = terminal::size() else {
            return false;
        };
        let changed = (
            usize::from(width).saturating_sub(1).max(2),
            usize::from(height).max(1),
        ) != (self.width, self.height);
        if changed {
            self.resize(width, height);
        }
        changed
    }

    pub(super) fn leave_card(&mut self) {
        self.cards.leave_focus();
        self.rendered_region = None;
    }

    /// Whether the last frame showed a card, so that a frame of its Tick changes the screen.
    pub(super) fn card_in_view(&self) -> bool {
        self.monitor_row_targets
            .iter()
            .any(|target| matches!(target.target, MonitorTarget::Card { .. }))
    }

    /// When the focused card's held Esc or unfinished sequence next has to be settled.
    pub(super) fn card_deadline(&self) -> Option<Instant> {
        self.card_focused()
            .then(|| self.cards.keys_deadline())
            .flatten()
    }

    /// Hands the user's raw input bytes, read while a card has the focus, to its Tick.
    pub(super) fn card_input(&mut self, input: &[u8], now: Instant) {
        let paste_markers = self.focused_card_screen(|screen| screen.bracketed_paste());
        let items = self
            .cards
            .keys_mut()
            .feed(input, now, paste_markers.unwrap_or(false));
        self.apply_card_inputs(items, now);
    }

    /// Settles a held Esc once its time passed; returns whether the screen changed.
    pub(super) fn expire_card_keys(&mut self, now: Instant) -> bool {
        if !self.card_focused() {
            return false;
        }
        let items = self.cards.keys_mut().expire(now);
        let leaves = items.contains(&CardInput::Leave);
        self.apply_card_inputs(items, now);
        leaves
    }

    fn focused_card_screen<T>(&self, read: impl FnOnce(&vt100::Screen) -> T) -> Option<T> {
        let view = self.cards.focused_view()?;
        let screen = view.screen.lock().ok()?;
        Some(read(screen.parser.screen()))
    }

    fn apply_card_inputs(&mut self, items: Vec<CardInput>, now: Instant) {
        for item in items {
            match item {
                CardInput::Bytes(bytes) => self.cards.input(bytes),
                CardInput::Focus(true) => self.focus_gained(now),
                CardInput::Focus(false) => self.focus_lost(),
                CardInput::Mouse(mouse) => {
                    if !self.card_mouse(mouse) {
                        self.leave_card();
                        return;
                    }
                }
                CardInput::Leave => {
                    self.leave_card();
                    return;
                }
            }
        }
    }

    /// A mouse report while a card has the focus: inside the Tick's screen it reaches the Tick
    /// when the Tick asked for mouse reports, translated to its screen; a click anywhere else
    /// leaves the card. Returns whether the focus stays.
    fn card_mouse(&mut self, mouse: SgrMouse) -> bool {
        let Some(focused) = self.cards.focused().cloned() else {
            return false;
        };
        let (row, column) = (
            usize::from(mouse.row.saturating_sub(1)),
            usize::from(mouse.column.saturating_sub(1)),
        );
        let place = self.monitor_row_targets.iter().find_map(|target| {
            let MonitorTarget::Card {
                instance,
                row: tick_row,
            } = &target.target
            else {
                return None;
            };
            (target.row == row && instance == &focused && target.columns.contains(&column))
                .then(|| (*tick_row, target.columns.start + 1))
        });
        match place {
            Some((Some(tick_row), content_start)) if column >= content_start => {
                let tick_column = u16::try_from(column - content_start).unwrap_or(u16::MAX);
                let report = self.focused_card_screen(|screen| {
                    mouse.for_tick(
                        tick_column,
                        tick_row,
                        screen.mouse_protocol_mode(),
                        screen.mouse_protocol_encoding(),
                    )
                });
                if let Some(Some(report)) = report {
                    self.cards.input(report);
                }
                true
            }
            Some(_) => true,
            None => !mouse.is_click(),
        }
    }

    /// A key crossterm decoded before the input thread switched to the card's raw input: passed
    /// on as the Tick's terminal would send it; Esc leaves.
    pub(super) fn card_key(&mut self, key: KeyEvent) -> Option<InputEvent> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        if key.code == KeyCode::Esc {
            self.leave_card();
            return None;
        }
        let application_cursor = self
            .focused_card_screen(|screen| screen.application_cursor())
            .unwrap_or(false);
        if let Some(bytes) = card_key_bytes(key, application_cursor) {
            self.cards.input(bytes);
        }
        None
    }

    /// Text crossterm decoded as a paste before the input thread switched to raw input.
    pub(super) fn card_paste(&mut self, text: &str) {
        let markers = self
            .focused_card_screen(|screen| screen.bracketed_paste())
            .unwrap_or(false);
        let mut bytes = Vec::new();
        if markers {
            bytes.extend(b"\x1b[200~");
        }
        bytes.extend(text.as_bytes());
        if markers {
            bytes.extend(b"\x1b[201~");
        }
        self.cards.input(bytes);
    }

    /// The open cards of the shown Chat whose rows are still listed, for the next frame. A Chat
    /// switch or a row leaving the list closes its cards; a row on a page not loaded yet is
    /// still listed. A card whose Tick ended, as its row or its view tells, keeps its last screen
    /// and says how on its rule.
    pub(super) fn card_sources(&mut self) -> Vec<CardSource> {
        let monitor = &self.monitor;
        self.cards
            .retain(self.chat, |instance| monitor.lists(instance));
        self.card_sizes
            .retain(|instance, _| self.cards.is_open(instance));
        let focused = self.cards.focused().cloned();
        self.cards
            .cards()
            .iter()
            .map(|card| {
                let (screen, note) = match &card.state {
                    CardState::Opening => (None, Some(CardNote::plain(OPENING_NOTE))),
                    CardState::Failed(reason) => (None, Some(CardNote::plain(reason.clone()))),
                    CardState::Attached(view) => (
                        Some(view.screen.clone()),
                        self.row_end_note(&card.instance).or_else(|| {
                            view.screen.lock().ok().and_then(|screen| {
                                screen.ended.as_ref().map(|ended| self.view_end_note(ended))
                            })
                        }),
                    ),
                };
                CardSource {
                    instance: card.instance.clone(),
                    demand: CardDemand {
                        min_rows: self.card_min_rows(&card.instance),
                        priority: self.cards.priority(card),
                    },
                    focused: focused.as_ref() == Some(&card.instance),
                    screen,
                    note,
                }
            })
            .collect()
    }

    /// Whether the focused card's Tick ended, as its row or its view tells.
    pub(super) fn focused_card_ended(&self) -> bool {
        let Some(view) = self.cards.focused_view() else {
            return false;
        };
        self.cards
            .focused()
            .is_some_and(|instance| self.row_end_note(instance).is_some())
            || view
                .screen
                .lock()
                .is_ok_and(|screen| screen.ended.is_some())
    }

    /// How `instance`'s row says its Tick ended, in its state mark's colour.
    fn row_end_note(&self, instance: &InstanceRef) -> Option<CardNote> {
        let entry = self
            .monitor
            .entries()
            .iter()
            .find(|entry| &entry.instance == instance)?;
        let ended = entry.ended.clone()?;
        Some(CardNote::ended(
            ended,
            crate::terminal::list_row::state_mark(entry.tone, self.display, 0),
        ))
    }

    /// Why a card's view ended, before its row tells: its Tick ended, as a gone row will say, or
    /// the view failed by itself.
    fn view_end_note(&self, ended: &ViewEnd) -> CardNote {
        match ended {
            ViewEnd::TickEnded => CardNote::ended(
                crate::monitor_rows::TICK_ENDED.into(),
                crate::terminal::list_row::state_mark(
                    crate::monitor_rows::StatusTone::Gone,
                    self.display,
                    0,
                ),
            ),
            ViewEnd::Failed(reason) => CardNote::plain(reason.clone()),
        }
    }

    /// After a frame: each shown card's view takes the size the frame gave it, paced while the
    /// size keeps changing, or at once after a separator drag ends; the focus leaves a card the
    /// frame did not show, such as when a menu covers Monitor.
    pub(super) fn settle_cards(&mut self, sizes: Vec<(InstanceRef, CardSize)>) {
        let resizes_due = std::mem::take(&mut self.card_resizes_due);
        for (instance, size) in &sizes {
            if let Some(card) = self.cards.card_mut(instance)
                && let CardState::Attached(view) = &mut card.state
            {
                view.resize(*size);
                if resizes_due {
                    view.flush_resize();
                }
            }
            self.card_sizes.insert(instance.clone(), *size);
        }
        if let Some(focused) = self.cards.focused()
            && !sizes.iter().any(|(instance, _)| instance == focused)
        {
            self.cards.leave_focus();
        }
    }

    /// The real terminal's input modes for the focused card's Tick, or the Console's own.
    pub(super) fn card_terminal_modes(&mut self) -> String {
        let wanted = self
            .card_focused()
            .then(|| self.focused_card_screen(CardModes::of).unwrap_or_default());
        self.surface.apply_card_modes(wanted)
    }

    /// Where the terminal cursor goes for the focused card: the Tick's cursor inside the card,
    /// `None` inside when the Tick hides it or it lies outside what the card shows.
    pub(super) fn card_cursor(&self) -> Option<Option<(u16, u16)>> {
        let focused = self.cards.focused()?;
        let Some((hidden, (row, column))) =
            self.focused_card_screen(|screen| (screen.hide_cursor(), screen.cursor_position()))
        else {
            return Some(None);
        };
        if hidden {
            return Some(None);
        }
        Some(self.monitor_row_targets.iter().find_map(|target| {
            let MonitorTarget::Card {
                instance,
                row: Some(tick_row),
            } = &target.target
            else {
                return None;
            };
            let screen_column = target.columns.start + 1 + usize::from(column);
            (instance == focused && *tick_row == row && screen_column < target.columns.end).then(
                || {
                    (
                        u16::try_from(screen_column).unwrap_or(u16::MAX),
                        u16::try_from(target.row).unwrap_or(u16::MAX),
                    )
                },
            )
        }))
    }

    /// A left press on a Monitor row whose Tick has a card, or on a card: acts on the release,
    /// so a drag never toggles. Returns whether the press is a card's.
    pub(super) fn press_card(&mut self, at: Cell) -> bool {
        let Some(target) = self
            .monitor_row_targets
            .iter()
            .find(|target| {
                Some(target.chat) == self.chat
                    && target.row == at.row
                    && target.columns.contains(&at.column)
            })
            .map(|target| target.target.clone())
        else {
            return false;
        };
        let is_card = match &target {
            MonitorTarget::Card { .. } => true,
            MonitorTarget::Tick(instance) => self.row_toggles_card(instance),
            _ => false,
        };
        if is_card {
            self.card_press = Some((at, target));
        }
        is_card
    }

    /// The release of a card press: a click on the row toggles its card like Space, a click in
    /// an open card enters it like Enter; a release elsewhere, after a drag, does nothing.
    pub(super) fn release_card(&mut self, at: Cell) -> Option<InputEvent> {
        let (pressed, target) = self.card_press.take()?;
        if pressed.row != at.row {
            return None;
        }
        self.clear_selection();
        self.clear_footer_selection();
        self.conversation_focused = false;
        self.monitor.focus(self.monitor.overlay, None);
        self.monitor.select_target(&target);
        match target {
            MonitorTarget::Tick(instance) => self.toggle_card(instance),
            MonitorTarget::Card { instance, .. } => self.enter_card(instance),
            _ => None,
        }
    }

    /// The key hints of the focused Monitor list, with Space on a row whose Tick has a card.
    pub(super) fn monitor_list_hint(&self) -> String {
        if let Some(focused) = self.cards.focused() {
            return if focused.shell_task().is_some() {
                crate::terminal::monitor_cards::FOCUSED_SHELL_CARD_HINT
            } else {
                crate::terminal::monitor_cards::FOCUSED_CARD_HINT
            }
            .to_owned();
        }
        let hint = self.monitor.hint();
        let Some(entry) = self.monitor.selected() else {
            return hint;
        };
        if !self.row_toggles_card(&entry.instance) {
            return hint;
        }
        let space = if self.cards.is_open(&entry.instance) {
            "space hide"
        } else {
            "space show"
        };
        match hint.rsplit_once(" · ") {
            Some((keys, last)) => format!("{keys} · {space} · {last}"),
            None => format!("{space} · {hint}"),
        }
    }
}

/// The bytes a legacy terminal sends for `key`, Alt as an Esc prefix; arrows, Home and End in
/// their SS3 form when the Tick asked for `application_cursor` keys. `None` for keys such a
/// terminal cannot send.
pub(super) fn card_key_bytes(key: KeyEvent, application_cursor: bool) -> Option<Vec<u8>> {
    const ESCAPE: u8 = 0x1b;
    /// F5 to F12 in the `CSI n ~` form; F1 to F4 are SS3 `P` to `S`.
    const FUNCTION_KEY_CODES: [u8; 8] = [15, 17, 18, 19, 20, 21, 23, 24];
    let cursor = |final_byte: u8| {
        let introducer = if application_cursor { b'O' } else { b'[' };
        vec![ESCAPE, introducer, final_byte]
    };
    let tilde = |code: u8| format!("\x1b[{code}~").into_bytes();
    let bytes = match (key.code, key.modifiers.difference(KeyModifiers::ALT)) {
        (KeyCode::Char(character), KeyModifiers::CONTROL) if character.is_ascii_alphabetic() => {
            vec![(character.to_ascii_lowercase() as u8) & 0x1f]
        }
        (KeyCode::Char(character), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
            character.to_string().into_bytes()
        }
        (KeyCode::Enter, _) => b"\r".to_vec(),
        (KeyCode::Backspace, _) => vec![0x7f],
        (KeyCode::Tab, _) => b"\t".to_vec(),
        (KeyCode::BackTab, _) => b"\x1b[Z".to_vec(),
        (KeyCode::Up, _) => cursor(b'A'),
        (KeyCode::Down, _) => cursor(b'B'),
        (KeyCode::Right, _) => cursor(b'C'),
        (KeyCode::Left, _) => cursor(b'D'),
        (KeyCode::Home, _) => cursor(b'H'),
        (KeyCode::End, _) => cursor(b'F'),
        (KeyCode::Insert, _) => tilde(2),
        (KeyCode::Delete, _) => tilde(3),
        (KeyCode::PageUp, _) => tilde(5),
        (KeyCode::PageDown, _) => tilde(6),
        (KeyCode::F(number @ 1..=4), _) => vec![ESCAPE, b'O', b'P' + number - 1],
        (KeyCode::F(number @ 5..=12), _) => tilde(FUNCTION_KEY_CODES[usize::from(number - 5)]),
        _ => return None,
    };
    Some(if key.modifiers.contains(KeyModifiers::ALT) {
        [vec![ESCAPE], bytes].concat()
    } else {
        bytes
    })
}

impl Console {
    /// Attaches `instance`'s card view with `grant` while the card still waits for it.
    pub(in crate::terminal) async fn attach_card(
        &self,
        chat: Uuid,
        instance: InstanceRef,
        grant: tickloop_harness::protocol::HostedViewGrant,
    ) -> Result<(), String> {
        let (size, redraw, start_painter) = {
            let mut state = self.state.lock().map_err(|error| error.to_string())?;
            if state.chat != Some(chat) || !state.cards.is_open(&instance) {
                return Ok(());
            }
            let size = state.card_size(&instance);
            let start_painter = state.cards.take_painter_start();
            (size, state.cards.redraw(), start_painter)
        };
        if start_painter {
            spawn_card_painter(Arc::downgrade(&self.state), redraw.clone());
        }
        let attached = CardView::attach(&grant, size, redraw).await;
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        match attached {
            Ok(view) => {
                state.cards.attached(chat, &instance, view);
            }
            Err(error) => state.cards.failed(
                &instance,
                super::super::safe_line(&format!("Cannot open: {}", error.message)),
            ),
        }
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())
    }

    pub(in crate::terminal) fn card_failed(
        &self,
        instance: &InstanceRef,
        reason: &str,
    ) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        state
            .cards
            .failed(instance, super::super::safe_line(reason));
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())
    }
}

/// Redraws the screen when a card's frame arrives, at most once per [`CARD_FRAME_INTERVAL`], and
/// only while a card is on screen: a hidden card stays attached, and the next frame that shows it
/// draws its latest screen.
fn spawn_card_painter(state: std::sync::Weak<Mutex<State>>, redraw: Arc<tokio::sync::Notify>) {
    tokio::spawn(async move {
        loop {
            redraw.notified().await;
            let Some(state) = state.upgrade() else {
                return;
            };
            if let Ok(mut state) = state.lock()
                && state.card_in_view()
            {
                let _ = state.draw(&mut io::stdout().lock());
            }
            drop(state);
            tokio::time::sleep(CARD_FRAME_INTERVAL).await;
        }
    });
}
