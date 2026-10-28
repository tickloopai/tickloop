//! Every tickhost/connect integration test, compiled as one binary.

#[path = "all/herdr_socket.rs"]
mod herdr_socket;
#[path = "all/session.rs"]
mod session;
#[path = "all/session_encoded_limits.rs"]
mod session_encoded_limits;
#[path = "all/session_observation.rs"]
mod session_observation;

fn session_directory() -> tempfile::TempDir {
    let directory = tempfile::Builder::new()
        .prefix("tls-")
        .tempdir_in(if cfg!(target_os = "macos") {
            "/private/tmp"
        } else {
            "/tmp"
        })
        .unwrap();
    register_runtime_directory(directory.path());
    directory
}

fn register_runtime_directory(path: &std::path::Path) {
    use std::io::Write;
    let Some(root) = std::env::var_os("TICKLOOP_TMPDIR") else {
        return;
    };
    let Ok(mut file) = std::fs::OpenOptions::new()
        .append(true)
        .open(std::path::Path::new(&root).join("owned-runtimes.jsonl"))
    else {
        return;
    };
    let mut record = serde_json::to_vec(path).unwrap();
    record.push(b'\n');
    file.write_all(&record).unwrap();
}
