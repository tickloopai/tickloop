//! The daemon's controller works only with a Tickhost of its own build, which the host's frozen
//! management hello names: one of another build that runs nothing is replaced at once, and one
//! that runs Ticks, Shell tasks or connections, or is too old to say, stays until the user
//! replaces it, refusing requests with what to do meanwhile.

use std::{
    io::Write,
    os::{fd::AsRawFd, unix::fs::PermissionsExt},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use serde_json::{Value, json};
use tickhost::{
    host,
    paths::HostPaths,
    shell::{ShellApproval, ShellIdentity, SpawnBinding, environment_digest},
};
use tickhost_connect::{
    host_error::HostError,
    hosted::{self, HostDescription, HostOccupancy, ManagementConnection},
};
use tickloop_agent_protocol::{OtherTickhost, TickhostOccupancy};
use tickloop_agent_runtime::hosted::controller::{HostController, OTHER_TICKHOST_BUILD};
use uuid::Uuid;

const OTHER_BUILD: &str = "0.0.0+an-older-build";
const KEY_FILE: &str = "tickhost.key";

/// How a Tickhost of another build answers the business hello.
#[derive(Clone, Copy, PartialEq)]
enum Business {
    /// As a host of an older business protocol does.
    Refused,
    /// As a host of the same business protocol but other sources does.
    Negotiated,
}

/// A Tickhost of another build. It answers the frozen management hello, the business hello as
/// `business` says, and on `shutdown_host` gives up its endpoint and lock before it answers, as a
/// host does once its Ticks ended.
struct OtherBuildHost {
    stopped: Arc<AtomicBool>,
}

impl OtherBuildHost {
    fn serve(
        paths: &HostPaths,
        key: &str,
        business: Business,
        description: Option<HostDescription>,
    ) -> Self {
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
        let socket = paths.socket();
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        let stopped = Arc::new(AtomicBool::new(false));
        let reply = hosted::HelloReply {
            epoch: Uuid::new_v4(),
            pid: std::process::id(),
            version: hosted::BUILD_VERSION.into(),
            major: hosted::MANAGEMENT_MAJOR,
            minor: hosted::MANAGEMENT_MINOR,
            capabilities: Vec::new(),
            host: description,
        };
        let key = key.to_owned();
        let flag = stopped.clone();
        tokio::spawn(async move {
            let mut lock = Some(lock);
            while let Ok((stream, _)) = listener.accept().await {
                let (reader, mut writer) = stream.into_split();
                let mut reader = tokio::io::BufReader::new(reader);
                let frames = hosted::BUSINESS_FRAME_BYTES;
                let Ok(Some(hello)) = hosted::read_frame(&mut reader, frames).await else {
                    continue;
                };
                if hello["channel"] != "management" {
                    let answer = match business {
                        Business::Refused => hosted::error(
                            &Value::Null,
                            &HostError::before(
                                "IncompatibleHost",
                                "Tickhost serves business protocol 8.0",
                            ),
                        ),
                        Business::Negotiated => hosted::ok(
                            &Value::Null,
                            json!(hosted::HelloReply {
                                major: hosted::BUSINESS_MAJOR,
                                minor: hosted::BUSINESS_MINOR,
                                capabilities: hosted::BUSINESS_CAPABILITIES
                                    .map(str::to_owned)
                                    .to_vec(),
                                host: None,
                                ..reply.clone()
                            }),
                        ),
                    };
                    let _ = hosted::write_frame(&mut writer, &answer, frames).await;
                    // The connection stays open, as an authenticated one does.
                    tokio::spawn(async move {
                        let _ = hosted::read_frame(&mut reader, frames).await;
                        drop(writer);
                    });
                    continue;
                }
                assert_eq!(hello["controller_key"], key.as_str());
                hosted::write_frame(&mut writer, &hosted::ok(&Value::Null, json!(reply)), frames)
                    .await
                    .unwrap();
                let Ok(Some(request)) = hosted::read_frame(&mut reader, frames).await else {
                    continue;
                };
                assert_eq!(request["method"], "shutdown_host");
                assert_eq!(request["params"]["epoch"], json!(reply.epoch));
                flag.store(true, Ordering::SeqCst);
                let _ = std::fs::remove_file(&socket);
                lock.take();
                hosted::write_frame(
                    &mut writer,
                    &hosted::ok(
                        &request["id"],
                        json!({"operation":request["params"]["operation"],"state":"completed"}),
                    ),
                    frames,
                )
                .await
                .unwrap();
                return;
            }
        });
        Self { stopped }
    }

    fn was_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }
}

struct Fixture {
    directory: tempfile::TempDir,
    paths: HostPaths,
    key: String,
    host: OtherBuildHost,
    controller: HostController,
}

