//! A full-screen view: shows a hosted Tick's terminal or a Shell's screen on the whole terminal
//! until the user hides it or the Tick ends. A Tick gets the user's keys unchanged; a Shell's
//! screen is read-only. Other views may show the same Tick at once, and its terminal takes the
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

use super::monitor_cards::{CardInput, CardKeys};

/// A second lone Esc within this time of the one the Tick got hides the view.
pub(super) const FULL_SCREEN_ESCAPE_WINDOW: Duration = Duration::from_secs(1);
/// Shown on the bottom row while a second Esc would hide the view.
const HIDE_HINT: &str = "Press esc again to hide";
/// The keys of a Shell's screen, on the bottom row the host leaves for them.
const SHELL_HINT: &str = "↑↓ scroll · G latest · esc hide";
const STOPPABLE_SHELL_HINT: &str = "ctrl+c stop · ↑↓ scroll · G latest · esc hide";
const INTERRUPT: u8 = 0x03;

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

/// What the user's keys do in a full-screen view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ViewInput {
    /// A Tick: keys reach it, and a second Esc within [`FULL_SCREEN_ESCAPE_WINDOW`] hides.
    Interactive,
    /// A Shell's screen: a lone Esc hides at once and its keys only scroll this view's place in
    /// the output; with `stoppable`, Ctrl+C stops the task.
    ReadOnly { stoppable: bool },
}

/// How a full-screen view ended.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum AttachEnd {
    /// The user hid it; the Tick keeps running.
    Hidden,
    /// The Tick ended while it was shown.
    Gone,
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
                json!({"size":requested_size(terminal_size())}),
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
            let mut screen = ViewScreen::new(terminal_size());
            let mut filter = FullScreenKeys::new(input);
            let mut bubbling = false;
            let mut hint_until: Option<Instant> = None;
            loop {
                let keys_due = filter.deadline().map(tokio::time::Instant::from_std);
                let hint_due = hint_until.map(tokio::time::Instant::from_std);
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
                                output.extend(overlay(input, hint_until.is_some(), screen.terminal));
                                write_terminal(&output);
                            }
                            // A terminal Tick's program exited while shown: the view ends,
                            // and the list says how.
                            Some("gone" | "exited") => return Ok(AttachEnd::Gone),
                            Some("bubbling") => bubbling = event["enabled"].as_bool().unwrap_or(false),
                            Some("bubble") => {
                                let key = event["key"].as_str().and_then(BubbledKey::from_name);
                                if input == ViewInput::Interactive && bubbling && key == Some(BubbledKey::Esc) {
                                    send(&mut requests, json!({"method":"detach"})).await?;
                                    return Ok(AttachEnd::Hidden);
                                }
                            }
                            Some("focus_reports") => {
                                if input == ViewInput::Interactive && event["enabled"].as_bool() == Some(true) {
                                    send(&mut requests, json!({"method":"focus","params":{"focused":true}})).await?;
                                }
                            }
                            _ => return Ok(AttachEnd::Lost(view_error(&event))),
                        }
                        Vec::new()
                    }
                    keys = typed.recv() => match keys {
                        Some(keys) => filter.feed(&keys, Instant::now(), screen.tick.screen()),
                        None => vec![KeyAction::Hide],
                    },
                    () = sleep_until(keys_due), if keys_due.is_some() => {
                        filter.expire(Instant::now(), screen.tick.screen())
                    }
                    () = sleep_until(hint_due), if hint_due.is_some() => Vec::new(),
                    _ = resized.recv() => {
                        let terminal = terminal_size();
                        let mut output = screen.resize_terminal(terminal);
                        output.extend(overlay(input, hint_until.is_some(), terminal));
                        write_terminal(&output);
                        send(&mut requests, json!({"method":"resize","params":{"size":requested_size(terminal)}})).await?;
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
                        KeyAction::Hide => {
                            send(&mut requests, json!({"method":"detach"})).await?;
                            return Ok(AttachEnd::Hidden);
                        }
                    }
                }
                let wanted = filter.hint_until(Instant::now());
                match (hint_until.is_some(), wanted.is_some()) {
                    (false, true) => write_terminal(&hint_overlay(screen.terminal, HIDE_HINT, INVERSE)),
                    (true, false) => write_terminal(&screen.draw()),
                    _ => {}
                }
                hint_until = wanted;
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
    /// Hide the view; nothing read after it reaches the Tick.
    Hide,
}

