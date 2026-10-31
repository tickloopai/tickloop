//! A full-screen view: shows a hosted Tick's terminal or a Shell's screen on the whole terminal
//! but its last row, which lists the view's keys and takes clicks on them, until the user hides
//! it. A Tick gets the user's keys unchanged; a Shell's screen is read-only. Other views may show the same Tick at once, and its terminal takes the
//! size of whichever view typed last, so this terminal can be smaller or larger than the screen
//! it shows.

use std::{
    io::Write,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use serde::Deserialize;
use serde_json::{Value, json};
use tick_protocol::BubbledKey;
use tickhost_connect::{
    host_error::HostError,
    hosted::{
        self, ATTACH_INPUT_BYTES, ATTACH_REQUEST_FRAME_BYTES, BUSINESS_FRAME_BYTES,
        CAPABILITY_DATA, CONTROL_TIMEOUT, HostConnection, MAX_COLUMNS, MAX_ROWS, MIN_COLUMNS,
        MIN_ROWS, Role,
    },
};
use tickloop_harness::protocol::HostedViewGrant;

use unicode_width::UnicodeWidthChar;

use super::monitor_cards::{CardInput, CardKeys};

/// A second lone Esc within this time of the one the Tick got hides the view.
pub(super) const FULL_SCREEN_ESCAPE_WINDOW: Duration = Duration::from_secs(1);
/// The rows below the Tick's screen that the view keeps for its keys.
const HINT_ROWS: u16 = 1;
const HIDE_KEYS: &str = "esc esc hide";
/// Shown on the bottom row while a second Esc would hide the view.
const HIDE_HINT: &str = "Press esc again to hide";
const SHELL_STOP_KEYS: &str = "ctrl+c stop · ";
const SHELL_SCROLL_KEYS: &str = "↑↓ scroll · G latest · ";
const ESCAPE_HIDE_KEYS: &str = "esc hide";
const REOPEN_KEYS: &str = "enter reopen";
const HINT_SEPARATOR: &str = " · ";
const INTERRUPT: u8 = 0x03;
const ENTER: u8 = b'\r';

/// Columns and rows assumed when the terminal cannot report its size.
const FALLBACK_TERMINAL_SIZE: (u16, u16) = (80, 24);
/// At most this many key reads wait for the Tick; a busy Tick slows typing instead of losing it.
const QUEUED_KEY_READS: usize = 16;
/// How often the key reader looks whether its view has ended.
const KEY_POLL_MILLISECONDS: i32 = 100;
/// The view draws on its own screen and leaves the user's scrollback untouched. It pushes empty
/// kitty keyboard flags on that screen, so it reads legacy keys, the ones the Tick's terminal
/// takes, whatever flags an earlier program left there.
const TAKE_SCREEN: &str = "\x1b[?1049h\x1b[>0u\x1b[H\x1b[2J";
/// Undoes every mode the Tick's frames may have turned on: colours, a hidden cursor, mouse and
/// paste reporting, application cursor and keypad keys; then pops the view's keyboard flags while
/// its own screen, which it leaves last, is still shown.
const RESTORE_TERMINAL: &str = "\x1b[0m\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?2004l\x1b[?1l\x1b>\x1b[<u\x1b[?1049l";
/// Starts a redraw of a screen of another size: no cursor, default colours, nothing shown.
const BEGIN_REDRAW: &[u8] = b"\x1b[?25l\x1b[m\x1b[H\x1b[J";
const DEFAULT_COLOURS: &[u8] = b"\x1b[m";
const SHOW_CURSOR: &str = "\x1b[?25h";
/// Saves and restores the cursor with the drawing colours, around the hint drawn over the Tick.
const SAVE_CURSOR: &str = "\x1b7";
const RESTORE_CURSOR: &str = "\x1b8";
const INVERSE: &str = "\x1b[7m";
const DIM: &str = "\x1b[2m";
/// Marks the keys on the hint row that take a click.
const UNDERLINE: &str = "\x1b[4m";
const NO_UNDERLINE: &str = "\x1b[24m";
/// Has the terminal report clicks and drags, in the encoding the view reads, while the Tick
/// does not ask for mouse reports itself; a Tick that asks keeps the reports it chose.
const CAPTURE_CLICKS: &str = "\x1b[?1002h\x1b[?1006h";
/// Shown on the hint row a moment after a selection was copied.
const COPIED_HINT: &str = "Copied";
const COPIED_HINT_TIME: Duration = Duration::from_millis(1500);
const READABLE_MOUSE_ENCODING: &str = "\x1b[?1006h";
const ARROW_UP: &[u8] = b"\x1b[A";
const ARROW_DOWN: &[u8] = b"\x1b[B";
const APPLICATION_ARROW_UP: &[u8] = b"\x1bOA";
const APPLICATION_ARROW_DOWN: &[u8] = b"\x1bOB";

/// What the user's keys do in a full-screen view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ViewInput {
    /// A Tick: keys reach it, and a second Esc within [`FULL_SCREEN_ESCAPE_WINDOW`] hides. Once
    /// its program ended, a lone Esc hides and, with `reopenable`, Enter reopens it.
    Interactive { reopenable: bool },
    /// A Shell's screen: a lone Esc hides at once and its keys only scroll this view's place in
    /// the output; with `stoppable`, Ctrl+C stops the task.
    ReadOnly { stoppable: bool },
}

