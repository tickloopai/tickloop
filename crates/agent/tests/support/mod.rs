#![allow(dead_code)]

pub mod cli_runtime;
mod common;
pub mod fixture_cli;
pub mod herdr;
pub mod hosted;
pub mod provider_transport;
pub mod routed;
mod runtime;
pub mod tick_admission_contract;

pub use common::*;
pub use runtime::runtime_dir;

/// Small transport limits that let tests cross the frame and chunk boundaries with little data.
pub const TRANSPORT_LIMITS: tickloop_agent_protocol::AccessLimits =
    match tickloop_agent_protocol::AccessLimits::new(64 << 10, 16 << 10) {
        Ok(limits) => limits,
        Err(_) => panic!("the test AccessLimits are valid"),
    };

/// A watch connects to its Tick once and then reconnects at most
/// [`RECONNECTS_PER_WATCH`](tickloop_agent::tick_stream::RECONNECTS_PER_WATCH) times.
pub const CONNECTS_PER_WATCH: usize =
    1 + tickloop_agent::tick_stream::RECONNECTS_PER_WATCH as usize;

pub fn model_script_requests(
    snapshot: &tickloop_agent::runtime::ChatSnapshot,
) -> Vec<&tickloop_agent::runtime::RequestUsageRecord> {
    snapshot
        .request_usage
        .entries()
        .iter()
        .filter(|entry| entry.kind != tickloop_agent::runtime::RequestKind::Title)
        .collect()
}

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