/// The user's keys in a full-screen view, split as a focused card splits them but with a longer
/// Esc pair: a lone Esc reaches the Tick at once and a second within
/// [`FULL_SCREEN_ESCAPE_WINDOW`] hides the view. On a read-only screen a lone Esc hides, Ctrl+C
/// interrupts a stoppable task and the other keys go to the host, which only scrolls with them.
/// The terminal's focus reports never reach the Tick, and a click does only when the Tick asked
/// for mouse reports, at its place on the Tick's screen.
struct FullScreenKeys {
    keys: CardKeys,
    input: ViewInput,
}

impl FullScreenKeys {
    fn new(input: ViewInput) -> Self {
        Self {
            keys: CardKeys::with_escape_window(FULL_SCREEN_ESCAPE_WINDOW),
            input,
        }
    }

    fn is_read_only(&self) -> bool {
        matches!(self.input, ViewInput::ReadOnly { .. })
    }

    fn feed(&mut self, input: &[u8], now: Instant, tick: &vt100::Screen) -> Vec<KeyAction> {
        let read_only = self.is_read_only();
        let items = self
            .keys
            .feed(input, now, tick.bracketed_paste(), read_only);
        self.actions(items, tick)
    }

    fn expire(&mut self, now: Instant, tick: &vt100::Screen) -> Vec<KeyAction> {
        let read_only = self.is_read_only();
        let items = self.keys.expire(now, read_only);
        self.actions(items, tick)
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

    fn actions(&self, items: Vec<CardInput>, tick: &vt100::Screen) -> Vec<KeyAction> {
        let mut actions = Vec::new();
        for item in items {
            match item {
                CardInput::Leave => {
                    actions.push(KeyAction::Hide);
                    break;
                }
                CardInput::Bytes(bytes) => match self.input {
                    ViewInput::Interactive => actions.push(KeyAction::Input(bytes)),
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
                _ if self.is_read_only() => {}
                CardInput::Mouse(mouse) => {
                    let (rows, columns) = tick.size();
                    let (column, row) =
                        (mouse.column.saturating_sub(1), mouse.row.saturating_sub(1));
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

/// What the view draws over the screen on its bottom row: the Esc hint while a second Esc would
/// hide it, and a Shell's keys always.
fn overlay(input: ViewInput, hides_on_escape: bool, terminal: ScreenSize) -> Vec<u8> {
    match input {
        _ if hides_on_escape => hint_overlay(terminal, HIDE_HINT, INVERSE),
        ViewInput::ReadOnly { stoppable } => hint_overlay(
            terminal,
            if stoppable {
                STOPPABLE_SHELL_HINT
            } else {
                SHELL_HINT
            },
            DIM,
        ),
        ViewInput::Interactive => Vec::new(),
    }
}

/// `hint` in `style` on the bottom row of `terminal`, drawn over the screen without moving its
/// cursor or changing its colours.
fn hint_overlay(terminal: ScreenSize, hint: &str, style: &str) -> Vec<u8> {
    let hint: String = hint.chars().take(usize::from(terminal.columns)).collect();
    format!(
        "{SAVE_CURSOR}\x1b[{};1H\x1b[m\x1b[2K{style}{hint}\x1b[m{RESTORE_CURSOR}",
        terminal.rows
    )
    .into_bytes()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
struct ScreenSize {
    rows: u16,
    columns: u16,
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

/// What this terminal shows of the Tick's screen. A screen of this terminal's size is drawn as
/// the host frames it; one of another size is redrawn whole from its top-left corner, cut where
/// this terminal ends and blank where this terminal reaches beyond it.
struct ViewScreen {
    tick: vt100::Parser,
    terminal: ScreenSize,
}

impl ViewScreen {
    fn new(terminal: ScreenSize) -> Self {
        Self {
            tick: vt100::Parser::default(),
            terminal,
        }
    }

    /// The output that shows a frame the host drew for a Tick screen of `size`.
    fn frame(&mut self, size: ScreenSize, data: &[u8]) -> Vec<u8> {
        if self.tick_size() != size {
            self.tick.screen_mut().set_size(size.rows, size.columns);
        }
        self.tick.process(data);
        if size == self.terminal {
            data.to_vec()
        } else {
            self.draw()
        }
    }

    /// The output that shows the Tick's screen once this terminal is `terminal`.
    fn resize_terminal(&mut self, terminal: ScreenSize) -> Vec<u8> {
        self.terminal = terminal;
        self.draw()
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
        if tick == self.terminal {
            return screen.state_formatted();
        }
        let rows = tick.rows.min(self.terminal.rows);
        let columns = tick.columns.min(self.terminal.columns);
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