/// How a full-screen view ended.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum AttachEnd {
    /// The user hid it; the Tick keeps running.
    Hidden,
    /// The Tick was closed or lost while it was shown.
    Gone,
    /// Enter on a Tick whose program ended: reopen it in its row's place.
    Reopen,
    /// The host ended the view, as it does for one that fell behind.
    Lost(String),
}

/// A view attached to a Tick terminal or a Shell's screen that has not taken this terminal yet,
/// so a refused attach never disturbs the screen.
pub(super) struct AttachedView {
    events: tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>,
    requests: tokio::net::unix::OwnedWriteHalf,
    input: ViewInput,
}

impl AttachedView {
    /// Attaches a view of what `grant` names at this terminal's size. The grant is single-use
    /// and soon expires, so the view attaches right after the grant arrives.
    pub(super) async fn attach(
        grant: &HostedViewGrant,
        input: ViewInput,
    ) -> Result<Self, HostError> {
        let mut connection = HostConnection::connect(
            &grant.endpoint,
            Role::Viewer {
                instance: grant.instance.clone(),
                view: grant.view,
            },
            &[CAPABILITY_DATA],
        )
        .await?;
        if connection.epoch() != grant.host_epoch {
            return Err(HostError::before(
                "EpochChanged",
                "the Tickhost was replaced since this view was granted",
            ));
        }
        connection
            .request(
                "attach",
                json!({"size":requested_size(tick_area(terminal_size()))}),
                CONTROL_TIMEOUT,
            )
            .await?;
        let (events, requests) = connection.into_parts();
        Ok(Self {
            events,
            requests,
            input,
        })
    }

