//! The `tickhost` executable: the hidden `run` service as `ps` shows it, `stop`, and the hidden
//! `launch` entry.

use std::{
    io::Write,
    process::{Command, Stdio},
    time::Duration,
};

use serde_json::json;
use tickhost::shell::{self, ShellApproval, ShellIdentity, SpawnBinding, SpawnRequest};
use tickhost::{host, paths::HostPaths};
use tickhost_connect::hosted::{
    BUILD_VERSION, CAPABILITY_LIFECYCLE, CONTROL_TIMEOUT, HostConnection, Role,
};

fn tickhost() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tickhost"))
}

#[test]
fn release_identity_and_host_content_identity_are_reported_separately() {
    let release = tickhost().arg("--build-identity").output().unwrap();
    let content = tickhost().arg("--content-identity").output().unwrap();
    assert!(release.status.success());
    assert!(content.status.success());
    let release = String::from_utf8(release.stdout).unwrap();
    let content = String::from_utf8(content.stdout).unwrap();
    assert_eq!(release.trim(), env!("TICKLOOP_BUILD_IDENTITY"));
    assert_eq!(content.trim(), host::BUILD_IDENTITY);
    assert!(content.starts_with("tickhost:"));
    assert_ne!(release, content);
}

/// A `tickhost run` started the way the Agent daemon starts it, and its controller.
struct Service {
    config_root: ConfigRoot,
    process: crate::OwnedChild,
    controller: Option<HostConnection>,
    daemon_epoch: uuid::Uuid,
}

struct ConfigRoot {
    directory: tempfile::TempDir,
    paths: HostPaths,
}

impl ConfigRoot {
    fn new() -> Self {
        let directory = crate::runtime_directory("thc");
        let paths = HostPaths::of_config_root(Some(directory.path()));
        crate::register_runtime_directory(&paths.runtime);
        Self { directory, paths }
    }
}

impl Drop for ConfigRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.paths.runtime);
    }
}

