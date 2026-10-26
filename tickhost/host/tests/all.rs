//! Every tickhost integration test, compiled as one binary.

#[path = "all/cli.rs"]
mod cli;
#[path = "all/content_identity.rs"]
mod content_identity;
#[path = "all/hosted_launch.rs"]
mod hosted_launch;
#[path = "all/protocol.rs"]
mod protocol;
#[path = "all/shell_tasks.rs"]
mod shell_tasks;
#[path = "all/spawn_paths.rs"]
mod spawn_paths;

/// A short canonical directory for test sockets, whose paths are limited to about 100 bytes;
/// macOS's `/tmp` is a symbolic link to `/private/tmp`.
#[cfg(target_os = "macos")]
const SHORT_TEMP_ROOT: &str = "/private/tmp";
#[cfg(not(target_os = "macos"))]
const SHORT_TEMP_ROOT: &str = "/tmp";

struct OwnedChild(std::process::Child);
impl std::ops::Deref for OwnedChild {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for OwnedChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

#[test]
fn child_guard_reaps_a_process_when_fixture_initialization_panics() {
    let mut pid = 0;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let child = OwnedChild(
            std::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .unwrap(),
        );
        pid = child.id() as i32;
        panic!("fixture initialization probe");
    }));
    assert!(pid > 0);
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    if alive {
        unsafe {
            libc::kill(pid, libc::SIGKILL);
            libc::waitpid(pid, std::ptr::null_mut(), 0);
        }
    }
    assert!(result.is_err());
    assert!(!alive, "fixture initialization left PID {pid}");
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_some() {
            return;
        }
        unsafe {
            libc::kill(self.0.id() as i32, libc::SIGTERM);
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(12);
        while self.0.try_wait().ok().flatten().is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
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

fn runtime_directory(prefix: &str) -> tempfile::TempDir {
    let directory = tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(SHORT_TEMP_ROOT)
        .unwrap();
    register_runtime_directory(directory.path());
    directory
}
