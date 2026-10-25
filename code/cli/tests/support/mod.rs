#![allow(dead_code)]

pub mod cli_runtime;
pub mod fixture_cli;
pub mod herdr;
pub mod hosted;
mod runtime;

pub use runtime::runtime_dir;

/// Whether `process_id` has exited, reaping it when it is a child of this test process.
pub fn has_exited(process_id: u32) -> bool {
    let process_id = process_id as libc::pid_t;
    let mut status = 0;
    let reaped = unsafe { libc::waitpid(process_id, &mut status, libc::WNOHANG) };
    reaped == process_id || runtime::is_dead(process_id)
}

/// Stops the Tickhost a test started, if one still runs, through its management channel, so
/// a failed assertion never leaves a host and its Ticks behind.
pub fn stop_tickhost(runtime: &std::path::Path, key_file: &std::path::Path) {
    use std::io::{BufRead, BufReader, Write};
    let (Ok(key), Ok(stream)) = (
        std::fs::read_to_string(key_file),
        std::os::unix::net::UnixStream::connect(runtime.join("host.sock")),
    ) else {
        return;
    };
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(10)));
    let mut writer = &stream;
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    let hello = serde_json::json!({"channel":"management","major":tickhost_connect::hosted::MANAGEMENT_MAJOR,"minor":tickhost_connect::hosted::MANAGEMENT_MINOR,"controller_key":key.trim()});
    if writeln!(writer, "{hello}").is_err() || reader.read_line(&mut line).is_err() {
        return;
    }
    let Ok(hello) = serde_json::from_str::<serde_json::Value>(&line) else {
        return;
    };
    let shutdown = serde_json::json!({"id":1,"method":"shutdown_host","params":{"epoch":hello["result"]["epoch"],"operation":uuid::Uuid::new_v4()}});
    if writeln!(writer, "{shutdown}").is_ok() {
        line.clear();
        let _ = reader.read_line(&mut line);
    }
}