impl Service {
    async fn start() -> Self {
        let config_root = ConfigRoot::new();
        let key = host::random_token();
        let mut process = crate::OwnedChild(
            tickhost()
                .arg(tickhost::cli::RUN_COMMAND)
                .env("TICKLOOP_CONFIG_DIR", config_root.directory.path())
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let config = host::HostConfig {
            controller_key: key.clone(),
            idle_timeout: host::DEFAULT_IDLE_TIMEOUT,
        };
        let mut stdin = process.stdin.take().unwrap();
        writeln!(stdin, "{}", serde_json::to_string(&config).unwrap()).unwrap();
        drop(stdin);
        let daemon_epoch = uuid::Uuid::new_v4();
        let role = Role::Controller {
            controller_key: key,
            daemon_epoch,
        };
        let deadline = tokio::time::Instant::now() + CONTROL_TIMEOUT;
        let controller = loop {
            match HostConnection::connect(
                &config_root.paths.socket(),
                role.clone(),
                &[CAPABILITY_LIFECYCLE],
            )
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
            config_root,
            process,
            controller: Some(controller),
            daemon_epoch,
        }
    }

    fn stop(&self) -> std::process::Output {
        tickhost()
            .arg("stop")
            .env("TICKLOOP_CONFIG_DIR", self.config_root.directory.path())
            .output()
            .unwrap()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_waits_for_the_host_process_with_a_running_shell() {
    check_shell_stop("sleep 30", false, false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_exits_after_the_shell_owner_and_daemon_disconnected() {
    check_shell_stop("sleep 30", false, true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_cancels_an_unconfirmed_shell_handoff_recovery() {
    check_shell_stop("sleep 30", true, true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_kills_the_shell_process_group_when_it_ignores_sigterm() {
    check_shell_stop(
        "trap '' TERM; sleep 30 & echo $! > child.pid; wait",
        false,
        false,
    )
    .await;
}

async fn check_shell_stop(command: &str, interrupted_handoff: bool, disconnect: bool) {
    let mut service = Service::start().await;
    let binding = SpawnBinding {
        operation: uuid::Uuid::new_v4(),
        identity: ShellIdentity {
            daemon_epoch: service.daemon_epoch,
            worker: uuid::Uuid::new_v4(),
            chat: uuid::Uuid::new_v4(),
        },
        approval: ShellApproval {
            shell: "/bin/sh".into(),
            command: command.into(),
            cwd: service.config_root.directory.path().to_path_buf(),
            environment: shell::environment_digest(&[]),
        },
    };
    let grant = service
        .controller
        .as_mut()
        .unwrap()
        .request("allow_spawn", json!(binding), CONTROL_TIMEOUT)
        .await
        .unwrap();
    let directory = std::fs::File::open(&binding.approval.cwd).unwrap();
    let output = std::fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .append(true)
        .open(binding.approval.cwd.join("shell.output"))
        .unwrap();
    let mut submitted = Some(
        shell::submit(
            &service.config_root.paths.socket(),
            &serde_json::from_value(grant).unwrap(),
            &SpawnRequest {
                binding,
                description: None,
                environment: vec![],
            },
            &directory,
            &output,
        )
        .await
        .unwrap(),
    );
    let spawned = submitted.as_ref().unwrap().spawned.clone();
    let pid = spawned.pid;
    let _group = OwnedGroup(pid);
    if interrupted_handoff {
        let successor = shell::adopt(&shell::ShellHandoff {
            endpoint: service.config_root.paths.socket(),
            instance: spawned.instance,
            owner_token: spawned.owner_token,
            handoff: uuid::Uuid::new_v4(),
        })
        .await
        .unwrap();
        drop(successor);
    }
    let child_pid = if command.contains("child.pid") {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if let Ok(pid) =
                std::fs::read_to_string(service.config_root.directory.path().join("child.pid"))
                && let Ok(pid) = pid.trim().parse::<libc::pid_t>()
            {
                break Some(pid);
            }
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    } else {
        None
    };
    assert!(shell::process_group_exists(pid));
    if disconnect {
        drop(submitted.take());
        drop(service.controller.take());
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let stopping = std::time::Instant::now();
    let stopped = service.stop();
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    assert!(
        service.process.try_wait().unwrap().is_some(),
        "stop reported success while tickhost run was alive"
    );
    assert!(!shell::process_group_exists(pid));
    assert!(!service.config_root.paths.socket().exists());
    assert!(!service.config_root.paths.host_running().unwrap());
    assert!(stopping.elapsed() < Duration::from_secs(5));
    if let Some(pid) = child_pid {
        assert_ne!(
            unsafe { libc::kill(pid, 0) },
            0,
            "the shell descendant survived"
        );
    }
    if let Some(mut submitted) = submitted {
        assert_eq!(
            submitted.exits.exited().await.unwrap().reason.as_deref(),
            Some(shell::CANCELLED_REASON)
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_forces_a_host_that_cannot_handle_sigterm_and_reports_incomplete_cleanup() {
    let mut service = Service::start().await;
    assert_eq!(
        unsafe { libc::kill(service.process.id() as libc::pid_t, libc::SIGSTOP) },
        0
    );
    let stopping = std::time::Instant::now();
    let stopped = service.stop();
    assert_eq!(stopped.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&stopped.stderr).contains("required SIGKILL"));
    assert!(!String::from_utf8_lossy(&stopped.stdout).contains("its Ticks have ended"));
    assert!(service.process.try_wait().unwrap().is_some());
    assert!(!service.config_root.paths.host_running().unwrap());
    assert!(!service.config_root.paths.socket().exists());
    assert!(stopping.elapsed() < Duration::from_secs(15));
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_waits_for_the_process_even_if_the_lock_becomes_available_first() {
    let mut service = Service::start().await;
    assert_eq!(
        unsafe { libc::kill(service.process.id() as libc::pid_t, libc::SIGSTOP) },
        0
    );
    let mut stopping = tickhost()
        .arg("stop")
        .env("TICKLOOP_CONFIG_DIR", service.config_root.directory.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(stopping.try_wait().unwrap().is_none());
    std::fs::rename(
        service.config_root.paths.lock(),
        service.config_root.paths.runtime.join("held.lock"),
    )
    .unwrap();
    std::fs::write(
        service.config_root.paths.lock(),
        service.process.id().to_string(),
    )
    .unwrap();
    assert!(!service.config_root.paths.host_running().unwrap());
    tokio::time::sleep(Duration::from_millis(200)).await;
    let premature = stopping.try_wait().unwrap().is_some();
    assert_eq!(
        unsafe { libc::kill(service.process.id() as libc::pid_t, libc::SIGCONT) },
        0
    );
    let stopped = stopping.wait_with_output().unwrap();
    assert!(
        !premature,
        "stop returned while the original host process was still paused"
    );
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    assert!(service.process.try_wait().unwrap().is_some());
    assert!(!service.config_root.paths.socket().exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_service_shows_as_tickhost_run_and_reports_its_build_version() {
    let mut service = Service::start().await;
    let shown = tickhost::process::arguments(service.process.id())
        .expect("the fixture-owned Tickhost arguments must be readable")
        .iter()
        .map(|argument| argument.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
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
    let wrapper = service.config_root.directory.path().join("counter.py");
    std::fs::write(
        &wrapper,
        r#"
import runpy, signal, subprocess, sys
signal.signal(signal.SIGTERM, signal.SIG_IGN)
signal.signal(signal.SIGHUP, signal.SIG_IGN)
child = subprocess.Popen(["/bin/sleep", "30"])
with open("protocol-child.pid", "w") as output:
    output.write(str(child.pid))
runpy.run_path(sys.argv[1], run_name="__main__")
"#,
    )
    .unwrap();
    let opened = service
        .controller
        .as_mut().unwrap()
        .request(
            "open",
            json!({
                "operation": uuid::Uuid::new_v4(),
                "owner": {"root":"fixture-root","chat":uuid::Uuid::new_v4()},
                "owner_revision": 0,
                "adapter": "protocol",
                "command": [env!("CARGO_BIN_EXE_tickhost"), tickhost::cli::LAUNCH_COMMAND, "--", "/usr/bin/python3", "-u", wrapper, counter],
                "cwd": service.config_root.directory.path(),
                "environment": std::env::vars().collect::<Vec<_>>(),
                "title": null,
            }),
            CONTROL_TIMEOUT + Duration::from_secs(2),
        )
        .await
        .unwrap();
    let business = opened["business_pid"].as_u64().unwrap() as libc::pid_t;
    let _protocol_group = OwnedGroup(tickhost::process::sample(business as u32).unwrap().parent);
    let terminal = service.controller.as_mut().unwrap().request("open", json!({
        "operation": uuid::Uuid::new_v4(),
        "owner": {"root":"fixture-root","chat":uuid::Uuid::new_v4()},
        "owner_revision": 0,
        "adapter": "terminal",
        "command": ["/bin/sh", "-c", "trap '' TERM HUP; sleep 30 & echo $! > terminal-child.pid; wait"],
        "cwd": service.config_root.directory.path(),
        "environment": std::env::vars().collect::<Vec<_>>(),
        "title": null,
    }), CONTROL_TIMEOUT).await.unwrap();
    let terminal_pid = terminal["business_pid"].as_u64().unwrap() as u32;
    let _terminal_group = OwnedGroup(terminal_pid);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let terminal_child = loop {
        if let Ok(pid) = std::fs::read_to_string(
            service
                .config_root
                .directory
                .path()
                .join("terminal-child.pid"),
        ) && let Ok(pid) = pid.trim().parse::<libc::pid_t>()
        {
            break pid;
        }
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let protocol_child = std::fs::read_to_string(
        service
            .config_root
            .directory
            .path()
            .join("protocol-child.pid"),
    )
    .unwrap()
    .trim()
    .parse::<libc::pid_t>()
    .unwrap();
    let epoch = service.controller.as_ref().unwrap().epoch();

    let stopped = service.stop();
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    assert!(String::from_utf8_lossy(&stopped.stdout).contains("Stopped the Tickhost"));
    assert!(service.process.wait().unwrap().success());
    assert!(!service.config_root.paths.host_running().unwrap());
    assert!(!service.config_root.paths.socket().exists());
    assert_ne!(unsafe { libc::kill(business, 0) }, 0, "the Tick has ended");
    for pid in [protocol_child, terminal_child, terminal_pid as libc::pid_t] {
        assert_ne!(
            unsafe { libc::kill(pid, 0) },
            0,
            "Tick descendant {pid} survived stop"
        );
    }
    assert!(
        !service.config_root.paths.was_emptied(epoch),
        "a stopped host leaves no empty-host mark, so the daemon counts its Ticks as lost"
    );

    let again = service.stop();
    assert!(again.status.success());
    assert!(String::from_utf8_lossy(&again.stdout).contains("No Tickhost is running"));
}

struct OwnedGroup(u32);

impl Drop for OwnedGroup {
    fn drop(&mut self) {
        unsafe { libc::killpg(self.0 as libc::pid_t, libc::SIGKILL) };
    }
}

#[test]
fn stop_never_signals_a_process_the_host_lock_names_unless_it_is_the_live_tickhost_run() {
    use std::os::fd::AsRawFd;
    let config_root = ConfigRoot::new();
    let paths = &config_root.paths;
    paths.prepare().unwrap();
    let mut unrelated = crate::OwnedChild(Command::new("/bin/sleep").arg("600").spawn().unwrap());
    std::fs::write(paths.lock(), unrelated.id().to_string()).unwrap();
    let stop = || {
        tickhost()
            .arg("stop")
            .env("TICKLOOP_CONFIG_DIR", config_root.directory.path())
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
fn only_stop_is_offered_and_open_is_gone() {
    let help = tickhost().arg("--help").output().unwrap();
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(help.contains("\n  stop"), "{help}");
    for hidden in ["run", "launch", "open"] {
        assert!(!help.contains(&format!("\n  {hidden}")), "{help}");
    }
    let open = tickhost()
        .args(["open", "--", "/usr/bin/true"])
        .output()
        .unwrap();
    assert_eq!(open.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&open.stderr).contains("unrecognized subcommand 'open'"),
        "{}",
        String::from_utf8_lossy(&open.stderr)
    );
}
