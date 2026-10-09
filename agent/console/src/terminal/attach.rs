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
use tickloop_agent_protocol::HostedViewGrant;

/// A lone Esc closes the view and leaves the Tick running.
const ESCAPE: u8 = 0x1b;
/// An Esc with nothing after it within this time was pressed alone; the bytes of an escape
/// sequence, such as an arrow key, follow it at once.
const ESCAPE_DELAY: std::time::Duration = std::time::Duration::from_millis(100);
/// Ctrl+B Esc sends Esc to the Tick and Ctrl+B Ctrl+B sends Ctrl+B.
const SEND_KEY_PREFIX: u8 = 0x02;

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

/// Columns and rows assumed when the terminal cannot report its size.
const FALLBACK_TERMINAL_SIZE: (u16, u16) = (80, 24);
/// The environment variable through which a popup's attach client receives its view grant.
pub const ATTACH_ENVIRONMENT: &str = "TICKLOOP_MONITOR_ATTACH";
/// At most this many key reads wait for the Tick; a busy Tick slows typing instead of losing it.
const QUEUED_KEY_READS: usize = 16;
/// How often the key reader looks whether its view has ended.
const KEY_POLL_MILLISECONDS: i32 = 100;
/// The view draws on its own screen and leaves the user's scrollback untouched.
const TAKE_SCREEN: &str = "\x1b[?1049h\x1b[H\x1b[2J";
/// Undoes every mode the Tick's frames may have turned on: colours, a hidden cursor, mouse and
/// paste reporting, application cursor and keypad keys, and the view's own screen.
const RESTORE_TERMINAL: &str = "\x1b[0m\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?2004l\x1b[?1l\x1b>\x1b[?1049l";
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
                        for request in input_requests(&filter.feed(&keys)) {
                            send(&mut requests, request).await?;
                        }
                        escape_deadline = filter
                            .holds_escape()
                            .then(|| tokio::time::Instant::now() + ESCAPE_DELAY);
                    }
                    _ = tokio::time::sleep_until(escape_deadline.unwrap_or_else(tokio::time::Instant::now)), if escape_deadline.is_some() => {
                        escape_deadline = None;
                        if filter.take_lone_escape() {
                            send(&mut requests, json!({"method":"detach"})).await?;
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

/// The input requests that carry `forwarded` to the Tick, each within the host's input limit:
/// a held prefix byte can make one read's keys one byte longer than a request may carry.
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
/// Esc closes it once nothing follows within the escape delay, while escape sequences and
/// Alt-keys reach the Tick. Ctrl+B Esc sends one Esc, Ctrl+B Ctrl+B sends one Ctrl+B and Ctrl+B
/// followed by any other byte sends both in order; a prefix may span reads. Text inside a
/// bracketed paste never closes the view or counts as a prefix.
#[derive(Default)]
struct KeyFilter {
    after_prefix: bool,
    in_paste: bool,
    /// An Esc that ended a read: the start of a sequence if more keys follow, else a lone Esc.
    held_escape: bool,
    /// The newest forwarded bytes, enough to recognise a paste marker split across reads.
    recent: Vec<u8>,
}

impl KeyFilter {
    /// The bytes of `input` the Tick receives.
    fn feed(&mut self, input: &[u8]) -> Vec<u8> {
        let mut forwarded = Vec::with_capacity(input.len() + 1);
        if !input.is_empty() && std::mem::take(&mut self.held_escape) {
            self.forward(&mut forwarded, ESCAPE);
        }
        for (index, &byte) in input.iter().enumerate() {
            if self.after_prefix {
                self.after_prefix = false;
                match byte {
                    SEND_KEY_PREFIX | ESCAPE => self.forward(&mut forwarded, byte),
                    other => {
                        self.forward(&mut forwarded, SEND_KEY_PREFIX);
                        self.forward(&mut forwarded, other);
                    }
                }
            } else if byte == SEND_KEY_PREFIX && !self.in_paste {
                self.after_prefix = true;
            } else if byte == ESCAPE && !self.in_paste && index + 1 == input.len() {
                self.held_escape = true;
            } else {
                self.forward(&mut forwarded, byte);
            }
        }
        forwarded
    }

    /// Whether a read ended with an Esc that may still begin a sequence.
    fn holds_escape(&self) -> bool {
        self.held_escape
    }

    /// Takes the held Esc once nothing followed it in time: it was pressed alone.
    fn take_lone_escape(&mut self) -> bool {
        std::mem::take(&mut self.held_escape)
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
        chunks.iter().flat_map(|chunk| filter.feed(chunk)).collect()
    }

    #[test]
    fn escape_sequences_alt_keys_and_ordinary_keys_reach_the_tick_unchanged() {
        let mut filter = KeyFilter::default();
        assert_eq!(
            filter.feed(b"\x1bjk\x1b[A\xe8\x85\xbe"),
            b"\x1bjk\x1b[A\xe8\x85\xbe".to_vec()
        );
        assert_eq!(
            feed_all(&mut filter, &[b"\x1b", b"[B"]),
            b"\x1b[B".to_vec(),
            "a sequence split across reads"
        );
        assert!(!filter.take_lone_escape());
    }

    #[test]
    fn a_lone_escape_closes_the_view_once_nothing_follows_it() {
        let mut filter = KeyFilter::default();
        assert_eq!(filter.feed(b"ab\x1b"), b"ab".to_vec());
        assert!(filter.holds_escape());
        assert!(filter.take_lone_escape(), "a lone Esc closes the view");
        assert!(!filter.holds_escape());
    }

    #[test]
    fn the_prefix_sends_escape_or_itself_once_and_otherwise_both_bytes_even_across_reads() {
        let mut filter = KeyFilter::default();
        assert_eq!(filter.feed(b"\x02\x1b"), vec![ESCAPE]);
        assert!(!filter.take_lone_escape(), "Ctrl+B Esc never closes");
        assert_eq!(
            feed_all(&mut filter, &[b"\x02", b"\x02"]),
            vec![SEND_KEY_PREFIX]
        );
        assert_eq!(
            feed_all(&mut filter, &[b"ab\x02", b"qz"]),
            b"ab\x02qz".to_vec()
        );
    }

    #[test]
    fn a_held_prefix_and_a_full_read_go_out_as_requests_the_host_accepts() {
        let mut filter = KeyFilter::default();
        assert!(filter.feed(&[SEND_KEY_PREFIX]).is_empty());
        let forwarded = filter.feed(&[b'x'; ATTACH_INPUT_BYTES]);
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
            feed_all(&mut filter, &[b"\x1b[20", b"0~a\x02\x1b", b"b\x1b[201~"]),
            b"\x1b[200~a\x02\x1bb\x1b[201~".to_vec()
        );
        assert!(!filter.holds_escape(), "an Esc inside the paste is text");
        assert!(filter.feed(b"\x1b").is_empty());
        assert!(
            filter.take_lone_escape(),
            "Esc closes again after the paste"
        );
    }
}
