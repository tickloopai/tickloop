#![cfg(unix)]

use std::{
    fs::File,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::process::CommandExt,
    },
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};

#[test]
fn panic_fixture() {
    if std::env::var_os("TICKLOOP_PANIC_FIXTURE").is_none() {
        return;
    }
    let _session =
        tick_market::terminal::VisibleTerminalSession::enter(None).unwrap_or_else(|error| {
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
                &raw mut size,
            )
        },
        0
    );
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "native_visible_stdio::panic_fixture",
            "--nocapture",
        ])
        .env("TICKLOOP_PANIC_FIXTURE", "1")
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
