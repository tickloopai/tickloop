#![cfg(unix)]

use std::{
    fs::File,
    io::{BufRead, BufReader, Write},
    os::fd::{AsRawFd, FromRawFd},
    os::unix::process::CommandExt,
    process::{Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

use serde_json::{Value, json};
use tick_connect::protocol::{TickDescriptor, descriptor_digest};

fn visible_demo() -> (
    std::process::Child,
    File,
    mpsc::Receiver<Value>,
    Arc<Mutex<Vec<u8>>>,
) {
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
    let mut command = Command::new(env!("CARGO_BIN_EXE_tick"));
    command
        .args(["demo", "kline"])
        .env("TICKLOOP_INSTANCE_ID", "herdr:visible-kline")
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
    let input = tty.try_clone().unwrap();
    let screen = Arc::new(Mutex::new(Vec::new()));
    let rendered = Arc::clone(&screen);
    std::thread::spawn(move || {
        let mut tty = tty;
        let mut chunk = [0u8; 4096];
        while let Ok(length) = std::io::Read::read(&mut tty, &mut chunk) {
            if length == 0 {
                break;
            }
            rendered.lock().unwrap().extend_from_slice(&chunk[..length]);
        }
    });
    let stdout = child.stdout.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let line = line.unwrap();
            sender
                .send(serde_json::from_str(&line).expect("stdout must contain JSON-RPC only"))
                .unwrap();
        }
    });
    (child, input, receiver, screen)
}

fn next(receiver: &mpsc::Receiver<Value>) -> Value {
    receiver.recv_timeout(Duration::from_secs(10)).unwrap()
}

#[test]
fn standalone_escape_does_not_swallow_the_next_key() {
    let (mut child, mut tty, frames, screen) = visible_demo();
    assert_eq!(next(&frames)["method"], "tick/update");
    std::thread::sleep(Duration::from_millis(200));
    tty.write_all(b"\x1b").unwrap();
    std::thread::sleep(Duration::from_millis(250));
    tty.write_all(b"5").unwrap();
    let switched = loop {
        let frame = next(&frames);
        if frame["method"] == "tick/update" && frame["params"]["data"]["period"] == "5m" {
            break frame;
        }
    };
    assert_eq!(switched["params"]["data"]["period"], "5m");
    tty.write_all(b"q").unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while child.try_wait().unwrap().is_none() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    if child.try_wait().unwrap().is_none() {
        child.kill().unwrap();
        panic!(
            "Tick did not exit after Esc and 5: {:?}",
            String::from_utf8_lossy(&screen.lock().unwrap())
                .chars()
                .rev()
                .take(300)
                .collect::<String>()
        );
    }
    assert!(child.wait().unwrap().success());
}

#[test]
fn visible_kline_keyboard_and_tool_share_the_same_stdio_instance() {
    let (mut child, mut tty, frames, screen) = visible_demo();
    let first = frames
        .recv_timeout(Duration::from_secs(10))
        .unwrap_or_else(|error| {
            panic!(
                "initial update missing: {error}; status={:?}; terminal={:?}",
                child.try_wait(),
                String::from_utf8_lossy(&screen.lock().unwrap())
            );
        });
    assert_eq!(first["method"], "tick/update");
    assert_eq!(first["params"]["version"], 3);
    assert_eq!(first["params"]["tick_instance_id"], "herdr:visible-kline");

    let stdin = child.stdin.as_mut().unwrap();
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0","id":1,"method":"tick/read"})
    )
    .unwrap();
    let read = loop {
        let frame = frames
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|error| {
                panic!(
                    "read missing: {error}; terminal={:?}",
                    String::from_utf8_lossy(&screen.lock().unwrap())
                );
            });
        if frame["id"] == 1 {
            break frame;
        }
    };
    assert_eq!(read["result"]["data"]["period"], "day");
    assert_eq!(read["result"]["tick_instance_id"], "herdr:visible-kline");

    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0","id":10,"method":"tick/describe"})
    )
    .unwrap();
    let describe = loop {
        let frame = next(&frames);
        if frame["id"] == 10 {
            break frame;
        }
    };
    assert_eq!(describe["result"]["protocol"], 2);
    assert_eq!(describe["result"]["tick_type"], "kline");
    assert!(
        describe["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "mark")
    );
    let descriptor: TickDescriptor = serde_json::from_value(describe["result"].clone()).unwrap();
    writeln!(stdin, "{}", json!({"jsonrpc":"2.0","id":13,"method":"tool/call","params":{"tool":"mark","args":{"label":"test","price":500.0},"expected_descriptor_digest":"stale"}})).unwrap();
    let stale_catalog = loop {
        let frame = next(&frames);
        if frame["id"] == 13 {
            break frame;
        }
    };
    assert_eq!(stale_catalog["error"]["code"], -32600);

    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0","id":11,"method":"tick/read","params":{"protocol":1}})
    )
    .unwrap();
    let rejected_version = loop {
        let frame = next(&frames);
        if frame["id"] == 11 {
            break frame;
        }
    };
    assert_eq!(rejected_version["error"]["code"], -32001);
    writeln!(stdin, "{}", json!({"jsonrpc":"2.0","id":12,"method":"tick/read","params":{"tick_instance_id":"herdr:other"}})).unwrap();
    let rejected_instance = loop {
        let frame = next(&frames);
        if frame["id"] == 12 {
            break frame;
        }
    };
    assert_eq!(rejected_instance["error"]["code"], -32002);

    writeln!(stdin, "not-json").unwrap();
    let parse_error = loop {
        let frame = next(&frames);
        if frame["error"]["code"] == -32700 {
            break frame;
        }
    };
    assert!(parse_error["id"].is_null());
    writeln!(stdin, "{}", "x".repeat(600_000)).unwrap();
    let oversized = loop {
        let frame = next(&frames);
        if frame["error"]["code"] == -32003 {
            break frame;
        }
    };
    assert!(oversized["id"].is_null());

    tty.write_all(b"5").unwrap();
    let switched = loop {
        let frame = next(&frames);
        if frame["method"] == "tick/update" && frame["params"]["data"]["period"] == "5m" {
            break frame;
        }
    };
    assert!(
        switched["params"]["sequence"].as_u64().unwrap()
            > first["params"]["sequence"].as_u64().unwrap()
    );

    writeln!(stdin, "{}", json!({"jsonrpc":"2.0","id":2,"method":"tool/call","params":{"tool":"mark","args":{"label":"test","price":500.0},"expected_descriptor_digest":descriptor_digest(&descriptor)}})).unwrap();
    let marked = loop {
        let frame = next(&frames);
        if frame["method"] == "tick/update"
            && frame["params"]["data"]["marks"]
                .as_array()
                .is_some_and(|marks| !marks.is_empty())
        {
            break frame;
        }
    };
    assert_eq!(marked["params"]["data"]["marks"][0]["label"], "test");
    writeln!(stdin, "{}", json!({"jsonrpc":"2.0","id":3,"method":"tool/call","params":{"tool":"unmark","args":{"label":"test"}}})).unwrap();
    let unmarked = loop {
        let frame = next(&frames);
        if frame["method"] == "tick/update"
            && frame["params"]["sequence"].as_u64().unwrap_or(0)
                > marked["params"]["sequence"].as_u64().unwrap()
        {
            break frame;
        }
    };
    assert_eq!(unmarked["params"]["data"]["marks"], json!([]));
    std::thread::sleep(Duration::from_millis(100));
    let rendered = screen.lock().unwrap();
    let rendered = String::from_utf8_lossy(&rendered);
    assert!(
        rendered.contains("5m") && rendered.contains("test"),
        "visible TTY did not show switched period and mark"
    );
    tty.write_all(b"q").unwrap();
    assert!(child.wait().unwrap().success());
}

