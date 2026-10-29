//! Resource bounds of the daemon's hosted Tick registry and host controller: one registry record
//! is at most one 1 MiB shard, a host start gives up after its budget, an incompatible host is
//! never replaced, and an orphaned host no daemon can control is stopped by the next request.

use crate::support;

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use serde_json::json;
use support::hosted::counter_request;
use tick_protocol::InstanceRef;
use tickhost::{host::TickOwner, paths::HostPaths};
use tickhost_connect::hosted::{
    BUILD_VERSION, BUSINESS_FRAME_BYTES, BUSINESS_MAJOR, BUSINESS_MINOR, CAPABILITY_LIFECYCLE,
    read_frame, write_frame,
};
use tickloop_code_local_environment::hosted::{
    controller::{HostController, START_ATTEMPTS, START_WAIT},
    registry::{LaunchRecord, LaunchState, Registry, SHARD_BYTES},
};
use uuid::Uuid;

fn private_directory(prefix: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(tickloop_harness::test_support::SHORT_TEMP_ROOT)
        .unwrap()
}

fn launch_path(config_root: &Path, chat: Uuid, operation: Uuid) -> PathBuf {
    config_root
        .join("tickhost/chats")
        .join(chat.to_string())
        .join(format!("launch-{operation}.json"))
}

fn live_record(chat: Uuid, title: String) -> LaunchRecord {
    let mut record = LaunchRecord::opening(
        Uuid::new_v4(),
        chat,
        tickhost::host::TickAdapter::Terminal,
        vec!["/usr/bin/true".into()],
        "/".into(),
        Some(title),
    )
    .unwrap();
    record.state = LaunchState::Live {
        instance: InstanceRef::hosted(Uuid::new_v4()),
        host_epoch: Uuid::new_v4(),
    };
    record
}

/// The title that makes `record` serialize to exactly `size` bytes once it is stored, when its
/// write time has as many digits as now.
fn title_for_size(record: &LaunchRecord, size: usize) -> String {
    let mut measured = record.clone();
    measured.title = Some(String::new());
    measured.updated_ms = 1_000_000_000_000;
    let base = serde_json::to_vec(&measured).unwrap().len();
    "t".repeat(size - base)
}

#[tokio::test]
async fn a_registry_record_over_one_shard_is_refused_before_anything_changes() {
    assert_eq!(SHARD_BYTES, 1024 * 1024, "the production shard is 1 MiB");
    let directory = private_directory("hbr");
    let config_root = directory.path().join("state");
    std::fs::create_dir(&config_root).unwrap();
    let root = Registry::default().root(&config_root).await.unwrap();

    let chat = Uuid::new_v4();
    let mut largest = live_record(chat, String::new());
    largest.title = Some(title_for_size(&largest, SHARD_BYTES));
    root.record_launch(&largest).await.unwrap();
    assert_eq!(
        std::fs::metadata(launch_path(&config_root, chat, largest.operation))
            .unwrap()
            .len(),
        SHARD_BYTES as u64,
        "a record of exactly one shard is stored"
    );

    let unseen = Uuid::new_v4();
    let mut oversized = live_record(unseen, String::new());
    oversized.title = Some(title_for_size(&oversized, SHARD_BYTES + 1));
    let refused = root.record_launch(&oversized).await.unwrap_err();
    assert_eq!(refused.code, "RecordTooLarge", "{refused:?}");
    assert!(!refused.dispatched);
    assert!(
        !config_root
            .join("tickhost/chats")
            .join(unseen.to_string())
            .exists(),
        "nothing is created for a refused record"
    );
}