impl Fixture {
    /// A daemon whose key opens the management channel of a running host of another build.
    fn with_other_build(business: Business, description: Option<HostDescription>) -> Self {
        let directory = tempfile::Builder::new()
            .prefix("tv")
            .tempdir_in("/private/tmp")
            .unwrap();
        let paths = HostPaths {
            runtime: directory.path().join("h"),
        };
        let key = host::random_token();
        std::fs::write(directory.path().join(KEY_FILE), &key).unwrap();
        let host = OtherBuildHost::serve(&paths, &key, business, description);
        let controller = HostController::in_process(
            directory.path(),
            paths.clone(),
            Uuid::new_v4(),
            host::DEFAULT_IDLE_TIMEOUT,
        );
        Self {
            directory,
            paths,
            key,
            host,
            controller,
        }
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
            },
        }
    }

    /// The build the host now serving this daemon's endpoint names.
    async fn serving_build(&self) -> String {
        ManagementConnection::connect(&self.paths.socket(), &self.key)
            .await
            .unwrap()
            .hello()
            .host
            .clone()
            .unwrap()
            .build
    }
}

fn other_build(occupancy: HostOccupancy) -> Option<HostDescription> {
    Some(HostDescription {
        build: OTHER_BUILD.into(),
        executable: None,
        business_major: hosted::BUSINESS_MAJOR,
        business_minor: hosted::BUSINESS_MINOR,
        occupancy,
    })
}

#[tokio::test]
async fn an_empty_tickhost_of_another_build_is_replaced_without_asking() {
    let fixture =
        Fixture::with_other_build(Business::Refused, other_build(HostOccupancy::default()));

    let grant = tokio::time::timeout(
        Duration::from_secs(20),
        fixture.controller.allow_spawn(&fixture.spawn_binding()),
    )
    .await
    .unwrap()
    .unwrap();

    assert!(fixture.host.was_stopped());
    assert_eq!(fixture.serving_build().await, host::BUILD_IDENTITY);
    assert_eq!(
        fixture.controller.host_epoch().await.unwrap(),
        Some(grant.epoch),
        "the spawn went to the host of this build"
    );
}

#[tokio::test]
async fn a_tickhost_of_another_build_that_runs_something_stays_until_the_user_replaces_it() {
    let occupancy = HostOccupancy {
        ticks: vec!["Quote board".into()],
        shells: 1,
        connections: 0,
    };
    let fixture = Fixture::with_other_build(Business::Refused, other_build(occupancy.clone()));

    let refused = fixture
        .controller
        .allow_spawn(&fixture.spawn_binding())
        .await
        .unwrap_err();
    assert_eq!(
        (refused.code.as_str(), refused.message.as_str()),
        ("IncompatibleHost", OTHER_TICKHOST_BUILD)
    );
    let listing = fixture.controller.host_epoch().await.unwrap_err();
    assert_eq!(listing.message, OTHER_TICKHOST_BUILD);
    assert!(!fixture.host.was_stopped(), "nothing it runs ends unasked");
    assert_eq!(
        fixture.controller.other_tickhost().await.unwrap(),
        Some(OtherTickhost {
            description: format!(
                "Running Tickhost build {OTHER_BUILD}; this build {}",
                host::BUILD_IDENTITY
            ),
            occupancy: Some(TickhostOccupancy {
                ticks: occupancy.ticks,
                shells: occupancy.shells,
                connections: occupancy.connections,
            }),
        })
    );

    fixture.controller.replace_other_tickhost().await.unwrap();
    assert!(fixture.host.was_stopped());
    assert_eq!(fixture.controller.other_tickhost().await.unwrap(), None);
    fixture
        .controller
        .allow_spawn(&fixture.spawn_binding())
        .await
        .unwrap();
    assert_eq!(fixture.serving_build().await, host::BUILD_IDENTITY);
}

#[tokio::test]
async fn a_tickhost_too_old_to_name_its_build_is_another_that_may_run_something() {
    let fixture = Fixture::with_other_build(Business::Refused, None);

    let refused = fixture.controller.host_epoch().await.unwrap_err();
    assert_eq!(refused.message, OTHER_TICKHOST_BUILD);
    assert_eq!(
        fixture.controller.other_tickhost().await.unwrap(),
        Some(OtherTickhost {
            description: format!(
                "Running Tickhost build unknown (an older Tickhost); this build {}",
                host::BUILD_IDENTITY
            ),
            occupancy: None,
        })
    );
    assert!(!fixture.host.was_stopped());
}

/// A host of the same business protocol but other sources negotiates the business hello, and
/// counts the connection this daemon made while asking its build; that one alone keeps nothing
/// from being replaced.
#[tokio::test]
async fn a_tickhost_of_another_build_with_this_business_protocol_is_replaced_too() {
    let fixture = Fixture::with_other_build(
        Business::Negotiated,
        other_build(HostOccupancy {
            ticks: Vec::new(),
            shells: 0,
            connections: 1,
        }),
    );

    tokio::time::timeout(
        Duration::from_secs(20),
        fixture.controller.allow_spawn(&fixture.spawn_binding()),
    )
    .await
    .unwrap()
    .unwrap();

    assert!(fixture.host.was_stopped());
    assert_eq!(fixture.serving_build().await, host::BUILD_IDENTITY);
}