    /// Takes this terminal, which must already be in raw mode, until the user hides the view or
    /// it ends; the terminal is restored afterwards. On a stoppable Shell's screen Ctrl+C calls
    /// `interrupt`.
    pub(super) async fn run(self, mut interrupt: impl FnMut()) -> Result<AttachEnd, HostError> {
        let Self {
            mut events,
            mut requests,
            input,
        } = self;
        write_terminal(TAKE_SCREEN.as_bytes());
        let stop = Arc::new(AtomicBool::new(false));
        let (keys, mut typed) = tokio::sync::mpsc::channel(QUEUED_KEY_READS);
        let reader = {
            let stop = stop.clone();
            std::thread::spawn(move || read_keys(&stop, &keys))
        };
        let outcome = async {
            let mut resized =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
                    .map_err(|error| HostError::before("TerminalUnavailable", error.to_string()))?;
            let mut terminal = terminal_size();
            let mut screen = ViewScreen::new(tick_area(terminal));
            let mut filter = FullScreenKeys::new(input);
            let mut bubbling = false;
            let mut shown_hint = filter.hint(Instant::now());
            let mut output = CAPTURE_CLICKS.as_bytes().to_vec();
            output.extend(hint_row(terminal, &shown_hint));
            write_terminal(&output);
            loop {
                let keys_due = filter.deadline().map(tokio::time::Instant::from_std);
                let hint_due = filter
                    .hint_changes_at(Instant::now())
                    .map(tokio::time::Instant::from_std);
                let actions = tokio::select! {
                    event = hosted::read_frame(&mut events, BUSINESS_FRAME_BYTES) => {
                        let Some(event) = event? else {
                            return Ok(AttachEnd::Lost("the Tickhost closed this view".into()));
                        };
                        match event["event"].as_str() {
                            Some("frame") => {
                                let size = ScreenSize::deserialize(&event["size"]).map_err(|_| {
                                    HostError::after("ProtocolViolation", "a frame did not name its screen size")
                                })?;
                                let data = event["data"].as_str().unwrap_or_default();
                                let mut output = screen.frame(size, data.as_bytes());
                                output.extend(mouse_reports(screen.tick.screen()));
                                // A frame may clear the whole terminal, the hint row with it.
                                output.extend(hint_row(terminal, &shown_hint));
                                write_terminal(&output);
                            }
                            // The Tick's program or the Shell ended: its last screen stays
                            // shown, saying how, and only hiding or reopening is left.
                            Some("exited") => {
                                let exit = Exit::deserialize(&event["exit"]).unwrap_or_default();
                                filter.end(crate::monitor_rows::exit_status(exit.code, exit.signal));
                            }
                            Some("gone") => return Ok(AttachEnd::Gone),
                            Some("bubbling") => bubbling = event["enabled"].as_bool().unwrap_or(false),
                            Some("bubble") => {
                                let key = event["key"].as_str().and_then(BubbledKey::from_name);
                                if matches!(input, ViewInput::Interactive { .. }) && !filter.has_ended() && bubbling && key == Some(BubbledKey::Esc) {
                                    send(&mut requests, json!({"method":"detach"})).await?;
                                    return Ok(AttachEnd::Hidden);
                                }
                            }
                            Some("focus_reports") => {
                                if matches!(input, ViewInput::Interactive { .. }) && event["enabled"].as_bool() == Some(true) {
                                    send(&mut requests, json!({"method":"focus","params":{"focused":true}})).await?;
                                }
                            }
                            _ => return Ok(AttachEnd::Lost(view_error(&event))),
                        }
                        Vec::new()
                    }
                    keys = typed.recv() => match keys {
                        Some(keys) => filter.feed(&keys, Instant::now(), screen.tick.screen(), terminal),
                        None => vec![KeyAction::Hide],
                    },
                    () = sleep_until(keys_due), if keys_due.is_some() => {
                        filter.expire(Instant::now(), screen.tick.screen(), terminal)
                    }
                    () = sleep_until(hint_due), if hint_due.is_some() => Vec::new(),
                    _ = resized.recv() => {
                        terminal = terminal_size();
                        let mut output = screen.resize_terminal(tick_area(terminal));
                        output.extend(mouse_reports(screen.tick.screen()));
                        output.extend(hint_row(terminal, &shown_hint));
                        write_terminal(&output);
                        send(&mut requests, json!({"method":"resize","params":{"size":requested_size(tick_area(terminal))}})).await?;
                        Vec::new()
                    }
                };
                for action in actions {
                    match action {
                        KeyAction::Input(bytes) => {
                            for chunk in bytes.chunks(ATTACH_INPUT_BYTES) {
                                send(
                                    &mut requests,
                                    json!({"method":"input","params":{"data":chunk}}),
                                )
                                .await?;
                            }
                        }
                        KeyAction::Interrupt => interrupt(),
                        KeyAction::Reopen => return Ok(AttachEnd::Reopen),
                        KeyAction::Select(selection) => {
                            let mut output = screen.draw();
                            output.extend(screen.highlight(selection));
                            output.extend(hint_row(terminal, &shown_hint));
                            write_terminal(&output);
                        }
                        KeyAction::Unselect => {
                            let mut output = screen.draw();
                            output.extend(hint_row(terminal, &shown_hint));
                            write_terminal(&output);
                        }
                        KeyAction::Copy(selection) => {
                            let text = screen.selected_text(selection);
                            if !text.trim().is_empty() {
                                let mut output = std::io::stdout().lock();
                                if super::clipboard::Clipboard::detect()
                                    .write(&text, &mut output)
                                    .is_ok()
                                {
                                    filter.copied(Instant::now());
                                }
                            }
                        }
                        KeyAction::Hide => {
                            send(&mut requests, json!({"method":"detach"})).await?;
                            return Ok(AttachEnd::Hidden);
                        }
                    }
                }
                let wanted = filter.hint(Instant::now());
                if wanted != shown_hint {
                    write_terminal(&hint_row(terminal, &wanted));
                    shown_hint = wanted;
                }
            }
        }
        .await;
        stop.store(true, Ordering::Release);
        drop(typed);
        let _ = reader.join();
        write_terminal(RESTORE_TERMINAL.as_bytes());
        outcome
    }
}