#[tokio::test]
async fn a_registry_file_over_one_shard_is_refused_and_never_read_whole() {
    assert_eq!(SHARD_BYTES, 1024 * 1024, "the production shard is 1 MiB");
    let directory = private_directory("hbf");
    let config_root = directory.path().join("state");
    std::fs::create_dir(&config_root).unwrap();
    let root = Registry::default().root(&config_root).await.unwrap();
    let chat = Uuid::new_v4();
    let record = live_record(chat, "counter".into());
    root.record_launch(&record).await.unwrap();
    let path = launch_path(&config_root, chat, record.operation);
    let stored = std::fs::read(&path).unwrap();
    let padded = |size: usize| {
        let mut bytes = stored.clone();
        bytes.resize(size, b' ');
        bytes
    };

    std::fs::write(&path, padded(SHARD_BYTES)).unwrap();
    assert_eq!(
        root.launches(chat).await.unwrap().len(),
        1,
        "a file of exactly one shard is read"
    );

    std::fs::write(&path, padded(SHARD_BYTES + 1)).unwrap();
    let refused = root.launches(chat).await.unwrap_err();
    assert_eq!(refused.code, "RecordTooLarge", "{refused:?}");

    // A sparse file of a GiB is refused as quickly as a small one.
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(1024 * 1024 * 1024)
        .unwrap();
    let started = Instant::now();
    let refused = root.launches(chat).await.unwrap_err();
    assert_eq!(refused.code, "RecordTooLarge", "{refused:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
}

/// A stand-in for the daemon's executable that records each start and never serves.
fn recording_executable(directory: &Path) -> (PathBuf, PathBuf) {
    recording_executable_with_metadata(directory, BUILD_VERSION, tickhost::host::BUILD_IDENTITY)
}

/// A `tickhost` with independent release and component identities that never serves.
fn recording_executable_with_metadata(
    directory: &Path,
    version: &str,
    content_identity: &str,
) -> (PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let starts = directory.join("starts");
    let script = directory.join("never-serves");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\ncase \"$1\" in\n--version) echo 'tickhost {version}'; exit 0;;\n--content-identity) echo '{content_identity}'; exit 0;;\nrun) read -r config; echo start >> '{}';;\n*) exit 2;;\nesac\n",
            starts.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    (script, starts)
}

/// How long a started process may take to record itself on a loaded machine.
const START_RECORD_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to keep watching for a start beyond the expected ones.
const EXTRA_START_SETTLE: Duration = Duration::from_millis(300);

/// The starts recorded once the `expected` ones have recorded themselves (or the timeout passed)
/// and any further start has had time to show.
async fn starts(file: &Path, expected: usize) -> usize {
    let recorded = || {
        std::fs::read_to_string(file)
            .map(|starts| starts.lines().count())
            .unwrap_or(0)
    };
    let deadline = tokio::time::Instant::now() + START_RECORD_TIMEOUT;
    while recorded() < expected && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    tokio::time::sleep(EXTRA_START_SETTLE).await;
    recorded()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_that_never_serves_is_started_three_times_then_reported_unavailable() {
    assert_eq!(
        (START_ATTEMPTS, START_WAIT),
        (3, Duration::from_secs(3)),
        "the production start budget"
    );
    let directory = private_directory("hbs");
    // A release version change alone must not reject unchanged Tickhost content.
    let (executable, starts_file) = recording_executable_with_metadata(
        directory.path(),
        "0.0.0-other",
        tickhost::host::BUILD_IDENTITY,
    );
    let controller = HostController::new(
        directory.path(),
        HostPaths {
            runtime: directory.path().join("h"),
        },
        Uuid::new_v4(),
        executable,
        support::hosted::IDLE_TIMEOUT,
    );
    let owner = TickOwner {
        root: "fixture-root".into(),
        chat: Uuid::new_v4(),
    };
    // The request names the fixture's Tickhost, whose first use installs it; only the bounded
    // starts are timed.
    let request = counter_request(owner, directory.path());
    let started = Instant::now();
    let refused = controller.open(request).await.unwrap_err();
    let elapsed = started.elapsed();
    assert_eq!(refused.code, "HostUnavailable", "{refused:?}");
    assert!(!refused.dispatched, "nothing reached a host");
    assert!(
        refused.message.contains("did not start"),
        "the error says the start budget ran out: {refused:?}"
    );
    assert_eq!(
        starts(&starts_file, START_ATTEMPTS as usize).await,
        START_ATTEMPTS as usize
    );
    let budget = START_WAIT * START_ATTEMPTS;
    assert!(
        elapsed >= budget && elapsed < budget + Duration::from_secs(3),
        "the daemon waited {elapsed:?} for a budget of {budget:?}"
    );
}

/// A host at the endpoint that holds the host lock and answers hello with business protocol
/// `major`. It counts the requests it reads and drops each connection after the first one.
struct FakeHost {
    requests: Arc<AtomicUsize>,
    _lock: std::fs::File,
    server: tokio::task::JoinHandle<()>,
}

impl FakeHost {
    fn start(paths: &HostPaths, major: u16) -> Self {
        use std::os::fd::AsRawFd;
        paths.prepare().unwrap();
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(paths.lock())
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let listener = tokio::net::UnixListener::bind(paths.socket()).unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let counted = requests.clone();
        let server = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (reader, mut writer) = stream.into_split();
                let mut reader = tokio::io::BufReader::new(reader);
                if read_frame(&mut reader, BUSINESS_FRAME_BYTES).await.is_err() {
                    continue;
                }
                let hello = json!({"result":{"epoch":Uuid::new_v4(),"pid":std::process::id(),"version":BUILD_VERSION,"major":major,"minor":BUSINESS_MINOR,"capabilities":[CAPABILITY_LIFECYCLE]}});
                if write_frame(&mut writer, &hello, BUSINESS_FRAME_BYTES)
                    .await
                    .is_err()
                {
                    continue;
                }
                if let Ok(Some(_)) = read_frame(&mut reader, BUSINESS_FRAME_BYTES).await {
                    counted.fetch_add(1, Ordering::SeqCst);
                }
            }
        });
        Self {
            requests,
            _lock: lock,
            server,
        }
    }
}

