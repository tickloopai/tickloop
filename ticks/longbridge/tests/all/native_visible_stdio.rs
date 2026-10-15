#![cfg(all(unix, feature = "live-api"))]

use std::{
    fs::File,
    io::{BufRead, BufReader, Write},
    os::{fd::FromRawFd, unix::process::CommandExt},
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

use serde_json::{Value, json};

/// Live candles take a quote connection and a history request before the first update.
const FRAME_TIMEOUT: Duration = Duration::from_secs(30);

/// `tick-longbridge` as its host launches it: a Tick Instance with the protocol on
/// stdin/stdout and its view on the controlling terminal of a new session. It reads the login
/// in the caller's own home.
fn hosted_longbridge(arguments: &[&str]) -> (Child, File, mpsc::Receiver<Value>) {
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
    let mut command = Command::new(env!("CARGO_BIN_EXE_tick-longbridge"));
    command
        .args(arguments)
        .env("TICKLOOP_INSTANCE_ID", "hosted:live-kline")
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
    let mut screen = tty.try_clone().unwrap();
    std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        while let Ok(length) = std::io::Read::read(&mut screen, &mut chunk) {
            if length == 0 {
                break;
            }
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
    (child, tty, receiver)
}

fn frame_where(frames: &mpsc::Receiver<Value>, matches: impl Fn(&Value) -> bool) -> Value {
    loop {
        let frame = frames.recv_timeout(FRAME_TIMEOUT).unwrap();
        if matches(&frame) {
            return frame;
        }
    }
}

fn request(child: &mut Child, id: u64, method: &str, params: Value) {
    writeln!(
        child.stdin.as_mut().unwrap(),
        "{}",
        json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
    )
    .unwrap();
}

/// Run in your own terminal after `tick-longbridge auth`:
/// `cargo nextest run -p tickloop-tick-longbridge --features live-api --run-ignored only`.
#[test]
#[ignore = "reads live Longbridge candles with your own login"]
fn a_hosted_kline_describes_reads_calls_and_updates_over_protocol_3() {
    let (mut child, mut tty, frames) = hosted_longbridge(&["kline", "700.HK"]);

    let first = frame_where(&frames, |frame| frame["method"] == "tick/update");
    assert_eq!(first["params"]["data"]["symbol"], "700.HK");

    request(&mut child, 1, "tick/describe", json!({}));
    let describe = frame_where(&frames, |frame| frame["id"] == 1);
    assert_eq!(describe["result"]["protocol"], 3);
    assert_eq!(describe["result"]["kind"], "kline");
    let revision = describe["result"]["revision"].clone();

    request(&mut child, 2, "tick/read", json!({}));
    let read = frame_where(&frames, |frame| frame["id"] == 2);
    assert_eq!(read["result"]["data"]["period"], "day");

    request(
        &mut child,
        3,
        "tick/call",
        json!({"tool":"mark","args":{"label":"live","price":400.0},"revision":revision}),
    );
    let marked = frame_where(&frames, |frame| {
        frame["method"] == "tick/update"
            && frame["params"]["data"]["marks"]
                .as_array()
                .is_some_and(|marks| !marks.is_empty())
    });
    assert_eq!(marked["params"]["data"]["marks"][0]["label"], "live");

    tty.write_all(b"q").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while child.try_wait().unwrap().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    if child.try_wait().unwrap().is_none() {
        child.kill().unwrap();
        panic!("tick-longbridge did not exit after q");
    }
    assert!(child.wait().unwrap().success());
}
