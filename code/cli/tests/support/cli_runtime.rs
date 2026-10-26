use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    time::Duration,
};

use tickloop_code_client::LocalAgentClient;

use tickloop_code_access::{AgentAccess, AgentCommand, CommandEnvelope};

use uuid::Uuid;

const DAEMON_SOCKET: &str = "daemon.sock";
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(15);

thread_local! {
    static DAEMONS: RefCell<Vec<PrivateDaemon>> = const { RefCell::new(Vec::new()) };
}

/// A CLI fixture's config root and the runtime directories derived from it. Dropping it stops
/// the daemon there gracefully, flushing its Transcripts, then removes the daemon and Tickhost
/// runtime directories, which also ends a daemon that did not stop.
struct PrivateDaemon {
    _config_root: Option<tempfile::TempDir>,
    config_root: PathBuf,
    runtime: PathBuf,
    tickhost: PathBuf,
}

impl PrivateDaemon {
    fn of(config_root: &Path, owned: Option<tempfile::TempDir>) -> Self {
        let daemon = Self {
            config_root: config_root.to_path_buf(),
            runtime: daemon_directory_of(config_root),
            tickhost: tickhost::paths::HostPaths::of_config_root(Some(config_root)).runtime,
            _config_root: owned,
        };
        for path in [&daemon.config_root, &daemon.runtime, &daemon.tickhost] {
            tickloop_harness::test_support::register_runtime_directory(path);
        }
        daemon
    }
}

/// A private config root, so the CLI started with it in `TICKLOOP_CONFIG_DIR` gets a daemon and
/// a Tickhost of its own.
pub fn config_root() -> PathBuf {
    let directory = super::runtime_dir();
    let path = directory.path().to_path_buf();
    DAEMONS.with(|daemons| {
        daemons
            .borrow_mut()
            .push(PrivateDaemon::of(&path, Some(directory)))
    });
    path
}

/// Ends the daemon of a config root the test made itself when the test ends, and returns the
/// daemon's runtime directory.
pub fn adopt(config_root: &Path) -> PathBuf {
    let daemon = PrivateDaemon::of(config_root, None);
    let runtime = daemon.runtime.clone();
    DAEMONS.with(|daemons| daemons.borrow_mut().push(daemon));
    runtime
}