impl Drop for FakeHost {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// Shuts down the host a test started, whatever the test's outcome.
struct StopHost {
    runtime: PathBuf,
    key_file: PathBuf,
}

impl Drop for StopHost {
    fn drop(&mut self) {
        support::stop_tickhost(&self.runtime, &self.key_file);
    }
}

fn socket_inode(paths: &HostPaths) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(paths.socket()).unwrap().ino()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_incompatible_host_is_refused_and_never_replaced() {
    let directory = private_directory("hbi");
    let paths = HostPaths {
        runtime: directory.path().join("h"),
    };
    let host = FakeHost::start(&paths, BUSINESS_MAJOR + 1);
    let inode = socket_inode(&paths);
    std::fs::write(directory.path().join("tickhost.key"), "daemon-key").unwrap();
    let (executable, starts_file) = recording_executable(directory.path());
    let controller = HostController::new(
        directory.path(),
        paths.clone(),
        Uuid::new_v4(),
        executable,
        support::hosted::IDLE_TIMEOUT,
    );
    let owner = TickOwner {
        root: "fixture-root".into(),
        chat: Uuid::new_v4(),
    };
    let refused = controller
        .open(counter_request(owner, directory.path()))
        .await
        .unwrap_err();
    assert_eq!(refused.code, "IncompatibleHost", "{refused:?}");
    assert!(!refused.dispatched);
    assert_eq!(starts(&starts_file, 0).await, 0, "no other host is started");
    assert_eq!(host.requests.load(Ordering::SeqCst), 0, "nothing is sent");
    assert_eq!(socket_inode(&paths), inode, "the endpoint stays the host's");
    assert!(paths.host_running().unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tickhost_of_another_content_is_refused_before_it_starts() {
    let directory = private_directory("hbv");
    let other_content = "tickhost:different-content";
    let (executable, starts_file) =
        recording_executable_with_metadata(directory.path(), BUILD_VERSION, other_content);
    let controller = HostController::new(
        directory.path(),
        HostPaths {
            runtime: directory.path().join("h"),
        },
        Uuid::new_v4(),
        executable.clone(),
        support::hosted::IDLE_TIMEOUT,
    );
    let owner = TickOwner {
        root: "fixture-root".into(),
        chat: Uuid::new_v4(),
    };
    let refused = controller
        .open(counter_request(owner, directory.path()))
        .await
        .unwrap_err();
    assert_eq!(refused.code, "IncompatibleHost", "{refused:?}");
    assert!(!refused.dispatched);
    for named in [
        executable.to_string_lossy().as_ref(),
        other_content,
        tickhost::host::BUILD_IDENTITY,
        "same build",
    ] {
        assert!(
            refused.message.contains(named),
            "{named}: {}",
            refused.message
        );
    }
    assert_eq!(starts(&starts_file, 0).await, 0, "no host is started");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_open_that_reached_the_host_is_never_sent_again() {
    let directory = private_directory("hbd");
    let paths = HostPaths {
        runtime: directory.path().join("h"),
    };
    let host = FakeHost::start(&paths, BUSINESS_MAJOR);
    std::fs::write(directory.path().join("tickhost.key"), "daemon-key").unwrap();
    let (executable, starts_file) = recording_executable(directory.path());
    let controller = HostController::new(
        directory.path(),
        paths.clone(),
        Uuid::new_v4(),
        executable,
        support::hosted::IDLE_TIMEOUT,
    );
    let owner = TickOwner {
        root: "fixture-root".into(),
        chat: Uuid::new_v4(),
    };
    let lost = controller
        .open(counter_request(owner, directory.path()))
        .await
        .unwrap_err();
    assert!(lost.dispatched, "{lost:?}");
    assert_eq!(host.requests.load(Ordering::SeqCst), 1, "sent exactly once");
    assert_eq!(starts(&starts_file, 0).await, 0, "no other host is started");
}

/// Only one daemon serves a config root and only it holds the controller key, so a running host
/// that refuses this daemon's key, or runs while the key is missing, is an orphan no daemon can
/// control. The first request that meets it stops it as `tickhost stop` does: a listing then
/// finds no host and starts none, and an open starts a host with this daemon's key. An
/// unreadable key may still be the running host's, so it never stops or replaces it.
#[tokio::test(flavor = "multi_thread")]
async fn an_orphaned_host_is_stopped_by_the_next_listing_or_open() {
    for replacement_key in [Some(tickhost::host::random_token()), None] {
        for first in [FirstRequest::Listing, FirstRequest::Open] {
            an_orphaned_host_is_stopped(replacement_key.clone(), first).await;
        }
    }
}

/// The first request that meets an orphaned host.
#[derive(Clone, Copy, Debug)]
enum FirstRequest {
    Listing,
    Open,
}

/// An orphan whose daemon's key file now holds `replacement_key`, or is missing.
async fn an_orphaned_host_is_stopped(replacement_key: Option<String>, first: FirstRequest) {
    use std::os::unix::fs::PermissionsExt;
    let directory = private_directory("hbo");
    let paths = HostPaths::of_config_root(Some(directory.path()));
    let executable = support::hosted::executable_in(directory.path());
    let first_runtime = directory.path().join("first");
    std::fs::create_dir(&first_runtime).unwrap();
    let _stop_orphan = StopHost {
        runtime: paths.runtime.clone(),
        key_file: first_runtime.join("tickhost.key"),
    };
    let owner = TickOwner {
        root: "fixture-root".into(),
        chat: Uuid::new_v4(),
    };
    let previous = HostController::new(
        &first_runtime,
        paths.clone(),
        Uuid::new_v4(),
        executable.clone(),
        support::hosted::IDLE_TIMEOUT,
    );
    previous
        .open(counter_request(owner.clone(), directory.path()))
        .await
        .unwrap();
    // The daemon that started the host is gone; its Tick keeps running.
    drop(previous);
    let orphan = paths.running_host_pid().unwrap().unwrap();

    let second_runtime = directory.path().join("second");
    std::fs::create_dir(&second_runtime).unwrap();
    let key_file = second_runtime.join("tickhost.key");
    let _stop_replacement = StopHost {
        runtime: paths.runtime.clone(),
        key_file: key_file.clone(),
    };
    let second = HostController::new(
        &second_runtime,
        paths.clone(),
        Uuid::new_v4(),
        executable,
        support::hosted::IDLE_TIMEOUT,
    );

    std::fs::write(&key_file, tickhost::host::random_token()).unwrap();
    std::fs::set_permissions(&key_file, std::fs::Permissions::from_mode(0o000)).unwrap();
    let unreadable = [
        second
            .open(counter_request(owner.clone(), directory.path()))
            .await
            .unwrap_err(),
        second.list(&owner, None).await.unwrap_err(),
    ];
    for refused in unreadable {
        assert_eq!(refused.code, "HostUnavailable", "{refused:?}");
        assert!(!refused.dispatched);
    }
    assert_eq!(paths.running_host_pid().unwrap(), Some(orphan));
    std::fs::remove_file(&key_file).unwrap();
    if let Some(key) = replacement_key {
        std::fs::write(&key_file, key).unwrap();
    }

    if let FirstRequest::Listing = first {
        let listed = second.list(&owner, None).await.unwrap();
        assert_eq!(listed["total"], 0, "{listed}");
        assert!(exits(orphan).await, "a listing stops the orphaned host");
        assert!(!paths.host_running().unwrap(), "and starts none");
        assert_eq!(second.host_epoch().await.unwrap(), None);
    }

    let opened = second
        .open(counter_request(owner.clone(), directory.path()))
        .await
        .unwrap();
    let replacement = paths.running_host_pid().unwrap().unwrap();
    assert_ne!(
        replacement, orphan,
        "{first:?}: a new host serves the endpoint"
    );
    assert!(
        exits(orphan).await,
        "{first:?}: the orphaned host was stopped"
    );
    let listed = second.list(&owner, None).await.unwrap();
    assert_eq!(listed["total"], 1, "{listed}");
    assert_eq!(listed["items"][0]["instance"], json!(opened.instance));
    second
        .close(Uuid::new_v4(), &owner, &opened.instance)
        .await
        .unwrap();
}

/// Whether process `pid` exits within a few seconds.
async fn exits(pid: u32) -> bool {
    for _ in 0..200 {
        if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0 {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}
