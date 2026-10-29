//! A running Tickhost that refuses the daemon's controller key, or runs while the key is missing,
//! can never be controlled again, so every request stops it: one that only reads then finds no
//! host, and an open or spawn starts a host with the daemon's key. A host this process serves is
//! ended directly; a process that is not `tickhost run` is never signalled.

use std::{os::fd::AsRawFd, time::Duration};

use serde_json::Value;
use tickhost::{
    host::{self, HostConfig, TickOwner},
    in_process::InProcessHost,
    paths::HostPaths,
    shell::{ShellApproval, ShellIdentity, SpawnBinding, environment_digest},
};
use tickhost_connect::{host_error::HostError, hosted::ManagementConnection};
use tickloop_code_local_environment::hosted::controller::HostController;
use uuid::Uuid;

const KEY_FILE: &str = "tickhost.key";

/// What the daemon's key file holds once the host it started is orphaned.
#[derive(Clone, Copy, Debug)]
enum OrphanKey {
    Replaced,
    Missing,
}

/// The first request that meets the orphan.
#[derive(Clone, Copy, Debug)]
enum FirstRequest {
    /// A listing, which never starts a host.
    Listing,
    /// A spawn, which starts one.
    Spawn,
}

struct Fixture {
    directory: tempfile::TempDir,
    paths: HostPaths,
    controller: HostController,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::Builder::new()
            .prefix("to")
            .tempdir_in(tickloop_harness::test_support::SHORT_TEMP_ROOT)
            .unwrap();
        let paths = HostPaths {
            runtime: directory.path().join("h"),
        };
        let controller = HostController::in_process(
            directory.path(),
            paths.clone(),
            Uuid::new_v4(),
            host::DEFAULT_IDLE_TIMEOUT,
        );
        Self {
            directory,
            paths,
            controller,
        }
    }

    fn key_file(&self) -> std::path::PathBuf {
        self.directory.path().join(KEY_FILE)
    }

    fn spawn_binding(&self) -> SpawnBinding {
        SpawnBinding {
            operation: Uuid::new_v4(),
            identity: ShellIdentity {
                daemon_epoch: self.controller.daemon_epoch(),
                worker: Uuid::new_v4(),
                chat: Uuid::new_v4(),
            },
            approval: ShellApproval {
                shell: "/bin/sh".into(),
                command: "true".into(),
                cwd: self.directory.path().into(),
                environment: environment_digest(&[]),
                sandbox: None,
            },
        }
    }

    async fn spawn(&self) -> Result<Uuid, HostError> {
        self.controller
            .allow_spawn(&self.spawn_binding())
            .await
            .map(|grant| grant.epoch)
    }

    async fn list(&self) -> Result<Value, HostError> {
        let owner = TickOwner {
            root: "root".into(),
            chat: Uuid::new_v4(),
        };
        self.controller.list(&owner, None).await
    }

    async fn wait_for_host(&self) {
        for _ in 0..200 {
            if self.paths.socket().exists() && self.paths.host_running().unwrap() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("the host did not start");
    }
}

#[tokio::test]
async fn a_host_served_here_that_no_daemon_can_control_is_stopped_by_any_request() {
    for key in [OrphanKey::Replaced, OrphanKey::Missing] {
        for first in [FirstRequest::Listing, FirstRequest::Spawn] {
            an_orphan_served_here_is_stopped(key, first).await;
        }
    }
}

async fn an_orphan_served_here_is_stopped(key: OrphanKey, first: FirstRequest) {
    let fixture = Fixture::new();
    let orphan = fixture.spawn().await.unwrap();
    // The daemon's connections are gone, and so is the key it started the host with.
    fixture.controller.relinquish_daemon().await;
    match key {
        OrphanKey::Replaced => std::fs::write(fixture.key_file(), host::random_token()).unwrap(),
        OrphanKey::Missing => std::fs::remove_file(fixture.key_file()).unwrap(),
    }

    if let FirstRequest::Listing = first {
        let listed = fixture.list().await.unwrap();
        assert_eq!(listed["total"], 0, "{key:?}: {listed}");
        assert!(
            !fixture.paths.host_running().unwrap(),
            "{key:?}: a listing stops the orphan and starts nothing"
        );
        assert_eq!(fixture.controller.host_epoch().await.unwrap(), None);
    }

    let replacement = fixture.spawn().await.unwrap();
    assert_ne!(replacement, orphan, "{key:?} {first:?}");
    let key = std::fs::read_to_string(fixture.key_file()).unwrap();
    assert_eq!(
        ManagementConnection::connect(&fixture.paths.socket(), key.trim())
            .await
            .unwrap()
            .hello()
            .epoch,
        replacement,
        "the new host takes the daemon's key"
    );
}