async fn sleep_until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

async fn send(
    requests: &mut tokio::net::unix::OwnedWriteHalf,
    request: Value,
) -> Result<(), HostError> {
    hosted::write_frame(requests, &request, ATTACH_REQUEST_FRAME_BYTES).await
}

fn view_error(event: &Value) -> String {
    event["error"]["message"]
        .as_str()
        .unwrap_or("the Tickhost ended this view")
        .to_owned()
}

/// What a read of the user's keys asks of the view, in order.
#[derive(Debug, PartialEq, Eq)]
enum KeyAction {
    /// Bytes the Tick receives; on a Shell's screen, the keys that scroll it.
    Input(Vec<u8>),
    /// Ctrl+C on a stoppable Shell's screen: stop the task, and stay.
    Interrupt,
    /// Enter on a reopenable Tick whose program ended.
    Reopen,
    /// A drag over the Tick's screen selects what it covers so far.
    Select(Selection),
    /// The drag ended: copy what it selected, which stays shown.
    Copy(Selection),
    /// A new click: the selection shown goes.
    Unselect,
    /// Hide the view; nothing read after it reaches the Tick.
    Hide,
}

/// The user's keys in a full-screen view, split as a focused card splits them but with a longer
/// Esc pair: a lone Esc reaches the Tick at once and a second within
/// [`FULL_SCREEN_ESCAPE_WINDOW`] hides the view. On a read-only screen a lone Esc hides, Ctrl+C
/// interrupts a stoppable task and the other keys go to the host, which only scrolls with them.
/// Once the Tick's program ended, a lone Esc hides and Enter reopens a reopenable Tick; nothing
/// else reaches it. A click on the hint row's keys acts as they say. The terminal's focus reports
/// never reach the Tick, and a click on its screen does only when the Tick asked for mouse
/// reports, at its place on the Tick's screen; without them the wheel moves as the arrow keys.
struct FullScreenKeys {
    keys: CardKeys,
    input: ViewInput,
    /// How the Tick's program or the Shell ended, once it did.
    ended: Option<String>,
    /// Where the drag selecting text began, while the left button is held.
    drag_from: Option<(u16, u16)>,
    /// Whether a selection is drawn over the screen.
    shows_selection: bool,
    /// Until when the hint row says a selection was copied.
    copied_until: Option<Instant>,
}

impl FullScreenKeys {
    fn new(input: ViewInput) -> Self {
        Self {
            keys: CardKeys::with_escape_window(FULL_SCREEN_ESCAPE_WINDOW),
            input,
            ended: None,
            drag_from: None,
            shows_selection: false,
            copied_until: None,
        }
    }

    /// A selection was copied at `now`; the hint row says so a moment.
    fn copied(&mut self, now: Instant) {
        self.copied_until = Some(now + COPIED_HINT_TIME);
    }

    /// When the hint row next changes by itself: the Esc hint or the copy notice running out.
    fn hint_changes_at(&self, now: Instant) -> Option<Instant> {
        let copied = self.copied_until.filter(|until| now < *until);
        [self.hint_until(now), copied].into_iter().flatten().min()
    }

    /// The Tick's program or the Shell ended as `status` says: a Shell can no longer be stopped.
    fn end(&mut self, status: String) {
        self.ended = Some(status);
        if let ViewInput::ReadOnly { stoppable } = &mut self.input {
            *stoppable = false;
        }
    }

    fn has_ended(&self) -> bool {
        self.ended.is_some()
    }

    fn is_read_only(&self) -> bool {
        self.has_ended() || matches!(self.input, ViewInput::ReadOnly { .. })
    }

