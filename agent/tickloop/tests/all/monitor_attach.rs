//! The attach client a Monitor popup runs: a real PTY in front of `tickloop monitor attach`,
//! attached through a one-time view grant to a hosted Tick that shows every byte it receives.

use crate::support;

use std::{
    io::{Read, Write},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tickloop_agent_protocol::HostedViewGrant;
use tickloop_agent_runtime::hosted::controller::HostController;
use uuid::Uuid;

const TIMEOUT: Duration = Duration::from_secs(15);

/// An extension Tick whose terminal is raw and shows its size and the last bytes it received; it
/// logs every byte it receives to `received`.
const KEYS_TICK: &str = include_str!("../support/keys_tick.py");

struct Host {
    directory: tempfile::TempDir,
    controller: HostController,
    owner: tickhost::host::TickOwner,
}

impl Host {
    async fn start() -> Self {
        let directory = tempfile::Builder::new()
            .prefix("tma")
            .tempdir_in(tickloop_agent_test_support::SHORT_TEMP_ROOT)
            .unwrap();
        let controller = HostController::new(
            directory.path(),
            tickhost::paths::HostPaths {
                runtime: directory.path().join("h"),
            },
            Uuid::new_v4(),
            support::fixture_cli::tickhost(),
            support::hosted::IDLE_TIMEOUT,
        );
        Self {
            directory,
            controller,
            owner: tickhost::host::TickOwner {
                root: "fixture-root".into(),
                chat: Uuid::new_v4(),
            },
        }
    }

    async fn open_keys(&self) -> tickhost::host::OpenedTick {
        let script = self.directory.path().join("keys.py");
        std::fs::write(&script, KEYS_TICK).unwrap();
        self.controller
            .open(tickhost::host::OpenRequest {
                operation: Uuid::new_v4(),
                owner: self.owner.clone(),
                owner_revision: 0,
                adapter: tickhost::host::TickAdapter::Protocol,
                command: vec![
                    "/usr/bin/python3".into(),
                    "-u".into(),
                    script.to_string_lossy().into_owned(),
                ],
                cwd: self.directory.path().to_path_buf(),
                environment: vec![("PATH".into(), "/usr/bin:/bin".into())],
                title: None,
            })
            .await
            .unwrap()
    }

    async fn grant(&self, opened: &tickhost::host::OpenedTick) -> HostedViewGrant {
        self.controller
            .allow_view(&self.owner, &opened.instance)
            .await
            .unwrap()
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        support::stop_tickhost(
            &self.directory.path().join("h"),
            &self.directory.path().join("tickhost.key"),
        );
    }
}

/// `tickloop monitor attach` in its own PTY, as the popup runs it.
struct View {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    master: Box<dyn portable_pty::MasterPty + Send>,
    output: Arc<Mutex<Vec<u8>>>,
}

impl View {
    fn start(grant: &HostedViewGrant, home: &std::path::Path) -> Self {
        Self::start_sized(grant, home, 24, 80)
    }

    fn start_sized(grant: &HostedViewGrant, home: &std::path::Path, rows: u16, cols: u16) -> Self {
        let pty = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_tickloop"));
        command.args(["monitor", "attach"]);
        command.env_clear();
        command.env("PATH", "/usr/bin:/bin");
        command.env("HOME", home);
        command.env("TERM", "xterm-256color");
        command.env(
            tickloop_agent_protocol::ATTACH_ENVIRONMENT,
            serde_json::to_string(grant).unwrap(),
        );
        let child = pty.slave.spawn_command(command).unwrap();
        drop(pty.slave);
        let mut reader = pty.master.try_clone_reader().unwrap();
        let output = Arc::new(Mutex::new(Vec::new()));
        let captured = output.clone();
        std::thread::spawn(move || {
            let mut chunk = [0; 8192];
            while let Ok(count) = reader.read(&mut chunk) {
                if count == 0 {
                    break;
                }
                captured.lock().unwrap().extend_from_slice(&chunk[..count]);
            }
        });
        Self {
            writer: pty.master.take_writer().unwrap(),
            child,
            master: pty.master,
            output,
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).unwrap();
        self.writer.flush().unwrap();
    }

    fn screen(&self, rows: u16, columns: u16) -> String {
        self.terminal(rows, columns).screen().contents()
    }

    fn rows(&self, rows: u16, columns: u16) -> Vec<String> {
        self.terminal(rows, columns)
            .screen()
            .rows(0, columns)
            .collect()
    }

    /// This view's terminal, of `rows` and `columns`, after everything the client wrote.
    fn terminal(&self, rows: u16, columns: u16) -> vt100::Parser {
        let mut parser = vt100::Parser::new(rows, columns, 0);
        parser.process(&self.output.lock().unwrap());
        parser
    }

    fn shows(&self, rows: u16, columns: u16, text: &str) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let screen = self.screen(rows, columns);
            if screen.contains(text) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{text:?} never appeared:\n{screen}"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn exit_code(&mut self) -> u32 {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.exit_code();
            }
            assert!(Instant::now() < deadline, "the attach client never exited");
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn output_text(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
    }
}

