#![cfg(unix)]

use std::{
    fs::File,
    io::{BufRead, BufReader, Write},
    os::{fd::FromRawFd, unix::process::CommandExt},
    path::Path,
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

use serde_json::{Value, json};

use crate::support::fake_opend;

/// A refresh waits out OpenD's minimum gap between chain requests, about three seconds.
const FRAME_TIMEOUT: Duration = Duration::from_secs(20);

/// `tick-futu` as its host launches it: a Tick Instance with the protocol on stdin/stdout and
/// its view on the controlling terminal of a new session.
fn hosted_options(home: &Path, port: u16) -> (Child, File, mpsc::Receiver<Value>) {
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
    let mut command = Command::new(env!("CARGO_BIN_EXE_tick-futu"));
    command
        .args(["options", "AAPL.US", "--port", &port.to_string()])
        .env("TICKLOOP_INSTANCE_ID", "hosted:visible-options")
        .env("LANG", "en_US.UTF-8")
        .env("HOME", home)
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

#[test]
fn a_hosted_options_tick_describes_reads_calls_and_updates_over_protocol_3() {
    let home = tempfile::tempdir().unwrap();
    let (mut child, mut tty, frames) = hosted_options(home.path(), fake_opend::start());

    let first = frame_where(&frames, |frame| frame["method"] == "tick/update");
    assert_eq!(first["params"]["data"]["source"], "futu");
    assert_eq!(first["params"]["data"]["symbol"], "AAPL.US");
    assert_eq!(first["params"]["data"]["expiry"], fake_opend::EXPIRY);

    request(&mut child, 1, "tick/describe", json!({}));
    let describe = frame_where(&frames, |frame| frame["id"] == 1);
    assert_eq!(describe["result"]["protocol"], 3);
    assert_eq!(describe["result"]["kind"], "options");
    let revision = describe["result"]["revision"].clone();

    request(&mut child, 2, "tick/read", json!({}));
    let read = frame_where(&frames, |frame| frame["id"] == 2);
    assert_eq!(read["result"]["data"]["chain"]["strikes"], 1);

    request(
        &mut child,
        3,
        "tick/call",
        json!({"tool":"snapshot","args":{},"revision":revision}),
    );
    let snapshot = frame_where(&frames, |frame| frame["id"] == 3);
    assert_eq!(snapshot["result"]["rows"][0]["strike"], 100.0, "{snapshot}");

    request(
        &mut child,
        4,
        "tick/call",
        json!({"tool":"refresh","args":{},"revision":revision}),
    );
    let refreshed = frame_where(&frames, |frame| {
        frame["method"] == "tick/update"
            && frame["params"]["data"]["observed_at"] != first["params"]["data"]["observed_at"]
    });
    assert!(
        refreshed["params"]["seq"].as_u64().unwrap() > first["params"]["seq"].as_u64().unwrap()
    );

    tty.write_all(b"q").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while child.try_wait().unwrap().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    if child.try_wait().unwrap().is_none() {
        child.kill().unwrap();
        panic!("tick-futu did not exit after q");
    }
    assert!(child.wait().unwrap().success());
}