/// Gives ordinary interactive fixtures an explicit model and private test credential.
/// First-run and credential tests deliberately do not call this helper.
pub fn configure_model(config_root: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(config_root).unwrap();
    let canonical_root = config_root.canonicalize().unwrap();
    let config_root = canonical_root.as_path();
    std::fs::set_permissions(config_root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let model = tickloop_harness::model::cards::ModelCatalog::defaults()
        .get("deepseek-api", "deepseek-flash")
        .unwrap();
    tickloop_code_settings::UserSettings::in_root(config_root)
        .save_model(&model, false)
        .unwrap();
    tickloop_harness::model::credentials::save_api_key(
        config_root,
        "DEEPSEEK_API_KEY",
        "fixture-default-key",
    )
    .unwrap();
}

/// Runs `executable` with the fixture's config root when an API accepts only an executable path.
pub fn executable_in(config_root: &Path, executable: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let launcher = config_root.join(format!(
        "fixture-{}",
        executable.file_name().unwrap().to_str().unwrap()
    ));
    if !launcher.exists() {
        std::fs::create_dir_all(config_root).unwrap();
        std::fs::write(
            &launcher,
            format!(
                "#!/bin/sh\nexport TICKLOOP_CONFIG_DIR={}\nexec {} \"$@\"\n",
                shell_words::quote(config_root.to_str().unwrap()),
                shell_words::quote(executable.to_str().unwrap()),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    launcher
}

/// The runtime directory of the daemon serving `config_root`.
pub fn daemon_directory_of(config_root: &Path) -> PathBuf {
    tickloop_code_client::daemon_endpoint::runtime_directory_of(Some(config_root))
}

/// The runtime directory of the daemon a CLI fixture command reaches through its config root.
pub fn command_directory(command: &std::process::Command) -> PathBuf {
    command
        .get_envs()
        .find_map(|(name, value)| (name == "TICKLOOP_CONFIG_DIR").then_some(value).flatten())
        .map(|config_root| daemon_directory_of(Path::new(config_root)))
        .expect("CLI fixture has no private config root")
}

fn verify_owned(directory: &Path) -> Result<(), String> {
    let is_owned = DAEMONS.with(|daemons| {
        daemons
            .borrow()
            .iter()
            .any(|daemon| daemon.runtime == directory)
    });
    if !is_owned {
        return Err("refusing to access an endpoint not owned by this CLI fixture".into());
    }
    Ok(())
}

pub async fn flush_transcripts(directory: &Path) -> Result<(), String> {
    verify_owned(directory)?;
    tokio::time::timeout(CLEANUP_TIMEOUT, async {
        let client = LocalAgentClient::connect(directory.join(DAEMON_SOCKET))
            .await
            .map_err(|error| error.to_string())?;
        flush(&client).await
    })
    .await
    .map_err(|_| "private CLI Transcript flush timed out".to_owned())?
}

pub async fn shutdown_daemon(directory: &Path) -> Result<(), String> {
    verify_owned(directory)?;
    tokio::time::timeout(CLEANUP_TIMEOUT, async {
        let client = LocalAgentClient::connect(directory.join(DAEMON_SOCKET))
            .await
            .map_err(|error| error.to_string())?;
        flush(&client).await?;
        client
            .shutdown_daemon()
            .await
            .map_err(|error| error.to_string())?;
        while directory.join(DAEMON_SOCKET).exists() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        Ok(())
    })
    .await
    .map_err(|_| "private CLI daemon shutdown timed out".to_owned())?
}

async fn flush(client: &LocalAgentClient) -> Result<(), String> {
    client
        .execute(CommandEnvelope {
            daemon_epoch: client.daemon_epoch(),
            command_id: Uuid::new_v4(),
            command: AgentCommand::FlushTranscripts,
        })
        .await
        .map_err(|error| error.to_string())?
        .into_result()
        .map_err(|error| error.to_string())?;
    Ok(())
}

impl Drop for PrivateDaemon {
    fn drop(&mut self) {
        let directory = self.runtime.clone();
        if directory.join(DAEMON_SOCKET).exists() {
            stop_gracefully(&directory);
        }
        if self.tickhost.join("host.sock").exists() {
            let stopped = std::process::Command::new(super::fixture_cli::tickhost())
                .arg("stop")
                .env("TICKLOOP_CONFIG_DIR", &self.config_root)
                .env_remove("TICKLOOP_TICK_RUNTIME_DIR")
                .output();
            if !stopped.is_ok_and(|output| output.status.success()) {
                eprintln!(
                    "Private CLI Tickhost cleanup failed: {}",
                    self.tickhost.display()
                );
                return;
            }
        }
        let _ = std::fs::remove_dir_all(&self.runtime);
        let _ = std::fs::remove_dir_all(&self.tickhost);
    }
}

fn stop_gracefully(directory: &Path) {
    // Cleanup runs after the test's Tokio runtime may already have been dropped.
    let cleanup_directory = directory.to_path_buf();
    let cleanup = std::thread::Builder::new()
        .name("private-cli-daemon-cleanup".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| error.to_string())?;
            runtime.block_on(async {
                tokio::time::timeout(CLEANUP_TIMEOUT, async {
                    let client = LocalAgentClient::connect(cleanup_directory.join(DAEMON_SOCKET))
                        .await
                        .map_err(|error| error.to_string())?;
                    let flushed = flush(&client).await;
                    client
                        .shutdown_daemon()
                        .await
                        .map_err(|error| error.to_string())?;
                    while cleanup_directory.join(DAEMON_SOCKET).exists() {
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                    flushed
                })
                .await
                .map_err(|_| "private CLI daemon shutdown timed out".to_owned())?
            })
        });
    let result = match cleanup {
        Ok(thread) => thread
            .join()
            .unwrap_or_else(|_| Err("private CLI daemon cleanup thread panicked".into())),
        Err(error) => Err(error.to_string()),
    };
    if let Err(error) = result {
        eprintln!(
            "Private CLI daemon cleanup failed; removing {} ends the daemon: {error}",
            directory.display()
        );
    }
}

struct CleanupProbeHost {
    child: std::process::Child,
    runtime: PathBuf,
}
impl Drop for CleanupProbeHost {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.runtime);
    }
}

#[test]
fn adopted_config_cleanup_stops_tickhost_before_removing_its_runtime() {
    use std::{
        io::Write,
        process::{Command, Stdio},
        time::Instant,
    };
    let root = tempfile::tempdir().unwrap();
    let paths = tickhost::paths::HostPaths::of_config_root(Some(root.path()));
    let host = Command::new(super::fixture_cli::tickhost())
        .arg("run")
        .env("TICKLOOP_CONFIG_DIR", root.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut host = CleanupProbeHost {
        child: host,
        runtime: paths.runtime.clone(),
    };
    writeln!(
        host.child.stdin.take().unwrap(),
        "{}",
        serde_json::to_string(&tickhost::host::HostConfig {
            controller_key: tickhost::host::random_token(),
            idle_timeout: Duration::from_secs(600)
        })
        .unwrap()
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !paths.socket().exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let started = paths.socket().exists();
    let root_path = root.path().to_path_buf();
    std::thread::spawn(move || {
        adopt(&root_path);
    })
    .join()
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    while host.child.try_wait().unwrap().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let exited = host.child.try_wait().unwrap().is_some();
    // The regression also cleans its own child when run against the broken fixture.
    if !exited {
        let _ = host.child.kill();
    }
    host.child.wait().unwrap();
    let _ = std::fs::remove_dir_all(&paths.runtime);
    assert!(started, "fixture Tickhost did not start");
    assert!(
        exited,
        "fixture removed the runtime but left Tickhost running"
    );
}

#[test]
fn fixture_main_thread_releases_its_config_root_on_exit() {
    let output = std::process::Command::new(super::fixture_cli::executable())
        .arg("--cleanup-probe")
        .output()
        .unwrap();
    assert!(output.status.success());
    let root = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
    let leaked = root.exists();
    if leaked {
        std::fs::remove_dir_all(&root).unwrap();
    }
    assert!(!leaked, "fixture main thread left {}", root.display());
}