/// Every key the user types reaches the same Tick process, wide characters and mouse reports
/// included; Ctrl+B Esc sends Esc, Ctrl+B Ctrl+B sends one Ctrl+B, Ctrl+B with another key sends
/// both, pasted text never closes the view, and a resize reaches the Tick. A second client attaches
/// beside the first and its keys reach the same process. A lone Esc closes only its own view,
/// leaves the Tick and the other view running and restores the terminal.
#[tokio::test(flavor = "multi_thread")]
async fn an_attached_view_reaches_the_same_process_until_esc_closes_it() {
    let host = Host::start().await;
    let opened = host.open_keys().await;
    let home = host.directory.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let mut view = View::start(&host.grant(&opened).await, &home);
    view.shows(24, 80, "size 80x24");

    view.send(b"\x02\x1b");
    view.shows(24, 80, "keys 1b");
    view.send(b"\x02\x02");
    view.shows(24, 80, "keys 1b 02");
    view.send(b"\x02");
    std::thread::sleep(Duration::from_millis(200));
    view.send(b"x");
    view.shows(24, 80, "keys 1b 02 02 78");
    view.send("腾".as_bytes());
    view.shows(24, 80, "02 78 e8 85 be");
    view.send(b"\x1b[200~\x02\x1b\x1b[201~");
    view.shows(24, 80, "e8 85 be 1b 5b 32 30 30 7e 02 1b 1b 5b 32 30 31 7e");

    view.send(b"\x1b[<0;5;5M");
    view.shows(24, 80, "7e 1b 5b 3c 30 3b 35 3b 35 4d");

    let mut beside = View::start(&host.grant(&opened).await, &home);
    beside.shows(24, 80, "3c 30 3b 35 3b 35 4d");
    beside.send(b"y");
    beside.shows(24, 80, "35 4d 79");
    view.shows(24, 80, "35 4d 79");
    beside.send(b"\x1b");
    assert_eq!(beside.exit_code(), 0, "a lone Esc closes that view");
    view.send(b"z");
    view.shows(24, 80, "4d 79 7a");

    view.master
        .resize(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    view.shows(30, 100, "size 100x30");

    view.send(b"\x1b");
    assert_eq!(view.exit_code(), 0, "a lone Esc closes the view");
    let output = view.output_text();
    let restored = &output[output.len().saturating_sub(160)..];
    for reset in [
        "\x1b[0m",
        "\x1b[?25h",
        "\x1b[?1000l",
        "\x1b[?1006l",
        "\x1b[?2004l",
    ] {
        assert!(
            restored.contains(reset),
            "{reset:?} restores the terminal: {restored:?}"
        );
    }
    assert!(
        output.ends_with("\x1b[?1049l"),
        "the view's own screen is left last"
    );
    assert_eq!(
        unsafe { libc::kill(opened.business_pid as i32, 0) },
        0,
        "the Tick keeps running"
    );
    let mut again = View::start(&host.grant(&opened).await, &home);
    again.shows(24, 80, "size 80x24");
    again.send(b"\x1b");
    assert_eq!(
        again.exit_code(),
        0,
        "a later view attaches as the first did"
    );
}

/// Views of different sizes show one screen. It takes the size of the view that typed last, or
/// of the newest view while none typed; a smaller view shows its top-left part cut at its own
/// edges and a larger one shows it whole with blank space beyond. When the view it follows
/// closes, the screen takes the size of the one that remains.
#[tokio::test(flavor = "multi_thread")]
async fn views_of_different_sizes_show_one_screen_cut_or_padded_to_fit() {
    let host = Host::start().await;
    let opened = host.open_keys().await;
    let home = host.directory.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let mut large = View::start_sized(&host.grant(&opened).await, &home, 24, 80);
    large.shows(24, 80, "size 80x24");
    let mut small = View::start_sized(&host.grant(&opened).await, &home, 10, 40);
    small.shows(10, 40, "size 40x10");
    large.shows(24, 80, "size 40x10");

    large.send(b"abcdefghijklmnopqrstuvwx");
    large.shows(24, 80, "count 24");
    small.shows(10, 40, "count 24");
    let typed = large.rows(24, 80);
    assert_eq!(typed[0], "size 80x24", "{typed:?}");
    assert!(typed[1].len() > 40, "{typed:?}");
    let cut = small.rows(10, 40);
    assert_eq!(cut[0], "size 80x24");
    assert_eq!(cut[1], typed[1][..40], "the top-left part of the screen");
    assert_eq!(cut[2], "count 24", "a cut line never runs into the next");

    small.send(b"y");
    small.shows(10, 40, "count 25");
    large.shows(24, 80, "count 25");
    let padded = large.rows(24, 80);
    assert_eq!(padded[0], "size 40x10", "{padded:?}");
    assert_eq!(
        padded[1].len(),
        40,
        "the screen's lines end at its own width: {padded:?}"
    );
    assert_eq!(padded[3], "count 25");
    assert!(
        padded[4..].iter().all(String::is_empty),
        "blank beyond the screen: {padded:?}"
    );

    small.send(b"\x1b");
    assert_eq!(small.exit_code(), 0);
    large.shows(24, 80, "size 80x24");
    large.send(b"\x1b");
    assert_eq!(large.exit_code(), 0);
    assert_eq!(
        unsafe { libc::kill(opened.business_pid as i32, 0) },
        0,
        "the Tick keeps running"
    );
}

/// A paste far larger than the attach client's input queue of 16 reads of 4 KiB reaches the
/// same Tick process whole and in order: a full queue holds further keys back in the terminal
/// instead of dropping them, and the Esc and Ctrl+B keys inside the paste are passed through.
#[tokio::test(flavor = "multi_thread")]
async fn a_paste_larger_than_the_input_queue_reaches_the_tick_whole_and_in_order() {
    const PASTED_BYTES: usize = 256 * 1024;
    const PRINTABLE_ASCII: std::ops::RangeInclusive<u8> = b' '..=b'~';
    let host = Host::start().await;
    let opened = host.open_keys().await;
    let home = host.directory.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let mut view = View::start(&host.grant(&opened).await, &home);
    view.shows(24, 80, "size 80x24");

    let mut paste = b"\x1b[200~".to_vec();
    paste.extend(PRINTABLE_ASCII.cycle().take(PASTED_BYTES));
    paste.extend(b"\x1b\x02\x1b\x1b[201~");
    let mut writer = std::mem::replace(&mut view.writer, Box::new(std::io::sink()));
    let pasted = paste.clone();
    let writing = std::thread::spawn(move || {
        writer.write_all(&pasted).unwrap();
        writer.flush().unwrap();
        writer
    });
    view.shows(24, 80, &format!("count {}", paste.len()));
    view.writer = writing.join().unwrap();
    assert!(
        std::fs::read(host.directory.path().join("received")).unwrap() == paste,
        "every pasted byte arrives once and in order"
    );
    assert_eq!(
        unsafe { libc::kill(opened.business_pid as i32, 0) },
        0,
        "the same Tick process received it"
    );

    view.send(b"\x1b");
    assert_eq!(
        view.exit_code(),
        0,
        "the view stayed open until a lone Esc closed it"
    );
}
