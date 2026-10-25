//! The terminal side of an attached view: it shows a hosted Tick's terminal in the current
//! terminal and sends the user's keys to that same process until the user closes it. Other views
//! may show the same Tick at once, and its screen takes the size of whichever view typed last,
//! so this terminal can be smaller or larger than the screen it shows.

use std::{
    io::Write,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use serde::Deserialize;
use serde_json::{Value, json};
use tickhost_connect::{
    host_error::HostError,
    hosted::{
        self, ATTACH_INPUT_BYTES, ATTACH_REQUEST_FRAME_BYTES, BUSINESS_FRAME_BYTES,
        CAPABILITY_DATA, CONTROL_TIMEOUT, HostConnection, MAX_COLUMNS, MAX_ROWS, MIN_COLUMNS,
        MIN_ROWS, Role,
    },
};
use tickloop_harness::protocol::HostedViewGrant;

/// A lone Esc closes the view and leaves the Tick running.
const ESCAPE: u8 = 0x1b;
/// An Esc with nothing after it within this time was pressed alone; the bytes of an escape
/// sequence, such as an arrow key, follow it at once.
const ESCAPE_DELAY: std::time::Duration = std::time::Duration::from_millis(100);
/// Ctrl+B Esc sends Esc to the Tick and Ctrl+B Ctrl+B sends Ctrl+B.
const SEND_KEY_PREFIX: u8 = 0x02;
/// The byte after Esc that begins a control sequence, the form of arrow keys and of every key
/// the kitty keyboard protocol reports.
const CONTROL_SEQUENCE_START: u8 = b'[';
/// A control sequence still unfinished at this length is no key's, so it reaches the Tick as it
/// is.
const LONGEST_KEY_SEQUENCE: usize = 32;
/// Modifier bits of the kitty keyboard protocol: Ctrl, and the Caps Lock and Num Lock states a
/// terminal may report with any key.
const KITTY_CONTROL: u16 = 4;
const KITTY_LOCKS: u16 = 64 | 128;

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

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

/// How an attached view ended.
#[derive(Debug, PartialEq, Eq)]
pub enum AttachEnd {
    Detached,
    /// The Tick ended while it was shown.
    Gone,
    /// The host ended the view, as it does for one that fell behind.
    Lost(String),
}

/// Shows the Tick `grant` names in this process's terminal, which must already be in raw mode,
/// until the user closes the view or it ends.
pub async fn run_view(grant: &HostedViewGrant) -> Result<AttachEnd, HostError> {
    AttachedView::attach(grant).await?.run().await
}

/// A view attached to the Tick terminal that has not taken this terminal yet, so a refused attach
/// never disturbs the screen.
pub struct AttachedView {
    events: tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>,
    requests: tokio::net::unix::OwnedWriteHalf,
}

impl AttachedView {
    pub async fn attach(grant: &HostedViewGrant) -> Result<Self, HostError> {
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
        Ok(Self { events, requests })
    }

    /// Takes this terminal, which must already be in raw mode, until the user closes the view
    /// or it ends; the terminal is restored afterwards.
    pub async fn run(self) -> Result<AttachEnd, HostError> {
        let Self {
            mut events,
            mut requests,
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
            let mut filter = KeyFilter::default();
            let mut escape_deadline: Option<tokio::time::Instant> = None;
            loop {
                tokio::select! {
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
                                write_terminal(&screen.frame(size, data.as_bytes()));
                            }
                            Some("gone") => return Ok(AttachEnd::Gone),
                            _ => return Ok(AttachEnd::Lost(view_error(&event))),
                        }
                    }
                    keys = typed.recv() => {
                        let Some(keys) = keys else {
                            send(&mut requests, json!({"method":"detach"})).await?;
                            return Ok(AttachEnd::Detached);
                        };
                        if deliver(&mut requests, filter.feed(&keys)).await? {
                            return Ok(AttachEnd::Detached);
                        }
                        escape_deadline = filter
                            .holds_sequence()
                            .then(|| tokio::time::Instant::now() + ESCAPE_DELAY);
                    }
                    _ = tokio::time::sleep_until(escape_deadline.unwrap_or_else(tokio::time::Instant::now)), if escape_deadline.is_some() => {
                        escape_deadline = None;
                        if deliver(&mut requests, filter.flush()).await? {
                            return Ok(AttachEnd::Detached);
                        }
                    }
                    _ = resized.recv() => {
                        let terminal = terminal_size();
                        write_terminal(&screen.resize_terminal(terminal));
                        send(&mut requests, json!({"method":"resize","params":{"size":requested_size(terminal)}})).await?;
                    }
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

async fn send(
    requests: &mut tokio::net::unix::OwnedWriteHalf,
    request: Value,
) -> Result<(), HostError> {
    hosted::write_frame(requests, &request, ATTACH_REQUEST_FRAME_BYTES).await
}

/// Sends the Tick its share of the user's keys and, when they close the view, detaches it;
/// returns whether they closed it.
async fn deliver(
    requests: &mut tokio::net::unix::OwnedWriteHalf,
    keys: FilteredKeys,
) -> Result<bool, HostError> {
    for request in input_requests(&keys.forwarded) {
        send(requests, request).await?;
    }
    if keys.closes {
        send(requests, json!({"method":"detach"})).await?;
    }
    Ok(keys.closes)
}

/// The input requests that carry `forwarded` to the Tick, each within the host's input limit:
/// a held prefix or escape sequence can make one read's keys longer than a request may carry.
fn input_requests(forwarded: &[u8]) -> impl Iterator<Item = Value> + '_ {
    forwarded
        .chunks(ATTACH_INPUT_BYTES)
        .map(|chunk| json!({"method":"input","params":{"data":chunk}}))
}

fn view_error(event: &Value) -> String {
    event["error"]["message"]
        .as_str()
        .unwrap_or("the Tickhost ended this view")
        .to_owned()
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

/// Splits the user's keys into what reaches the Tick and the request to close the view. A lone
/// Esc closes it, while escape sequences and Alt-keys reach the Tick: a legacy Esc closes once
/// nothing follows within the escape delay, and an Esc in the kitty keyboard protocol's encoding,
/// which a terminal may still send when a program left the protocol on, closes at once. Ctrl+B
/// Esc sends one Esc, Ctrl+B Ctrl+B sends one Ctrl+B, Ctrl+B followed by an escape sequence sends
/// that sequence and Ctrl+B followed by any other byte sends both in order. Esc and Ctrl+B count
/// in either encoding and reach the Tick as their legacy bytes; every other sequence reaches it
/// as it came. A prefix or a sequence may span reads. Text inside a bracketed paste never closes
/// the view or counts as a prefix.
#[derive(Default)]
struct KeyFilter {
    after_prefix: bool,
    in_paste: bool,
    /// An escape sequence not ended yet: a lone Esc, or the start of a key's sequence, held until
    /// the next keys or the escape delay show which.
    sequence: Vec<u8>,
    /// The newest forwarded bytes, enough to recognise a paste marker split across reads.
    recent: Vec<u8>,
}

/// What a read of the user's keys does.
#[derive(Debug, Default, PartialEq, Eq)]
struct FilteredKeys {
    /// The bytes the Tick receives.
    forwarded: Vec<u8>,
    /// Whether the keys close the view; keys read after the closing Esc go with it.
    closes: bool,
}

/// How a byte read after the start of an escape sequence relates to it.
enum SequenceStep {
    Continues,
    Ends,
    /// The byte belongs to no sequence begun so far, which therefore ended before it.
    Breaks,
}

impl KeyFilter {
    fn feed(&mut self, input: &[u8]) -> FilteredKeys {
        let mut keys = FilteredKeys::default();
        for &byte in input {
            if !self.sequence.is_empty() {
                match sequence_step(&self.sequence, byte) {
                    SequenceStep::Continues => {
                        self.sequence.push(byte);
                        continue;
                    }
                    SequenceStep::Ends => {
                        self.sequence.push(byte);
                        let sequence = std::mem::take(&mut self.sequence);
                        match kitty_key(&sequence) {
                            Some(key) => self.key(key, &mut keys),
                            None => self.forward_sequence(&sequence, &mut keys.forwarded),
                        }
                        if keys.closes {
                            return keys;
                        }
                        continue;
                    }
                    SequenceStep::Breaks => {
                        let sequence = std::mem::take(&mut self.sequence);
                        self.forward_sequence(&sequence, &mut keys.forwarded);
                    }
                }
            }
            if byte == ESCAPE && !self.in_paste {
                self.sequence.push(byte);
            } else {
                self.key(byte, &mut keys);
            }
        }
        keys
    }

    /// Whether the keys read so far end inside an escape sequence, perhaps a lone Esc.
    fn holds_sequence(&self) -> bool {
        !self.sequence.is_empty()
    }

    /// Ends the held sequence once nothing followed it within the escape delay: a lone Esc was
    /// pressed alone, and any other unfinished sequence reaches the Tick as it is.
    fn flush(&mut self) -> FilteredKeys {
        let mut keys = FilteredKeys::default();
        let sequence = std::mem::take(&mut self.sequence);
        match sequence.as_slice() {
            [] => {}
            [ESCAPE] => self.key(ESCAPE, &mut keys),
            _ => self.forward_sequence(&sequence, &mut keys.forwarded),
        }
        keys
    }

    /// Acts on one key the view has a meaning for, or forwards it: a byte read as it is, or the
    /// legacy byte of an Esc or Ctrl+B in the kitty keyboard protocol's encoding.
    fn key(&mut self, byte: u8, keys: &mut FilteredKeys) {
        if std::mem::take(&mut self.after_prefix) {
            if !matches!(byte, SEND_KEY_PREFIX | ESCAPE) {
                self.forward(&mut keys.forwarded, SEND_KEY_PREFIX);
            }
            self.forward(&mut keys.forwarded, byte);
        } else if self.in_paste {
            self.forward(&mut keys.forwarded, byte);
        } else if byte == SEND_KEY_PREFIX {
            self.after_prefix = true;
        } else if byte == ESCAPE {
            keys.closes = true;
        } else {
            self.forward(&mut keys.forwarded, byte);
        }
    }

    /// Forwards an escape sequence that is no key of the view's as it came, in place of any
    /// prefix before it.
    fn forward_sequence(&mut self, sequence: &[u8], forwarded: &mut Vec<u8>) {
        self.after_prefix = false;
        for &byte in sequence {
            self.forward(forwarded, byte);
        }
    }

    fn forward(&mut self, forwarded: &mut Vec<u8>, byte: u8) {
        forwarded.push(byte);
        self.recent.push(byte);
        if self.recent.len() > PASTE_START.len() {
            self.recent.remove(0);
        }
        if self.recent.ends_with(PASTE_START) {
            self.in_paste = true;
        } else if self.recent.ends_with(PASTE_END) {
            self.in_paste = false;
        }
    }
}

fn sequence_step(sequence: &[u8], byte: u8) -> SequenceStep {
    if sequence.len() == 1 {
        return if byte == CONTROL_SEQUENCE_START {
            SequenceStep::Continues
        } else {
            SequenceStep::Breaks
        };
    }
    match byte {
        _ if sequence.len() >= LONGEST_KEY_SEQUENCE => SequenceStep::Breaks,
        0x20..=0x3f => SequenceStep::Continues,
        0x40..=0x7e => SequenceStep::Ends,
        _ => SequenceStep::Breaks,
    }
}

/// The legacy byte of Esc or Ctrl+B when `sequence` is how the kitty keyboard protocol reports
/// that key pressed or repeated: `CSI 27 u` or `CSI 98;5 u`, perhaps with lock states, an event
/// type or alternate keys.
fn kitty_key(sequence: &[u8]) -> Option<u8> {
    let parameters = sequence.strip_prefix(b"\x1b[")?.strip_suffix(b"u")?;
    let mut fields = std::str::from_utf8(parameters).ok()?.split(';');
    let code = fields.next()?.split(':').next()?;
    let (modifiers, event) = match fields.next() {
        Some(field) => field.split_once(':').unwrap_or((field, "1")),
        None => ("1", "1"),
    };
    let modifiers = modifiers.parse::<u16>().ok()?.checked_sub(1)? & !KITTY_LOCKS;
    if !matches!(event, "1" | "2") {
        return None;
    }
    match (code, modifiers) {
        ("27", 0) => Some(ESCAPE),
        ("98", KITTY_CONTROL) => Some(SEND_KEY_PREFIX),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size(rows: u16, columns: u16) -> ScreenSize {
        ScreenSize { rows, columns }
    }

    /// Frames a Tick screen as the host does: whole at first and after a resize, then only what
    /// changed.
    struct HostScreen {
        parser: vt100::Parser,
        shown: Option<vt100::Screen>,
    }

    impl HostScreen {
        fn new(rows: u16, columns: u16) -> Self {
            Self {
                parser: vt100::Parser::new(rows, columns, 0),
                shown: None,
            }
        }

        fn frame(&mut self, output: &[u8]) -> (ScreenSize, Vec<u8>) {
            self.parser.process(output);
            let current = self.parser.screen();
            let bytes = match &self.shown {
                Some(shown) if shown.size() == current.size() => current.state_diff(shown),
                _ => current.state_formatted(),
            };
            self.shown = Some(current.clone());
            let (rows, columns) = current.size();
            (size(rows, columns), bytes)
        }
    }

    /// A terminal of `terminal`'s size that showed something else before.
    fn terminal(terminal: ScreenSize) -> vt100::Parser {
        let mut parser = vt100::Parser::new(terminal.rows, terminal.columns, 0);
        parser
            .process(&b"stale".repeat(usize::from(terminal.rows) * usize::from(terminal.columns)));
        parser
    }

    fn rows(terminal: &vt100::Parser) -> Vec<String> {
        let (_, columns) = terminal.screen().size();
        terminal.screen().rows(0, columns).collect()
    }

    #[test]
    fn a_smaller_terminal_shows_the_top_left_of_the_screen_cut_at_its_edges() {
        let mut host = HostScreen::new(6, 20);
        let (tick, frame) = host
            .frame("first line is long\r\nabcdefghi腾 wide\r\nthird\r\nfourth\r\nfifth".as_bytes());
        let small = size(4, 10);
        let mut view = ViewScreen::new(small);
        let mut shown = terminal(small);
        shown.process(&view.frame(tick, &frame));
        assert_eq!(
            rows(&shown),
            ["first line", "abcdefghi", "third", "fourth"],
            "the top-left part, never scrolled"
        );
        assert_eq!(
            shown.screen().cell(1, 9).unwrap().contents(),
            "",
            "a wide character that would spill past the edge is left out"
        );
        assert!(
            shown.screen().hide_cursor(),
            "a cursor outside the part shown is hidden"
        );

        let (tick, change) = host.frame(b"\x1b[2;1HABC\x1b[1;3H");
        shown.process(&view.frame(tick, &change));
        assert_eq!(rows(&shown)[..2], ["first line", "ABCdefghi"]);
        assert!(!shown.screen().hide_cursor());
        assert_eq!(shown.screen().cursor_position(), (0, 2));
    }

    #[test]
    fn a_larger_terminal_shows_the_whole_screen_with_blank_space_beyond_it() {
        let mut host = HostScreen::new(3, 10);
        let (tick, frame) = host.frame(b"\x1b[1;31m0123456789\x1b[mabc");
        let large = size(6, 20);
        let mut view = ViewScreen::new(large);
        let mut shown = terminal(large);
        shown.process(&view.frame(tick, &frame));
        assert_eq!(
            rows(&shown),
            ["0123456789", "abc", "", "", "", ""],
            "a wrapped line stays as the Tick drew it and the rest is blank"
        );
        assert_eq!(
            shown.screen().cell(0, 0).unwrap().fgcolor(),
            vt100::Color::Idx(1)
        );
        assert_eq!(
            shown.screen().cell(1, 0).unwrap().fgcolor(),
            vt100::Color::Default
        );
        assert_eq!(shown.screen().cursor_position(), (1, 3));
        assert!(!shown.screen().hide_cursor());
    }

    #[test]
    fn a_terminal_of_the_screen_size_gets_the_frames_as_drawn_and_a_redraw_when_either_resizes() {
        let mut host = HostScreen::new(3, 10);
        let same = size(3, 10);
        let mut view = ViewScreen::new(same);
        let mut shown = terminal(same);
        let (tick, frame) = host.frame(b"hello");
        let drawn = view.frame(tick, &frame);
        assert_eq!(drawn, frame, "the host's frame as it is");
        shown.process(&drawn);

        let larger = size(5, 12);
        shown.screen_mut().set_size(larger.rows, larger.columns);
        shown.process(&view.resize_terminal(larger));
        let (tick, change) = host.frame(b" world");
        shown.process(&view.frame(tick, &change));
        assert_eq!(rows(&shown), ["hello worl", "d", "", "", ""]);

        shown.screen_mut().set_size(same.rows, same.columns);
        shown.process(&view.resize_terminal(same));
        let (tick, change) = host.frame(b"!");
        let drawn = view.frame(tick, &change);
        assert_eq!(
            drawn, change,
            "back at the same size, changes are drawn as they come"
        );
        shown.process(&drawn);
        assert_eq!(shown.screen().contents(), host.parser.screen().contents());

        host.parser.screen_mut().set_size(4, 12);
        let (tick, frame) = host.frame(b"");
        assert_eq!(tick, size(4, 12));
        shown.process(&view.frame(tick, &frame));
        assert_eq!(
            rows(&shown),
            ["hello worl", "d!", ""],
            "a resized screen is cut to fit"
        );
    }

    fn feed_all(filter: &mut KeyFilter, chunks: &[&[u8]]) -> Vec<u8> {
        chunks
            .iter()
            .flat_map(|chunk| filter.feed(chunk).forwarded)
            .collect()
    }

    fn forwarded(bytes: &[u8]) -> FilteredKeys {
        FilteredKeys {
            forwarded: bytes.to_vec(),
            closes: false,
        }
    }

    const CLOSED: FilteredKeys = FilteredKeys {
        forwarded: Vec::new(),
        closes: true,
    };

    #[test]
    fn escape_sequences_alt_keys_and_ordinary_keys_reach_the_tick_unchanged() {
        let mut filter = KeyFilter::default();
        let typed = b"\x1bjk\x1b[A\x1bOB\x1b[<0;3;4M\xe8\x85\xbe";
        assert_eq!(filter.feed(typed), forwarded(typed));
        assert_eq!(
            feed_all(&mut filter, &[b"\x1b", b"[B"]),
            b"\x1b[B".to_vec(),
            "a sequence split across reads"
        );
        assert!(!filter.holds_sequence());
        let kitty = b"\x1b[27;1:3u\x1b[27;5u\x1b[13;5u\x1b[98;6u\x1b[97;3u";
        assert_eq!(
            filter.feed(kitty),
            forwarded(kitty),
            "an Esc release, Ctrl+Esc and other keys in the kitty encoding"
        );
    }

    #[test]
    fn a_lone_escape_closes_the_view_once_nothing_follows_it() {
        let mut filter = KeyFilter::default();
        assert_eq!(filter.feed(b"ab\x1b"), forwarded(b"ab"));
        assert!(filter.holds_sequence());
        assert_eq!(filter.flush(), CLOSED, "a lone Esc closes the view");
        assert!(!filter.holds_sequence());
    }

    #[test]
    fn an_escape_in_the_kitty_encoding_closes_the_view_at_once_even_across_reads() {
        for escape in [
            b"\x1b[27u".as_slice(),
            b"\x1b[27;1u",
            b"\x1b[27;1:1u",
            b"\x1b[27;1:2u",
            b"\x1b[27;129u",
        ] {
            let mut filter = KeyFilter::default();
            assert_eq!(
                filter.feed(&[b"ab", escape, b"cd"].concat()),
                FilteredKeys {
                    forwarded: b"ab".to_vec(),
                    closes: true,
                },
                "{escape:?}"
            );
        }
        let mut filter = KeyFilter::default();
        assert_eq!(filter.feed(b"\x1b[2"), forwarded(b""));
        assert!(filter.holds_sequence());
        assert_eq!(filter.feed(b"7u"), CLOSED);
    }

    #[test]
    fn an_unfinished_sequence_reaches_the_tick_once_nothing_follows_it() {
        let mut filter = KeyFilter::default();
        assert_eq!(filter.feed(b"a\x1b["), forwarded(b"a"));
        assert_eq!(filter.flush(), forwarded(b"\x1b["), "Alt+[");
        let long = [b"\x1b[".as_slice(), &[b'1'; LONGEST_KEY_SEQUENCE]].concat();
        assert_eq!(filter.feed(&long), forwarded(&long));
        assert!(!filter.holds_sequence(), "no key's sequence is that long");
    }

    #[test]
    fn the_prefix_sends_escape_or_itself_once_in_either_encoding_and_otherwise_both_bytes() {
        for (prefix, escape) in [
            (b"\x02".as_slice(), b"\x1b".as_slice()),
            (b"\x1b[98;5u", b"\x1b[27u"),
        ] {
            let mut filter = KeyFilter::default();
            let held = filter.feed(&[prefix, escape].concat());
            let flushed = filter.flush();
            assert!(!held.closes && !flushed.closes, "Ctrl+B Esc never closes");
            assert_eq!([held.forwarded, flushed.forwarded].concat(), [ESCAPE]);
            assert_eq!(
                feed_all(&mut filter, &[prefix, prefix]),
                vec![SEND_KEY_PREFIX]
            );
            assert_eq!(
                feed_all(&mut filter, &[b"ab", prefix, b"qz"]),
                b"ab\x02qz".to_vec()
            );
        }
        let mut filter = KeyFilter::default();
        assert_eq!(
            filter.feed(b"\x02\x1b[A"),
            forwarded(b"\x1b[A"),
            "a sequence after the prefix is sent alone"
        );
    }

    #[test]
    fn a_held_prefix_and_a_full_read_go_out_as_requests_the_host_accepts() {
        let mut filter = KeyFilter::default();
        assert!(filter.feed(&[SEND_KEY_PREFIX]).forwarded.is_empty());
        let forwarded = filter.feed(&[b'x'; ATTACH_INPUT_BYTES]).forwarded;
        assert_eq!(forwarded.len(), ATTACH_INPUT_BYTES + 1);
        let requests: Vec<Value> = input_requests(&forwarded).collect();
        assert_eq!(requests.len(), 2);
        let sent: Vec<u8> = requests
            .iter()
            .flat_map(|request| {
                let data = request["params"]["data"].as_array().unwrap();
                assert!(data.len() <= ATTACH_INPUT_BYTES, "within one input request");
                data.iter().map(|byte| byte.as_u64().unwrap() as u8)
            })
            .collect();
        assert_eq!(sent, forwarded, "every byte, once and in order");
    }

    #[test]
    fn pasted_text_never_closes_the_view_even_when_its_markers_span_reads() {
        let mut filter = KeyFilter::default();
        assert_eq!(
            feed_all(
                &mut filter,
                &[b"\x1b[20", b"0~a\x02\x1b", b"b\x1b[27u\x1b[201~"]
            ),
            b"\x1b[200~a\x02\x1bb\x1b[27u\x1b[201~".to_vec()
        );
        assert!(!filter.holds_sequence(), "an Esc inside the paste is text");
        assert!(filter.feed(b"\x1b").forwarded.is_empty());
        assert_eq!(filter.flush(), CLOSED, "Esc closes again after the paste");
    }

    /// A program that crashed on the alternate screen can leave keyboard flags pushed there; the
    /// view still reads legacy keys and leaves them as they were. Closing it turns off the input
    /// modes the Tick's frames turned on, as `python3 -q` does with bracketed paste and
    /// application cursor and keypad keys, before the Console takes the terminal back.
    #[test]
    fn the_view_reads_legacy_keys_on_its_screen_and_turns_off_the_ticks_input_modes() {
        use tickloop_harness::test_support::kitty_keyboard::{
            DISAMBIGUATE_ESCAPE_CODES, KittyKeyboard,
        };
        let mut output = b"\x1b[?1049h\x1b[>1u\x1b[?1049l".to_vec();
        output.extend(TAKE_SCREEN.as_bytes());
        assert_eq!(KittyKeyboard::after(&output).flags(), 0, "legacy keys");

        let mut host = HostScreen::new(24, 80);
        let (tick, frame) = host.frame(b"\x1b[?2004h\x1b[?1h\x1b=\x1b[?1000h\x1b[?1006h>>> ");
        output.extend(ViewScreen::new(size(24, 80)).frame(tick, &frame));
        let mut terminal = vt100::Parser::default();
        terminal.process(&output);
        let shown = terminal.screen();
        assert!(
            shown.bracketed_paste() && shown.application_cursor() && shown.application_keypad()
        );

        output.extend(RESTORE_TERMINAL.as_bytes());
        let keyboard = KittyKeyboard::after(&output);
        assert_eq!(keyboard.alternate(), [DISAMBIGUATE_ESCAPE_CODES]);
        assert!(keyboard.main().is_empty(), "{keyboard:?}");
        let mut terminal = vt100::Parser::default();
        terminal.process(&output);
        let restored = terminal.screen();
        assert!(!restored.alternate_screen());
        assert!(
            !restored.bracketed_paste()
                && !restored.application_cursor()
                && !restored.application_keypad()
        );
        assert_eq!(
            restored.mouse_protocol_mode(),
            vt100::MouseProtocolMode::None
        );
        assert_eq!(
            restored.mouse_protocol_encoding(),
            vt100::MouseProtocolEncoding::Default
        );
    }
}