    fn feed(
        &mut self,
        input: &[u8],
        now: Instant,
        tick: &vt100::Screen,
        terminal: ScreenSize,
    ) -> Vec<KeyAction> {
        let read_only = self.is_read_only();
        let items = self
            .keys
            .feed(input, now, tick.bracketed_paste(), read_only);
        self.actions(items, now, tick, terminal)
    }

    fn expire(
        &mut self,
        now: Instant,
        tick: &vt100::Screen,
        terminal: ScreenSize,
    ) -> Vec<KeyAction> {
        let read_only = self.is_read_only();
        let items = self.keys.expire(now, read_only);
        self.actions(items, now, tick, terminal)
    }

    fn deadline(&self) -> Option<Instant> {
        self.keys.deadline()
    }

    /// Until when the hint shows that a second Esc hides the view, while it would.
    fn hint_until(&self, now: Instant) -> Option<Instant> {
        self.keys
            .escape_sent_at()
            .map(|at| at + FULL_SCREEN_ESCAPE_WINDOW)
            .filter(|until| now < *until)
    }

    /// What the hint row says at `now`, and which of its keys take a click.
    fn hint(&self, now: Instant) -> Hint {
        if self.copied_until.is_some_and(|until| now < until) {
            return Hint::new(INVERSE, vec![HintPart::text(COPIED_HINT)]);
        }
        if self.hint_until(now).is_some() {
            return Hint::new(INVERSE, vec![HintPart::button(HIDE_HINT, HintButton::Hide)]);
        }
        match (self.input, &self.ended) {
            (ViewInput::Interactive { reopenable }, Some(ended)) => {
                let mut parts = vec![HintPart::text(ended), HintPart::text(HINT_SEPARATOR)];
                if reopenable {
                    parts.push(HintPart::button(REOPEN_KEYS, HintButton::Reopen));
                    parts.push(HintPart::text(HINT_SEPARATOR));
                }
                parts.push(HintPart::button(ESCAPE_HIDE_KEYS, HintButton::Hide));
                Hint::new(INVERSE, parts)
            }
            (ViewInput::Interactive { .. }, None) => {
                Hint::new(DIM, vec![HintPart::button(HIDE_KEYS, HintButton::Hide)])
            }
            (ViewInput::ReadOnly { stoppable }, _) => {
                let mut parts = Vec::new();
                if stoppable {
                    parts.push(HintPart::text(SHELL_STOP_KEYS));
                }
                parts.push(HintPart::text(SHELL_SCROLL_KEYS));
                parts.push(HintPart::button(ESCAPE_HIDE_KEYS, HintButton::Hide));
                Hint::new(DIM, parts)
            }
        }
    }

