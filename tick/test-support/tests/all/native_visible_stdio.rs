#![cfg(unix)]

use std::{
    fs::File,
    io::{BufRead, BufReader, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::process::CommandExt,
    },
    process::{Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

use serde_json::{Value, json};
use tick_protocol::{FRAME_TOO_LARGE, REVISION_MISMATCH, TickDescriptor};

const VISIBLE_FRAME_TIMEOUT: Duration = Duration::from_secs(10);
const VISIBLE_FRAME_POLL_INTERVAL: Duration = Duration::from_millis(10);

fn visible_kline() -> (
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
    let mut command = Command::new(env!("CARGO_BIN_EXE_tick-mock"));
    command
        .args(["kline", "700.HK"])
        .env("TICKLOOP_INSTANCE_ID", "hosted:visible-kline")
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
    let (mut child, mut tty, frames, screen) = visible_kline();
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
    let (mut child, mut tty, frames, screen) = visible_kline();
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
    let mut fields: Vec<_> = first["params"].as_object().unwrap().keys().collect();
    fields.sort();
    assert_eq!(fields, ["at", "data", "seq"]);

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
    assert!(read["result"]["seq"].as_u64().unwrap() >= first["params"]["seq"].as_u64().unwrap());

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
    assert_eq!(describe["result"]["protocol"], 3);
    assert_eq!(describe["result"]["kind"], "kline");
    assert!(
        describe["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "mark")
    );
    let descriptor: TickDescriptor = serde_json::from_value(describe["result"].clone()).unwrap();
    writeln!(stdin, "{}", json!({"jsonrpc":"2.0","id":13,"method":"tick/call","params":{"tool":"mark","args":{"label":"test","price":500.0},"revision":"stale"}})).unwrap();
    let stale_catalog = loop {
        let frame = next(&frames);
        if frame["id"] == 13 {
            break frame;
        }
    };
    assert_eq!(stale_catalog["error"]["code"], REVISION_MISMATCH);
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0","id":11,"method":"tick/read"})
    )
    .unwrap();
    let unmarked_read = loop {
        let frame = next(&frames);
        if frame["id"] == 11 {
            break frame;
        }
    };
    assert_eq!(
        unmarked_read["result"]["data"]["marks"],
        json!([]),
        "the call naming a stale revision never ran"
    );

    writeln!(stdin, "not-json").unwrap();
    writeln!(stdin, "{}", "x".repeat(600_000)).unwrap();
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0","id":14,"method":"tick/read","params":{"padding":"x".repeat(600_000)}})
    )
    .unwrap();
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0","id":12,"method":"tick/read"})
    )
    .unwrap();
    let next_answer = || {
        loop {
            let frame = next(&frames);
            if frame.get("id").is_some() {
                break frame;
            }
        }
    };
    // The read fast path can overtake the runtime's oversized-frame error.
    let answers = [next_answer(), next_answer()];
    let mut ids = answers
        .iter()
        .map(|answer| answer["id"].as_u64().expect("a request id"))
        .collect::<Vec<_>>();
    ids.sort_unstable();
    assert_eq!(ids, [12, 14], "lines naming no request id go unanswered");
    let oversized = answers.iter().find(|answer| answer["id"] == 14).unwrap();
    assert_eq!(oversized["error"]["code"], FRAME_TOO_LARGE);
    let read = answers.iter().find(|answer| answer["id"] == 12).unwrap();
    assert_eq!(read["result"]["data"]["period"], "day");

    tty.write_all(b"5").unwrap();
    let switched = loop {
        let frame = next(&frames);
        if frame["method"] == "tick/update" && frame["params"]["data"]["period"] == "5m" {
            break frame;
        }
    };
    assert!(switched["params"]["seq"].as_u64().unwrap() > first["params"]["seq"].as_u64().unwrap());

    writeln!(stdin, "{}", json!({"jsonrpc":"2.0","id":2,"method":"tick/call","params":{"tool":"mark","args":{"label":"test","price":500.0},"revision":descriptor.revision}})).unwrap();
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
    let deadline = std::time::Instant::now() + VISIBLE_FRAME_TIMEOUT;
    loop {
        let rendered = String::from_utf8_lossy(&screen.lock().unwrap()).into_owned();
        if rendered.contains("5m") && rendered.contains("test") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "visible TTY did not show switched period and mark: {rendered}"
        );
        std::thread::sleep(VISIBLE_FRAME_POLL_INTERVAL);
    }
    writeln!(stdin, "{}", json!({"jsonrpc":"2.0","id":3,"method":"tick/call","params":{"tool":"unmark","args":{"label":"test"},"revision":descriptor.revision}})).unwrap();
    let unmarked = loop {
        let frame = next(&frames);
        if frame["method"] == "tick/update"
            && frame["params"]["seq"].as_u64().unwrap_or(0)
                > marked["params"]["seq"].as_u64().unwrap()
        {
            break frame;
        }
    };
    assert_eq!(unmarked["params"]["data"]["marks"], json!([]));
    let size = libc::winsize {
        ws_row: 25,
        ws_col: 81,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    assert_eq!(
        unsafe { libc::ioctl(tty.as_raw_fd(), libc::TIOCSWINSZ, &size) },
        0
    );
    tty.write_all(b"q").unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while child.try_wait().unwrap().is_none() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    if child.try_wait().unwrap().is_none() {
        child.kill().unwrap();
        child.wait().unwrap();
        panic!("Tick did not exit after a pending terminal redraw and q");
    }
    assert!(child.wait().unwrap().success());
}

#[test]
fn native_terminal_restores_after_resize_and_termination_signal() {
    let (mut child, tty, frames, screen) = visible_kline();
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
