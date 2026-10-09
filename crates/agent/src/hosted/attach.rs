//! The terminal side of an attached view: it shows a hosted Tick's terminal in the current
//! terminal and sends the user's keys to that same process until they detach.

use std::{
    io::Write,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use serde_json::{Value, json};
use tick_connect::{
    host_error::HostError,
    hosted::{
        self, ATTACH_INPUT_BYTES, ATTACH_REQUEST_FRAME_BYTES, BUSINESS_FRAME_BYTES,
        CAPABILITY_DATA, CONTROL_TIMEOUT, HostConnection, Role,
    },
};
use tick_host::pty::{MAX_COLUMNS, MAX_ROWS, MIN_COLUMNS, MIN_ROWS};

use super::controller::HostedViewGrant;

/// Ctrl+B starts the detach prefix.
pub const DETACH_PREFIX: u8 = 0x02;
/// After the prefix, `q` detaches the view and leaves the Tick running.
pub const DETACH_KEY: u8 = b'q';

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
/// until the user detaches or the view ends. The terminal is restored whenever the view had
/// taken it.
pub async fn run_view(grant: &HostedViewGrant) -> Result<AttachEnd, HostError> {
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
            "the Tick host was replaced since this view was granted",
        ));
    }
    connection
        .request("attach", json!({"size":terminal_size()}), CONTROL_TIMEOUT)
        .await?;
    let (mut events, mut requests) = connection.into_parts();
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
        let mut filter = KeyFilter::default();
        loop {
            tokio::select! {
                event = hosted::read_frame(&mut events, BUSINESS_FRAME_BYTES) => {
                    let Some(event) = event? else {
                        return Ok(AttachEnd::Lost("the Tick host closed this view".into()));
                    };
                    match event["event"].as_str() {
                        Some("frame") => write_terminal(event["data"].as_str().unwrap_or_default().as_bytes()),
                        Some("gone") => return Ok(AttachEnd::Gone),
                        _ => return Ok(AttachEnd::Lost(view_error(&event))),
                    }
                }
                keys = typed.recv() => {
                    let (forwarded, detach) = match keys {
                        Some(keys) => filter.feed(&keys),
                        None => (Vec::new(), true),
                    };
                    for request in input_requests(&forwarded) {
                        send(&mut requests, request).await?;
                    }
                    if detach {
                        send(&mut requests, json!({"method":"detach"})).await?;
                        return Ok(AttachEnd::Detached);
                    }
                }
                _ = resized.recv() => {
                    send(&mut requests, json!({"method":"resize","params":{"size":terminal_size()}})).await?;
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
        .unwrap_or("the Tick host ended this view")
        .to_owned()
}

/// The current terminal's size within the bounds a Tick terminal accepts.
fn terminal_size() -> Value {
    let (columns, rows) = crossterm::terminal::size().unwrap_or(FALLBACK_TERMINAL_SIZE);
    json!({
        "rows": rows.clamp(MIN_ROWS, MAX_ROWS),
        "columns": columns.clamp(MIN_COLUMNS, MAX_COLUMNS),
    })
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

/// Splits the user's keys into what reaches the Tick and the detach request. Ctrl+B q detaches,
/// Ctrl+B Ctrl+B sends one Ctrl+B and Ctrl+B followed by any other byte sends both in order; a
/// prefix may span reads. Everything else, Esc included, reaches the Tick unchanged, and text
/// inside a bracketed paste never counts as a prefix.
#[derive(Default)]
pub struct KeyFilter {
    after_prefix: bool,
    in_paste: bool,
    /// The newest forwarded bytes, enough to recognise a paste marker split across reads.
    recent: Vec<u8>,
}

impl KeyFilter {
    /// The bytes of `input` the Tick receives, and whether the user asked to detach; nothing
    /// after the detach request is sent.
    pub fn feed(&mut self, input: &[u8]) -> (Vec<u8>, bool) {
        let mut forwarded = Vec::with_capacity(input.len());
        for &byte in input {
            if self.after_prefix {
                self.after_prefix = false;
                match byte {
                    DETACH_KEY => return (forwarded, true),
                    DETACH_PREFIX => self.forward(&mut forwarded, DETACH_PREFIX),
                    other => {
                        self.forward(&mut forwarded, DETACH_PREFIX);
                        self.forward(&mut forwarded, other);
                    }
                }
            } else if byte == DETACH_PREFIX && !self.in_paste {
                self.after_prefix = true;
            } else {
                self.forward(&mut forwarded, byte);
            }
        }
        (forwarded, false)
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

    fn feed_all(filter: &mut KeyFilter, chunks: &[&[u8]]) -> (Vec<u8>, bool) {
        let mut forwarded = Vec::new();
        for chunk in chunks {
            let (bytes, detach) = filter.feed(chunk);
            forwarded.extend(bytes);
            if detach {
                return (forwarded, true);
            }
        }
        (forwarded, false)
    }

    #[test]
    fn escape_and_ordinary_keys_reach_the_tick_unchanged() {
        let mut filter = KeyFilter::default();
        assert_eq!(
            filter.feed(b"\x1bjk\x1b[A\xe8\x85\xbe"),
            (b"\x1bjk\x1b[A\xe8\x85\xbe".to_vec(), false)
        );
    }

    #[test]
    fn a_held_prefix_and_a_full_read_go_out_as_requests_the_host_accepts() {
        let mut filter = KeyFilter::default();
        assert_eq!(filter.feed(&[DETACH_PREFIX]), (Vec::new(), false));
        let (forwarded, detach) = filter.feed(&[b'x'; ATTACH_INPUT_BYTES]);
        assert!(!detach);
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
    fn the_prefix_then_q_detaches_even_across_reads() {
        let mut filter = KeyFilter::default();
        assert_eq!(
            feed_all(&mut filter, &[b"ab\x02q", b"zz"]),
            (b"ab".to_vec(), true)
        );
        let mut filter = KeyFilter::default();
        assert_eq!(
            feed_all(&mut filter, &[b"ab\x02", b"qzz"]),
            (b"ab".to_vec(), true)
        );
    }

    #[test]
    fn a_doubled_prefix_sends_one_and_another_key_follows_it_in_order() {
        let mut filter = KeyFilter::default();
        assert_eq!(
            feed_all(&mut filter, &[b"\x02\x02"]),
            (vec![DETACH_PREFIX], false)
        );
        let mut filter = KeyFilter::default();
        assert_eq!(
            feed_all(&mut filter, &[b"\x02", b"\x02x"]),
            (b"\x02x".to_vec(), false)
        );
        let mut filter = KeyFilter::default();
        assert_eq!(
            feed_all(&mut filter, &[b"\x02x\x02", b"\x1b"]),
            (b"\x02x\x02\x1b".to_vec(), false)
        );
    }

    #[test]
    fn pasted_text_never_detaches_even_when_its_markers_span_reads() {
        let mut filter = KeyFilter::default();
        assert_eq!(
            feed_all(&mut filter, &[b"\x1b[20", b"0~a\x02q", b"b\x1b[201~"]),
            (b"\x1b[200~a\x02qb\x1b[201~".to_vec(), false)
        );
        assert_eq!(
            filter.feed(b"\x02q"),
            (Vec::new(), true),
            "the prefix works again after the paste"
        );
    }
}