    fn actions(
        &mut self,
        items: Vec<CardInput>,
        now: Instant,
        tick: &vt100::Screen,
        terminal: ScreenSize,
    ) -> Vec<KeyAction> {
        let mut actions = Vec::new();
        for item in items {
            match item {
                CardInput::Leave => {
                    actions.push(KeyAction::Hide);
                    break;
                }
                CardInput::Bytes(bytes) => match self.input {
                    ViewInput::Interactive { reopenable } if self.has_ended() => {
                        if reopenable && bytes.contains(&ENTER) {
                            actions.push(KeyAction::Reopen);
                            break;
                        }
                    }
                    ViewInput::Interactive { .. } => actions.push(KeyAction::Input(bytes)),
                    ViewInput::ReadOnly { stoppable } => {
                        for keys in bytes.split_inclusive(|byte| *byte == INTERRUPT) {
                            let (keys, interrupts) = match keys.strip_suffix(&[INTERRUPT]) {
                                Some(keys) => (keys, true),
                                None => (keys, false),
                            };
                            if !keys.is_empty() {
                                actions.push(KeyAction::Input(keys.to_vec()));
                            }
                            if interrupts && stoppable {
                                actions.push(KeyAction::Interrupt);
                            }
                        }
                    }
                },
                CardInput::Mouse(mouse) => {
                    let (column, row) =
                        (mouse.column.saturating_sub(1), mouse.row.saturating_sub(1));
                    let asks_for_mouse =
                        tick.mouse_protocol_mode() != vt100::MouseProtocolMode::None;
                    let takes_mouse = self.is_read_only() || !asks_for_mouse;
                    let (rows, columns) = tick.size();
                    // A drag selects the Tick's screen as far as it reaches, the hint row
                    // standing for its last row.
                    let at = (
                        row.min(rows.saturating_sub(1)),
                        column.min(columns.saturating_sub(1)),
                    );
                    if let Some(from) = self.drag_from {
                        if mouse.is_left_drag() {
                            self.shows_selection = true;
                            actions.push(KeyAction::Select(Selection::between(from, at)));
                            continue;
                        }
                        if mouse.is_release() {
                            self.drag_from = None;
                            if from != at {
                                actions.push(KeyAction::Copy(Selection::between(from, at)));
                            }
                            continue;
                        }
                    }
                    if row + HINT_ROWS >= terminal.rows {
                        let pressed = mouse
                            .is_left_click()
                            .then(|| self.hint(now).button_at(column))
                            .flatten();
                        match pressed {
                            Some(HintButton::Hide) => {
                                actions.push(KeyAction::Hide);
                                break;
                            }
                            Some(HintButton::Reopen) => {
                                actions.push(KeyAction::Reopen);
                                break;
                            }
                            None => continue,
                        }
                    }
                    if mouse.is_left_click() && std::mem::take(&mut self.shows_selection) {
                        actions.push(KeyAction::Unselect);
                    }
                    if takes_mouse && mouse.is_left_click() && row < rows && column < columns {
                        self.drag_from = Some((row, column));
                        continue;
                    }
                    if let Some(up) = mouse.wheel_scrolls_up()
                        && takes_mouse
                    {
                        if self.has_ended() && matches!(self.input, ViewInput::Interactive { .. }) {
                            continue;
                        }
                        let arrow = match (up, tick.application_cursor()) {
                            (true, false) => ARROW_UP,
                            (false, false) => ARROW_DOWN,
                            (true, true) => APPLICATION_ARROW_UP,
                            (false, true) => APPLICATION_ARROW_DOWN,
                        };
                        actions.push(KeyAction::Input(arrow.to_vec()));
                        continue;
                    }
                    if self.is_read_only() {
                        continue;
                    }
                    if row < rows
                        && column < columns
                        && let Some(report) = mouse.for_tick(
                            column,
                            row,
                            tick.mouse_protocol_mode(),
                            tick.mouse_protocol_encoding(),
                        )
                    {
                        actions.push(KeyAction::Input(report));
                    }
                }
                CardInput::Focus(_) => {}
            }
        }
        actions
    }
}

/// Text a drag selected on the Tick's screen, from `from` through `to` in reading order, both
/// `(row, column)` and both included.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Selection {
    from: (u16, u16),
    to: (u16, u16),
}

impl Selection {
    fn between(one: (u16, u16), other: (u16, u16)) -> Self {
        Self {
            from: one.min(other),
            to: one.max(other),
        }
    }

    /// Each row it covers with its columns, the end excluded, within `columns`.
    fn rows(self, columns: u16) -> impl Iterator<Item = (u16, u16, u16)> {
        (self.from.0..=self.to.0).map(move |row| {
            let start = if row == self.from.0 { self.from.1 } else { 0 };
            let end = if row == self.to.0 {
                self.to.1 + 1
            } else {
                columns
            };
            (row, start, end.min(columns))
        })
    }
}

/// What a click on a key of the hint row does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HintButton {
    Hide,
    Reopen,
}

/// The view's last row: its keys in `style`, the ones a click takes underlined.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Hint {
    style: &'static str,
    parts: Vec<HintPart>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct HintPart {
    text: String,
    button: Option<HintButton>,
}

impl HintPart {
    fn text(text: &str) -> Self {
        Self {
            text: text.to_owned(),
            button: None,
        }
    }

    fn button(text: &str, button: HintButton) -> Self {
        Self {
            text: text.to_owned(),
            button: Some(button),
        }
    }
}

impl Hint {
    fn new(style: &'static str, parts: Vec<HintPart>) -> Self {
        Self { style, parts }
    }