/// The lock of a host this process serves names this process, which is not `tickhost run`; no
/// request signals it, and each says why.
#[tokio::test]
async fn a_host_that_is_not_tickhost_run_is_never_signalled() {
    let fixture = Fixture::new();
    fixture.paths.prepare().unwrap();
    let foreign = InProcessHost::start(
        fixture.paths.clone(),
        HostConfig {
            controller_key: host::random_token(),
            idle_timeout: host::DEFAULT_IDLE_TIMEOUT,
        },
    )
    .unwrap();
    fixture.wait_for_host().await;
    assert!(!fixture.key_file().exists());

    for refused in [
        fixture.list().await.unwrap_err(),
        fixture.spawn().await.unwrap_err(),
    ] {
        assert_refused_signal(&refused);
    }
    assert!(fixture.paths.host_running().unwrap());
    assert!(!foreign.is_finished(), "the host keeps running");
}

/// A host of another business protocol checks the controller key only on its frozen management
/// channel; one that refuses it there is as uncontrollable as one that refuses the business
/// hello, not a host of another build kept for the user.
#[tokio::test]
async fn a_host_of_another_protocol_that_refuses_the_key_is_uncontrollable() {
    let fixture = Fixture::new();
    std::fs::write(fixture.key_file(), host::random_token()).unwrap();
    let _host = RefusingHost::serve(&fixture.paths);

    for refused in [
        fixture.list().await.unwrap_err(),
        fixture.spawn().await.unwrap_err(),
    ] {
        assert_refused_signal(&refused);
    }
    assert!(fixture.paths.host_running().unwrap());
}

/// The refusal of a stop the lock holder's arguments did not allow: the daemon tried to stop the
/// host, and signalled nothing.
fn assert_refused_signal(refused: &HostError) {
    assert_eq!(refused.code, "HostUnavailable", "{refused:?}");
    assert!(
        refused.message.contains("is not `tickhost run`"),
        "{refused:?}"
    );
    assert!(!refused.dispatched);
}

/// A host of an older business protocol whose controller key this daemon does not hold: it holds
/// the host lock, refuses the business hello as incompatible and the management hello as
/// forbidden.
struct RefusingHost {
    server: tokio::task::JoinHandle<()>,
}

impl RefusingHost {
    fn serve(paths: &HostPaths) -> Self {
        use std::{io::Write, os::unix::fs::PermissionsExt};
        use tickhost_connect::hosted;
        paths.prepare().unwrap();
        let mut lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(paths.lock())
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        lock.write_all(std::process::id().to_string().as_bytes())
            .unwrap();
        let listener = tokio::net::UnixListener::bind(paths.socket()).unwrap();
        std::fs::set_permissions(paths.socket(), std::fs::Permissions::from_mode(0o600)).unwrap();
        let server = tokio::spawn(async move {
            let _lock = lock;
            while let Ok((stream, _)) = listener.accept().await {
                let (reader, mut writer) = stream.into_split();
                let mut reader = tokio::io::BufReader::new(reader);
                let frames = hosted::BUSINESS_FRAME_BYTES;
                let Ok(Some(hello)) = hosted::read_frame(&mut reader, frames).await else {
                    continue;
                };
                let refusal = if hello["channel"] == "management" {
                    HostError::before("Forbidden", "management requires the daemon's key")
                } else {
                    HostError::before("IncompatibleHost", "Tickhost serves business protocol 1.0")
                };
                let _ = hosted::write_frame(
                    &mut writer,
                    &hosted::error(&Value::Null, &refusal),
                    frames,
                )
                .await;
            }
        });
        Self { server }
    }
}

impl Drop for RefusingHost {
    fn drop(&mut self) {
        self.server.abort();
    }
}
