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

const FRAME_TIMEOUT: Duration = Duration::from_secs(10);

/// `tick-reader` as its host launches it: a Tick Instance with the protocol on stdin/stdout and
/// its view on the controlling terminal of a new session.
fn hosted_reader(directory: &Path, documents: &[&str]) -> (Child, File, mpsc::Receiver<Value>) {
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
                &raw mut size,
            )
        },
        0
    );
    let mut command = Command::new(env!("CARGO_BIN_EXE_tick-reader"));
    command
        .args(documents)
        .current_dir(directory)
        .env("TICKLOOP_INSTANCE_ID", "hosted:visible-reader")
        .env("LANG", "en_US.UTF-8")
        .env("HOME", directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() < 0 || libc::ioctl(slave, libc::TIOCSCTTY as _, 0) < 0 {
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

fn wait_for_exit(child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while child.try_wait().unwrap().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    if child.try_wait().unwrap().is_none() {
        child.kill().unwrap();
        panic!("tick-reader did not exit after q");
    }
    assert!(child.wait().unwrap().success());
}

#[test]
fn a_hosted_reader_describes_reads_calls_and_updates_over_protocol_3() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("release.md"),
        "# Quarterly results\n\nRevenue was $12.8 billion.\n",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("outlook.md"),
        "# Outlook\n\nGuidance was raised.\n",
    )
    .unwrap();
    let (mut child, mut tty, frames) = hosted_reader(directory.path(), &["./release.md"]);

    let first = frame_where(&frames, |frame| frame["method"] == "tick/update");
    assert_eq!(
        first["params"]["data"]["documents"][0]["title"],
        "release.md"
    );

    request(&mut child, 1, "tick/describe", json!({}));
    let describe = frame_where(&frames, |frame| frame["id"] == 1);
    assert_eq!(describe["result"]["protocol"], 3);
    assert_eq!(describe["result"]["kind"], "reader");
    let revision = describe["result"]["revision"].clone();

    request(&mut child, 2, "tick/read", json!({}));
    let read = frame_where(&frames, |frame| frame["id"] == 2);
    assert_eq!(
        read["result"]["data"]["documents"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    request(
        &mut child,
        3,
        "tick/call",
        json!({"tool":"search","args":{"query":"revenue"},"revision":revision}),
    );
    let search = frame_where(&frames, |frame| frame["id"] == 3);
    assert!(
        search["result"]["matches"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Revenue was $12.8 billion."),
        "{search}"
    );

    request(
        &mut child,
        4,
        "tick/call",
        json!({"tool":"open_document","args":{"location":"./outlook.md"},"revision":revision}),
    );
    let (mut opened, mut answered) = (None, false);
    while opened.is_none() || !answered {
        let frame = frames.recv_timeout(FRAME_TIMEOUT).unwrap();
        if frame["id"] == 4 {
            assert!(frame.get("error").is_none(), "{frame}");
            answered = true;
        } else if frame["method"] == "tick/update"
            && frame["params"]["data"]["documents"]
                .as_array()
                .is_some_and(|documents| documents.len() == 2)
        {
            opened = Some(frame);
        }
    }
    assert!(
        opened.unwrap()["params"]["seq"].as_u64().unwrap()
            > first["params"]["seq"].as_u64().unwrap()
    );

    tty.write_all(b"q").unwrap();
    wait_for_exit(&mut child);
}

#[test]
fn native_visible_mode_requires_a_controlling_tty() {
    let mut document = tempfile::Builder::new().suffix(".md").tempfile().unwrap();
    writeln!(document, "# Original\n\nComplete text.").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_tick-reader"))
        .arg(document.path())
        .env("TICKLOOP_INSTANCE_ID", "hosted:without-tty")
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