    /// The key drawn at `column`, counted from 0.
    fn button_at(&self, column: u16) -> Option<HintButton> {
        let mut start = 0;
        for part in &self.parts {
            let width = part
                .text
                .chars()
                .map(|character| character.width().unwrap_or(0))
                .sum::<usize>();
            if (start..start + width).contains(&usize::from(column)) {
                return part.button;
            }
            start += width;
        }
        None
    }
}

/// The hint row drawn on the bottom row of `terminal`, cut at its width, without moving the
/// Tick's cursor or changing its colours.
fn hint_row(terminal: ScreenSize, hint: &Hint) -> Vec<u8> {
    let mut row = format!(
        "{SAVE_CURSOR}\x1b[{};1H\x1b[m\x1b[2K{}",
        terminal.rows, hint.style
    );
    let mut room = usize::from(terminal.columns);
    for part in &hint.parts {
        if part.button.is_some() {
            row.push_str(UNDERLINE);
        }
        for character in part.text.chars() {
            let width = character.width().unwrap_or(0);
            if width > room {
                room = 0;
                break;
            }
            room -= width;
            row.push(character);
        }
        if part.button.is_some() {
            row.push_str(NO_UNDERLINE);
        }
    }
    row.push_str("\x1b[m");
    row.push_str(RESTORE_CURSOR);
    row.into_bytes()
}

/// The mouse reports the terminal makes after a frame of `tick`: clicks for the hint row while
/// the Tick asks for none, in the encoding the view reads either way.
fn mouse_reports(tick: &vt100::Screen) -> &'static [u8] {
    if tick.mouse_protocol_mode() == vt100::MouseProtocolMode::None {
        CAPTURE_CLICKS.as_bytes()
    } else {
        READABLE_MOUSE_ENCODING.as_bytes()
    }
}

/// How a Tick's program or a Shell ended, as the host's `exited` event tells.
#[derive(Debug, Default, Deserialize)]
struct Exit {
    code: Option<i32>,
    signal: Option<i32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
struct ScreenSize {
    rows: u16,
    columns: u16,
}

/// The part of `terminal` the Tick's screen takes: all but the hint row.
fn tick_area(terminal: ScreenSize) -> ScreenSize {
    ScreenSize {
        rows: terminal.rows.saturating_sub(HINT_ROWS).max(1),
        columns: terminal.columns,
    }
}

/// This terminal's size, at least one row and one column.
fn terminal_size() -> ScreenSize {
    let (columns, rows) = crossterm::terminal::size().unwrap_or(FALLBACK_TERMINAL_SIZE);
    ScreenSize {
        rows: rows.max(1),
        columns: columns.max(1),
    }
}

/// The size a view of `terminal` asks the Tick terminal for, within the bounds it accepts.
fn requested_size(terminal: ScreenSize) -> Value {
    json!({
        "rows": terminal.rows.clamp(MIN_ROWS, MAX_ROWS),
        "columns": terminal.columns.clamp(MIN_COLUMNS, MAX_COLUMNS),
    })
}

/// What the area of this terminal above the hint row shows of the Tick's screen. A screen of the
/// area's size is drawn as the host frames it; one of another size is redrawn whole from its
/// top-left corner, cut where the area ends and blank where the area reaches beyond it.
struct ViewScreen {
    tick: vt100::Parser,
    /// The part of this terminal the Tick's screen takes.
    area: ScreenSize,
}

impl ViewScreen {
    fn new(area: ScreenSize) -> Self {
        Self {
            tick: vt100::Parser::default(),
            area,
        }
    }

    /// The output that shows a frame the host drew for a Tick screen of `size`.
    fn frame(&mut self, size: ScreenSize, data: &[u8]) -> Vec<u8> {
        if self.tick_size() != size {
            self.tick.screen_mut().set_size(size.rows, size.columns);
        }
        self.tick.process(data);
        if size == self.area {
            data.to_vec()
        } else {
            self.draw()
        }
    }

    /// The output that shows the Tick's screen once its area is `area`.
    fn resize_terminal(&mut self, area: ScreenSize) -> Vec<u8> {
        self.area = area;
        self.draw()
    }

