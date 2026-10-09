#![cfg(unix)]
//! The keys a built-in Tick answers in a real terminal. Esc and Ctrl+B belong to the view around
//! the Tick, Backspace returns one level outside an editor, and an editor keeps q as text while
//! Ctrl+C cancels it.

use std::{
    fs::File,
    io::Write,
    os::{fd::FromRawFd, unix::process::CommandExt},
    process::{Child, Command},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const UTF8_LOCALE: &str = "en_US.UTF-8";
const DRAW_TIMEOUT: Duration = Duration::from_secs(10);
const EXIT_TIMEOUT: Duration = Duration::from_secs(5);
/// Long enough for a key the Tick wrongly took as quit to end the process.
const SETTLE: Duration = Duration::from_millis(300);
const POLL_INTERVAL: Duration = Duration::from_millis(10);
/// Drawn once the Kline has loaded and its key loop runs.
const KLINE_READY: &str = "m MA off";
const EVENT_EDITOR: &str = "Event settings";

struct StandaloneKline {
    child: Child,
    tty: File,
    screen: Arc<Mutex<Vec<u8>>>,
    _home: tempfile::TempDir,
}

impl StandaloneKline {
    fn start() -> Self {
        let (mut master, mut slave) = (0, 0);
        let mut size = libc::winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut size,
                )
            },
            0
        );
        let home = tempfile::tempdir().unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_tick"));
        command
            .args(["builtin", "kline", "700.HK"])
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("LANG", UTF8_LOCALE)
            .env("TERM", "xterm-256color")
            .env("HOME", home.path());
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() < 0 || libc::ioctl(slave, libc::TIOCSCTTY.into(), 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                for stream in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
                    libc::dup2(slave, stream);
                }
                if slave > libc::STDERR_FILENO {
                    libc::close(slave);
                }
                libc::close(master);
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        unsafe {
            libc::close(slave);
        }
        let tty = unsafe { File::from_raw_fd(master) };
        let mut output = tty.try_clone().unwrap();
        let screen = Arc::new(Mutex::new(Vec::new()));
        let rendered = Arc::clone(&screen);
        std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            while let Ok(length) = std::io::Read::read(&mut output, &mut chunk) {
                if length == 0 {
                    break;
                }
                rendered.lock().unwrap().extend_from_slice(&chunk[..length]);
            }
        });
        let kline = Self {
            child,
            tty,
            screen,
            _home: home,
        };
        kline.draws_since(0, KLINE_READY);
        kline
    }

    /// How much the Tick has drawn so far, to wait for what a later key draws.
    fn drawn(&self) -> usize {
        self.screen.lock().unwrap().len()
    }

    fn draws_since(&self, start: usize, text: &str) {
        let deadline = Instant::now() + DRAW_TIMEOUT;
        while Instant::now() < deadline {
            if self.screen.lock().unwrap()[start..]
                .windows(text.len())
                .any(|window| window == text.as_bytes())
            {
                return;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        panic!("{text:?} was not drawn: {}", self.output());
    }

    fn press(&mut self, keys: &[u8]) {
        self.tty.write_all(keys).unwrap();
    }

    fn open_event_editor(&mut self) {
        let start = self.drawn();
        self.press(b"e");
        self.draws_since(start, EVENT_EDITOR);
    }

    fn keeps_running_after(&mut self, keys: &str) {
        std::thread::sleep(SETTLE);
        assert!(
            self.child.try_wait().unwrap().is_none(),
            "{keys} ended the Tick: {}",
            self.output()
        );
    }

    fn exits_after(&mut self, keys: &str) {
        let deadline = Instant::now() + EXIT_TIMEOUT;
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "{keys}: {status:?}");
                return;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        panic!("the Tick kept running after {keys}: {}", self.output());
    }

    fn output(&self) -> String {
        let screen = self.screen.lock().unwrap();
        let tail = screen.len().saturating_sub(600);
        String::from_utf8_lossy(&screen[tail..]).into_owned()
    }
}

impl Drop for StandaloneKline {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[test]
fn view_keys_and_backspace_leave_the_tick_running_and_q_quits() {
    let mut kline = StandaloneKline::start();
    kline.press(b"\x1b");
    kline.keeps_running_after("Esc");
    kline.press(b"\x02");
    kline.keeps_running_after("Ctrl+B");
    kline.press(b"\x7f");
    kline.keeps_running_after("Backspace outside an editor");
    kline.press(b"q");
    kline.exits_after("q");
}

#[test]
fn an_editor_keeps_q_as_text_and_ctrl_c_cancels_it_without_quitting() {
    let mut kline = StandaloneKline::start();
    kline.open_event_editor();
    kline.press(b"q");
    kline.keeps_running_after("q in the editor");
    kline.press(b"\x7f\x1b");
    kline.keeps_running_after("Backspace and Esc in the editor");
    kline.press(b"\x03");
    kline.keeps_running_after("Ctrl+C in the editor");
    kline.press(b"q");
    kline.exits_after("q once Ctrl+C closed the editor");
}

#[test]
fn ctrl_c_outside_an_editor_quits() {
    let mut kline = StandaloneKline::start();
    kline.press(b"\x03");
    kline.exits_after("Ctrl+C");
}

#[test]
fn a_termination_signal_ends_the_tick_even_while_it_edits() {
    let mut kline = StandaloneKline::start();
    kline.open_event_editor();
    assert_eq!(
        unsafe { libc::kill(kline.child.id() as libc::pid_t, libc::SIGTERM) },
        0
    );
    kline.exits_after("SIGTERM");
}
