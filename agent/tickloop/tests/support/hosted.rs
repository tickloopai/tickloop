//! Opens of hosted Ticks for tests that run a private Tickhost: a protocol program starts behind
//! the fixture CLI's Tick launcher, which the daemon's host controller puts in front of it.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use tickhost::host::{OpenRequest, TickAdapter, TickOwner};
use uuid::Uuid;

/// How long a test Tickhost lets an unused Tick idle: longer than any wait of a test step, and
/// short enough that a host a killed test left behind exits soon after its Ticks go idle.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// The environment a hosted test Tick runs with.
pub fn tick_environment() -> Vec<(String, String)> {
    vec![("PATH".into(), "/usr/bin:/bin".into())]
}

/// Python running `script` unbuffered with `arguments`.
pub fn python_program(script: &Path, arguments: &[String]) -> Vec<String> {
    let mut program = vec![
        "/usr/bin/python3".into(),
        "-u".into(),
        script.to_string_lossy().into_owned(),
    ];
    program.extend_from_slice(arguments);
    program
}

/// The example counter Tick.
pub fn counter_program() -> Vec<String> {
    let counter = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tick/examples/counter.py")
        .canonicalize()
        .unwrap();
    python_program(&counter, &[])
}

/// Installs the Tick package `counter` under `config_root` from a package directory it writes in
/// `sources`: its program `tick-counter` runs the example counter Tick, and its manifest declares
/// the Tick protocol.
pub fn install_counter_package(config_root: &Path, sources: &Path) -> tick_packages::PackageStore {
    use std::os::unix::fs::PermissionsExt;
    let source = sources.join("counter-package");
    std::fs::create_dir_all(source.join("bin")).unwrap();
    std::fs::write(
        source.join(tick_packages::MANIFEST_FILE_NAME),
        serde_json::json!({
            "name": "counter",
            "version": "1.0.0",
            "description": "The example counter",
            "executable": "bin/tick-counter",
            "protocol": true,
        })
        .to_string(),
    )
    .unwrap();
    let program = source.join("bin/tick-counter");
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\nexec '{}' \"$@\"\n",
            counter_program().join("' '")
        ),
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    let packages = tick_packages::PackageStore::in_config_root(config_root);
    packages.install_path(&source).unwrap();
    packages
}

/// An open of the protocol `program` for `owner`.
pub fn tick_request(owner: TickOwner, cwd: &Path, program: Vec<String>) -> OpenRequest {
    OpenRequest {
        operation: Uuid::new_v4(),
        owner,
        owner_revision: 0,
        adapter: TickAdapter::Protocol,
        command: program,
        cwd: cwd.to_path_buf(),
        environment: tick_environment(),
        title: None,
    }
}

/// An open of the example counter Tick for `owner`.
pub fn counter_request(owner: TickOwner, cwd: &Path) -> OpenRequest {
    tick_request(owner, cwd, counter_program())
}

/// A running Tickhost of another build than the tests' own, at a daemon's Tickhost endpoint. It
/// answers the frozen management hello with `description` to the holder of `key`, refuses the
/// business protocol as an older host does, and on `shutdown_host` gives up its endpoint and lock
/// before it answers, as a host does once its Ticks ended.
pub struct OtherBuildHost {
    stopped: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl OtherBuildHost {
    pub fn serve(
        paths: &tickhost::paths::HostPaths,
        key: &str,
        description: Option<tickhost_connect::hosted::HostDescription>,
    ) -> Self {
        use std::{io::Write, os::fd::AsRawFd, os::unix::fs::PermissionsExt};
        use tickhost_connect::{host_error::HostError, hosted};
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
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let listener = tokio::net::UnixListener::from_std(listener).unwrap();
        let stopped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
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
                if hello["channel"] != "management" || hello["controller_key"] != key.as_str() {
                    let refusal = HostError::before(
                        "IncompatibleHost",
                        "Tickhost serves business protocol 8.0",
                    );
                    let _ = hosted::write_frame(
                        &mut writer,
                        &hosted::error(&serde_json::Value::Null, &refusal),
                        frames,
                    )
                    .await;
                    continue;
                }
                let _ = hosted::write_frame(
                    &mut writer,
                    &hosted::ok(&serde_json::Value::Null, serde_json::json!(reply)),
                    frames,
                )
                .await;
                let Ok(Some(request)) = hosted::read_frame(&mut reader, frames).await else {
                    continue;
                };
                if request["method"] != "shutdown_host" {
                    continue;
                }
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
                let _ = std::fs::remove_file(&socket);
                lock.take();
                let _ = hosted::write_frame(
                    &mut writer,
                    &hosted::ok(
                        &request["id"],
                        serde_json::json!({"operation":request["params"]["operation"],"state":"completed"}),
                    ),
                    frames,
                )
                .await;
                return;
            }
        });
        Self { stopped }
    }

    pub fn was_stopped(&self) -> bool {
        self.stopped.load(std::sync::atomic::Ordering::SeqCst)
    }
}