    /// The text `selection` covers, its rows' trailing blanks left out.
    fn selected_text(&self, selection: Selection) -> String {
        let screen = self.tick.screen();
        let (_, columns) = screen.size();
        selection
            .rows(columns)
            .map(|(row, start, end)| {
                screen
                    .contents_between(row, start, row, end)
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Draws `selection` over the shown screen in inverse, within the part this terminal shows.
    fn highlight(&self, selection: Selection) -> Vec<u8> {
        let screen = self.tick.screen();
        let columns = self.tick_size().columns.min(self.area.columns);
        let mut output = SAVE_CURSOR.as_bytes().to_vec();
        for (row, start, end) in selection.rows(columns) {
            if row >= self.area.rows || start >= end {
                continue;
            }
            let text = screen.contents_between(row, start, row, end);
            let width = unicode_width::UnicodeWidthStr::width(text.as_str());
            let padding = usize::from(end - start).saturating_sub(width);
            output.extend(
                format!(
                    "\x1b[{};{}H\x1b[m{INVERSE}{text}{}\x1b[m",
                    row + 1,
                    start + 1,
                    " ".repeat(padding)
                )
                .as_bytes(),
            );
        }
        output.extend(RESTORE_CURSOR.as_bytes());
        output
    }

    fn tick_size(&self) -> ScreenSize {
        let (rows, columns) = self.tick.screen().size();
        ScreenSize { rows, columns }
    }

    /// The whole Tick screen as this terminal shows it. At the same size it is drawn as the host
    /// would draw it whole, so the host's later changes apply to it as they come.
    fn draw(&self) -> Vec<u8> {
        let screen = self.tick.screen();
        let tick = self.tick_size();
        if tick == self.area {
            return screen.state_formatted();
        }
        let rows = tick.rows.min(self.area.rows);
        let columns = tick.columns.min(self.area.columns);
        let is_cut = columns < tick.columns;
        // A wide character starting in the last column shown would spill past this terminal's
        // edge, so such a row leaves that column blank.
        let narrowed = is_cut.then(|| {
            screen
                .rows_formatted(0, columns - 1)
                .take(rows.into())
                .collect::<Vec<_>>()
        });
        let mut output = BEGIN_REDRAW.to_vec();
        for (row, cells) in (0..rows).zip(screen.rows_formatted(0, columns)) {
            output.extend(format!("\x1b[{};1H", row + 1).as_bytes());
            output.extend(DEFAULT_COLOURS);
            match &narrowed {
                Some(narrowed)
                    if screen
                        .cell(row, columns - 1)
                        .is_some_and(vt100::Cell::is_wide) =>
                {
                    output.extend(&narrowed[usize::from(row)])
                }
                _ => output.extend(cells),
            }
        }
        output.extend(DEFAULT_COLOURS);
        output.extend(screen.input_mode_formatted());
        let (row, column) = screen.cursor_position();
        if !screen.hide_cursor() && row < rows && column < columns {
            output.extend(format!("\x1b[{};{}H{SHOW_CURSOR}", row + 1, column + 1).as_bytes());
        }
        output
    }
}

fn write_terminal(bytes: &[u8]) {
    let mut output = std::io::stdout().lock();
    let _ = output.write_all(bytes).and_then(|()| output.flush());
}

/// Reads the user's keys in chunks of at most one input request until the view ends. Waiting
/// for a free slot holds further keys in the terminal rather than dropping them.
fn read_keys(stop: &AtomicBool, keys: &tokio::sync::mpsc::Sender<Vec<u8>>) {
    let mut buffer = [0; ATTACH_INPUT_BYTES];
    while !stop.load(Ordering::Acquire) {
        let mut ready = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        match unsafe { libc::poll(&mut ready, 1, KEY_POLL_MILLISECONDS) } {
            0 => continue,
            count if count < 0 => {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return;
            }
            _ => {}
        }
        let count =
            unsafe { libc::read(libc::STDIN_FILENO, buffer.as_mut_ptr().cast(), buffer.len()) };
        if count < 0 {
            let error = std::io::Error::last_os_error();
            if matches!(
                error.kind(),
                std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
            ) {
                continue;
            }
            return;
        }
        if count == 0
            || keys
                .blocking_send(buffer[..count as usize].to_vec())
                .is_err()
        {
            return;
        }
    }
}

#[cfg(test)]
#[path = "attach_tests.rs"]
mod tests;