#[test]
fn native_terminal_restores_after_resize_and_termination_signal() {
    let (mut child, tty, frames, screen) = visible_demo();
    assert_eq!(next(&frames)["method"], "tick/update");
    let before_resize = screen.lock().unwrap().len();
    let size = libc::winsize {
        ws_row: 18,
        ws_col: 60,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    assert_eq!(
        unsafe { libc::ioctl(tty.as_raw_fd(), libc::TIOCSWINSZ, &size) },
        0
    );
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGWINCH) }, 0);
    std::thread::sleep(Duration::from_millis(150));
    assert!(screen.lock().unwrap().len() > before_resize);
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGTERM) }, 0);
    assert!(child.wait().unwrap().success());
    std::thread::sleep(Duration::from_millis(50));
    let rendered = screen.lock().unwrap();
    assert!(
        rendered
            .windows(b"\x1b[?1049l".len())
            .any(|part| part == b"\x1b[?1049l")
    );
    let mut restored: libc::termios = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::tcgetattr(tty.as_raw_fd(), &mut restored) },
        0
    );
    assert_ne!(restored.c_lflag & libc::ECHO, 0);
}

#[test]
fn panic_fixture() {
    if std::env::var_os("TICKLOOP_PANIC_FIXTURE").is_none() {
        return;
    }
    let _session =
        tickloop_ticks::terminal::VisibleTerminalSession::enter(None).unwrap_or_else(|error| {
            let message = format!("terminal enter failed: {error}\n");
            unsafe {
                libc::write(libc::STDERR_FILENO, message.as_ptr().cast(), message.len());
            }
            panic!("{error}");
        });
    panic!("native terminal panic fixture");
}

#[test]
fn native_terminal_restores_after_panic() {
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
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "panic_fixture", "--nocapture"])
        .env("TICKLOOP_PANIC_FIXTURE", "1")
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
    let child = command.spawn().unwrap();
    unsafe {
        libc::close(slave);
    }
    let tty = unsafe { File::from_raw_fd(master) };
    let terminal_fd = tty.try_clone().unwrap();
    let screen = Arc::new(Mutex::new(Vec::new()));
    let rendered = Arc::clone(&screen);
    std::thread::spawn(move || {
        let mut tty = tty;
        let mut chunk = [0u8; 4096];
        while let Ok(length) = std::io::Read::read(&mut tty, &mut chunk) {
            if length == 0 {
                break;
            }
            rendered.lock().unwrap().extend_from_slice(&chunk[..length]);
        }
    });
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    std::thread::sleep(Duration::from_millis(50));
    let rendered = screen.lock().unwrap();
    assert!(
        rendered
            .windows(b"\x1b[?1049l".len())
            .any(|part| part == b"\x1b[?1049l"),
        "terminal={:?}; stdout={:?}",
        String::from_utf8_lossy(&rendered),
        String::from_utf8_lossy(&output.stdout)
    );
    let mut restored: libc::termios = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::tcgetattr(terminal_fd.as_raw_fd(), &mut restored) },
        0
    );
    assert_ne!(restored.c_lflag & libc::ECHO, 0);
}

#[test]
fn native_visible_mode_requires_a_controlling_tty() {
    let output = Command::new(env!("CARGO_BIN_EXE_tick"))
        .args(["demo", "kline"])
        .env("TICKLOOP_INSTANCE_ID", "herdr:without-tty")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("controlling TTY"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
