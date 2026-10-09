//! The `tickhost` executable: the hidden `run` service as `ps` shows it, `stop`, and the checks
//! `open` makes before anything starts.

use std::{
    io::Write,
    process::{Command, Stdio},
    time::Duration,
};

use serde_json::json;
use tickhost::{host, paths::HostPaths};
use tickhost_connect::hosted::{
    BUILD_VERSION, CAPABILITY_LIFECYCLE, CONTROL_TIMEOUT, HostConnection, Role,
};

fn tickhost() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tickhost"))
}

/// A `tickhost run` started the way the Agent daemon starts it, and its controller.
struct Service {
    _directory: tempfile::TempDir,
    paths: HostPaths,
    process: std::process::Child,
    controller: HostConnection,
}

impl Service {
    async fn start() -> Self {
        let directory = tempfile::Builder::new()
            .prefix("thc")
            .tempdir_in("/private/tmp")
            .unwrap();
        let paths = HostPaths {
            runtime: directory.path().join("h"),
        };
        let key = host::random_token();
        let mut process = tickhost()
            .arg(tickhost::cli::RUN_COMMAND)
            .env(tickhost::paths::RUNTIME_DIR_ENV, &paths.runtime)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let config = host::HostConfig {
            controller_key: key.clone(),
            idle_timeout: host::DEFAULT_IDLE_TIMEOUT,
        };
        let mut stdin = process.stdin.take().unwrap();
        writeln!(stdin, "{}", serde_json::to_string(&config).unwrap()).unwrap();
        drop(stdin);
        let role = Role::Controller {
            controller_key: key,
            daemon_epoch: uuid::Uuid::new_v4(),
        };
        let deadline = tokio::time::Instant::now() + CONTROL_TIMEOUT;
        let controller = loop {
            match HostConnection::connect(&paths.socket(), role.clone(), &[CAPABILITY_LIFECYCLE])
                .await
            {
                Ok(connection) => break connection,
                Err(_) if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(error) => panic!("tickhost run never served: {error}"),
            }
        };
        Self {
            _directory: directory,
            paths,
            process,
            controller,
        }
    }

    fn stop(&self) -> std::process::Output {
        tickhost()
            .arg("stop")
            .env(tickhost::paths::RUNTIME_DIR_ENV, &self.paths.runtime)
            .output()
            .unwrap()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_service_shows_as_tickhost_run_and_reports_its_build_version() {
    let mut service = Service::start().await;
    let shown = Command::new("ps")
        .args(["-o", "args=", "-p", &service.process.id().to_string()])
        .output()
        .unwrap();
    let shown = String::from_utf8_lossy(&shown.stdout);
    assert!(shown.trim().ends_with("tickhost run"), "{shown}");
    let version = tickhost().arg("--version").output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        format!("tickhost {BUILD_VERSION}")
    );
    assert!(service.stop().status.success());
    assert!(service.process.wait().unwrap().success());
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_ends_every_hosted_tick_and_leaves_them_lost_to_the_daemon() {
    let mut service = Service::start().await;
    let counter = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tick/examples/counter.py")
        .canonicalize()
        .unwrap();
    let opened = service
        .controller
        .request(
            "open",
            json!({
                "operation": uuid::Uuid::new_v4(),
                "owner": {"root":"fixture-root","chat":uuid::Uuid::new_v4()},
                "owner_revision": 0,
                "command": [env!("CARGO_BIN_EXE_tickhost"), "open", "--", "/usr/bin/python3", "-u", counter],
                "cwd": counter.parent().unwrap(),
                "environment": std::env::vars().collect::<Vec<_>>(),
                "title": null,
            }),
            CONTROL_TIMEOUT + Duration::from_secs(2),
        )
        .await
        .unwrap();
    let business = opened["business_pid"].as_u64().unwrap() as libc::pid_t;
    let epoch = service.controller.epoch();

    let stopped = service.stop();
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    assert!(String::from_utf8_lossy(&stopped.stdout).contains("Stopped the Tickhost"));
    assert!(service.process.wait().unwrap().success());
    assert!(!service.paths.host_running().unwrap());
    assert!(!service.paths.socket().exists());
    assert_ne!(unsafe { libc::kill(business, 0) }, 0, "the Tick has ended");
    assert!(
        !service.paths.was_emptied(epoch),
        "a stopped host leaves no empty-host mark, so the daemon counts its Ticks as lost"
    );

    let again = service.stop();
    assert!(again.status.success());
    assert!(String::from_utf8_lossy(&again.stdout).contains("No Tickhost is running"));
}

#[test]
fn stop_never_signals_a_process_the_host_lock_names_unless_it_is_the_live_tickhost_run() {
    use std::os::fd::AsRawFd;
    let directory = tempfile::Builder::new()
        .prefix("ths")
        .tempdir_in("/private/tmp")
        .unwrap();
    let paths = HostPaths {
        runtime: directory.path().join("h"),
    };
    paths.prepare().unwrap();
    let mut unrelated = Command::new("/bin/sleep").arg("600").spawn().unwrap();
    std::fs::write(paths.lock(), unrelated.id().to_string()).unwrap();
    let stop = || {
        tickhost()
            .arg("stop")
            .env(tickhost::paths::RUNTIME_DIR_ENV, &paths.runtime)
            .output()
            .unwrap()
    };
    let alive = |child: &mut std::process::Child| child.try_wait().unwrap().is_none();

    let unlocked = stop();
    assert!(unlocked.status.success());
    assert!(String::from_utf8_lossy(&unlocked.stdout).contains("No Tickhost is running"));
    assert!(alive(&mut unrelated), "a stale lock names no live host");

    let lock = std::fs::File::open(paths.lock()).unwrap();
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let held = stop();
    assert_eq!(held.status.code(), Some(2));
    let refusal = String::from_utf8_lossy(&held.stderr);
    assert!(
        refusal.contains(&format!("names process {}", unrelated.id()))
            && refusal.contains("is not `tickhost run`")
            && refusal.contains("refusing to signal it"),
        "{refusal}"
    );
    std::thread::sleep(Duration::from_millis(200));
    assert!(alive(&mut unrelated), "the process the lock names survives");
    unrelated.kill().unwrap();
    unrelated.wait().unwrap();
    drop(lock);
}

#[test]
fn open_refuses_a_missing_program_and_a_run_without_a_host() {
    let missing = tickhost()
        .args(["__preflight", "open", "--", "/nonexistent/program"])
        .output()
        .unwrap();
    assert_eq!(missing.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&missing.stderr).contains("executable not found"));

    let mut alone = tickhost();
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("HERDR_") {
            alone.env_remove(name);
        }
    }
    let alone = alone
        .args(["open", "--", "/usr/bin/true"])
        .env_remove("TICKLOOP_TICKHOST_LAUNCH")
        .output()
        .unwrap();
    assert_eq!(alone.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&alone.stderr)
            .contains("requires a Herdr Pane or a Tickhost launch"),
        "{}",
        String::from_utf8_lossy(&alone.stderr)
    );
}

#[test]
fn run_and_the_preflight_entry_stay_out_of_help() {
    let help = tickhost().arg("--help").output().unwrap();
    let help = String::from_utf8_lossy(&help.stdout);
    for shown in ["open", "stop"] {
        assert!(help.contains(&format!("\n  {shown}")), "{help}");
    }
    assert!(!help.contains("\n  run"), "{help}");
    assert!(!help.contains("__preflight"), "{help}");
}
