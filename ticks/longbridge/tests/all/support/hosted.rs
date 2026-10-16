//! `tick-longbridge` as its host launches it: a Tick Instance with the protocol on stdin/stdout
//! and its view on the controlling terminal of a new session.

use std::{
    fs::File,
    io::{BufRead, BufReader, Write},
    os::{fd::FromRawFd, unix::process::CommandExt},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

use serde_json::{Value, json};

pub struct Hosted {
    pub child: Child,
    pub tty: File,
    frames: mpsc::Receiver<Value>,
    /// Everything the Tick wrote to its terminal so far.
    screen: Arc<Mutex<Vec<u8>>>,
}

/// Starts `command`, an invocation of `tick-longbridge`, as a Tick Instance on an 80x24 terminal.
pub fn launch(mut command: Command) -> Hosted {
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
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() < 0 || libc::ioctl(slave, libc::TIOCSCTTY.into(), 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            libc::dup2(slave, libc::STDERR_FILENO);
            if slave > libc::STDERR_FILENO {
                libc::close(slave);
            }
            libc::close(master);
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    unsafe {
        libc::close(slave);
    }
    let tty = unsafe { File::from_raw_fd(master) };
    let mut output = tty.try_clone().unwrap();
    let screen = Arc::new(Mutex::new(Vec::new()));
    let drawn = Arc::clone(&screen);
    std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        while let Ok(length) = std::io::Read::read(&mut output, &mut chunk) {
            if length == 0 {
                break;
            }
            drawn.lock().unwrap().extend_from_slice(&chunk[..length]);
        }
    });
    let stdout = child.stdout.take().unwrap();
    let (sender, frames) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let line = line.unwrap();
            let frame = serde_json::from_str(&line).expect("stdout must contain JSON-RPC only");
            if sender.send(frame).is_err() {
                return;
            }
        }
    });
    Hosted {
        child,
        tty,
        frames,
        screen,
    }
}

impl Hosted {
    pub fn request(&mut self, id: u64, method: &str, params: Value) {
        writeln!(
            self.child.stdin.as_mut().unwrap(),
            "{}",
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
        )
        .unwrap();
    }

    /// The next frame that `matches`, skipping the others, waiting at most `timeout` for each.
    pub fn frame_where(&self, timeout: Duration, matches: impl Fn(&Value) -> bool) -> Value {
        loop {
            let Ok(frame) = self.frames.recv_timeout(timeout) else {
                panic!(
                    "no frame within {timeout:?}; the terminal shows: {}",
                    self.screen()
                );
            };
            if matches(&frame) {
                return frame;
            }
        }
    }

    /// The next frame within `timeout`, if any.
    pub fn next_frame(&self, timeout: Duration) -> Option<Value> {
        self.frames.recv_timeout(timeout).ok()
    }

    /// The text on the Tick's terminal now.
    pub fn screen(&self) -> String {
        let mut terminal = vt100::Parser::new(24, 80, 0);
        terminal.process(&self.screen.lock().unwrap());
        terminal.screen().contents()
    }
}
