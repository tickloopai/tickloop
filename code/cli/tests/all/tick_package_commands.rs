//! `tickloop tick` as a user runs it, against a private config root, with a local file or a
//! local HTTP server standing in for the Tick marketplace and its downloads.

use std::{
    collections::{BTreeMap, HashMap},
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use flate2::{Compression, write::GzEncoder};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tick_oauth_test_server::{AuthorizationServer, REVOCATION_PATH, TokenReply, USER_CODE};
use tick_packages::CURRENT_TARGET;

use crate::support;

const OTHER_TARGET: &str = "x86_64-unknown-linux-gnu";
const TERMINAL_TIMEOUT: Duration = Duration::from_secs(20);
const OFFICIAL_MARKETPLACE_URL: &str = "https://github.com/anthropics/claude-plugins-official.git";
/// Where a test creates the repository that stands in for the built-in Git marketplace.
pub(crate) const OFFICIAL_MARKETPLACE_REPOSITORY: &str = "official-marketplace";

pub(crate) struct Fixture {
    _directory: tempfile::TempDir,
    pub(crate) root: PathBuf,
}

impl Fixture {
    pub(crate) fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        fs::create_dir(root.join("home")).unwrap();
        // Tests never reach GitHub: the built-in Git marketplace is read from a local repository
        // a test may create, and is unreadable otherwise.
        fs::write(
            root.join("home/.gitconfig"),
            format!(
                "[url \"file://{}\"]\n\tinsteadOf = {OFFICIAL_MARKETPLACE_URL}\n",
                root.join(OFFICIAL_MARKETPLACE_REPOSITORY).display()
            ),
        )
        .unwrap();
        support::cli_runtime::adopt(&root.join("config"));
        Self {
            _directory: directory,
            root,
        }
    }

    pub(crate) fn tickloop(&self, arguments: &[&str]) -> Output {
        self.command(arguments).output().unwrap()
    }

    /// Runs tickloop on a terminal of its own, which the test reads and types on.
    pub(crate) fn tickloop_on_terminal(&self, arguments: &[&str]) -> Terminal {
        self.tickloop_on_terminal_with_env(arguments, &[])
    }

    pub(crate) fn tickloop_on_terminal_with_env(
        &self,
        arguments: &[&str],
        environment: &[(&str, &str)],
    ) -> Terminal {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_tickloop"));
        command.args(arguments);
        command.cwd(&self.root);
        command.env_clear();
        command.env("PATH", "/usr/bin:/bin");
        command.env("HOME", self.root.join("home"));
        command.env("TERM", "xterm-256color");
        command.env("NO_COLOR", "1");
        command.env("TICKLOOP_CONFIG_DIR", self.root.join("config"));
        for (name, value) in environment {
            command.env(name, value);
        }
        let child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let shown = Arc::new(Mutex::new(String::new()));
        let collected = shown.clone();
        thread::spawn(move || {
            let mut buffer = [0; 4096];
            while let Ok(read) = reader.read(&mut buffer) {
                if read == 0 {
                    break;
                }
                let text = String::from_utf8_lossy(&buffer[..read]).replace('\r', "");
                collected.lock().unwrap().push_str(&text);
            }
        });
        let input = pair.master.take_writer().unwrap();
        Terminal {
            child,
            master: pair.master,
            input,
            shown,
        }
    }

    pub(crate) fn tickloop_from(&self, marketplace: &str, arguments: &[&str]) -> Output {
        self.command(arguments)
            .env("TICKLOOP_TICK_MARKETPLACE", marketplace)
            .output()
            .unwrap()
    }

    pub(crate) fn command(&self, arguments: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_tickloop"));
        command
            .args(arguments)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.root.join("home"))
            .env("NO_COLOR", "1")
            .env("TICKLOOP_CONFIG_DIR", self.root.join("config"))
            .current_dir(&self.root)
            .stdin(Stdio::null());
        command
    }

    /// A package directory of `sample` at `version`, whose program prints `version`.
    fn package(&self, version: &str) -> PathBuf {
        self.package_named("sample", version)
    }

    pub(crate) fn package_named(&self, name: &str, version: &str) -> PathBuf {
        let directory = self.root.join("sources").join(name).join(version);
        fs::create_dir_all(directory.join("bin")).unwrap();
        fs::create_dir_all(directory.join("skills").join(name)).unwrap();
        fs::write(
            directory.join("tick-package.json"),
            json!({
                "name": name,
                "version": version,
                "description": format!("The {name} package"),
                "executable": format!("bin/tick-{name}"),
                "protocol": true,
            })
            .to_string(),
        )
        .unwrap();
        let program = directory.join("bin").join(format!("tick-{name}"));
        fs::write(&program, format!("#!/bin/sh\necho {name} {version}\n")).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(
            directory.join("skills").join(name).join("SKILL.md"),
            format!("---\nname: {name}\n---\n"),
        )
        .unwrap();
        directory
    }

    /// The package `name` at `version` whose manifest declares `auth`.
    pub(crate) fn package_with_auth(&self, name: &str, version: &str, auth: Value) -> PathBuf {
        let directory = self.package_named(name, version);
        let manifest_path = directory.join("tick-package.json");
        let mut manifest: Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest["auth"] = auth;
        fs::write(&manifest_path, manifest.to_string()).unwrap();
        directory
    }

    /// The package `name` at `version` that logs in with OAuth at an issuer no test reaches.
    pub(crate) fn package_with_login(&self, name: &str, version: &str) -> PathBuf {
        self.package_with_auth(
            name,
            version,
            oauth2(&[("Remote", "https://auth.example.com")]),
        )
    }

    /// Where the package `name`'s login is saved.
    fn credentials(&self, name: &str) -> PathBuf {
        self.root.join("config/credentials").join(name)
    }

    /// The package `sample` at `version` as a release archive, and its SHA-256.
    fn archive(&self, version: &str) -> (Vec<u8>, String) {
        archive_of(&self.package(version))
    }

    /// Writes `marketplace` as the local marketplace file `label` and returns its path.
    pub(crate) fn marketplace_file(&self, label: &str, marketplace: &Value) -> String {
        self.text_file(label, &marketplace.to_string())
    }

    fn text_file(&self, label: &str, contents: &str) -> String {
        let path = self.root.join(format!("{label}.json"));
        fs::write(&path, contents).unwrap();
        path_text(&path)
    }

    pub(crate) fn install_official(&self, directory: &Path) {
        let manifest: Value =
            serde_json::from_slice(&fs::read(directory.join("tick-package.json")).unwrap())
                .unwrap();
        let (bytes, sha256) = archive_of(directory);
        let archive = self.root.join("fixture.tar.gz");
        fs::write(&archive, bytes).unwrap();
        tick_plugins::PluginStore::in_config_root(&self.root.join("config"))
            .install_archive(
                &archive,
                tick_plugins::ExpectedArchive {
                    marketplace: "tickloop",
                    name: manifest["name"].as_str().unwrap(),
                    version: manifest["version"].as_str().unwrap(),
                    sha256: &sha256,
                    url: "https://example.test/fixture.tar.gz",
                },
            )
            .unwrap();
    }

    fn installed_root(&self) -> PathBuf {
        self.root.join("config/plugins/cache/local/sample/local")
    }

    fn packages_directory(&self) -> PathBuf {
        self.root.join("config/plugins")
    }

    /// Every file and directory under the packages directory, with each file's contents.
    fn packages_state(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut state = BTreeMap::new();
        let mut pending = vec![self.packages_directory()];
        while let Some(directory) = pending.pop() {
            for entry in fs::read_dir(&directory).unwrap() {
                let path = entry.unwrap().path();
                let contents = if path.is_dir() {
                    pending.push(path.clone());
                    Vec::new()
                } else {
                    fs::read(&path).unwrap()
                };
                state.insert(path, contents);
            }
        }
        state
    }

    /// The packages directory holds only its lock, its registry and package directories.
    fn assert_no_leftovers(&self) {
        for entry in fs::read_dir(self.packages_directory()).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            assert!(
                name == ".lock" || name == "installed.json" || !name.starts_with('.'),
                "left behind {name}"
            );
        }
    }
}

/// A tickloop running on a terminal of its own.
pub(crate) struct Terminal {
    child: Box<dyn Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
    input: Box<dyn Write + Send>,
    shown: Arc<Mutex<String>>,
}

impl Terminal {
    /// Waits until the terminal shows `text`, and returns all it showed so far.
    pub(crate) fn wait_for(&self, text: &str) -> String {
        let deadline = Instant::now() + TERMINAL_TIMEOUT;
        loop {
            let shown = self.shown.lock().unwrap().clone();
            if shown.contains(text) {
                return shown;
            }
            assert!(Instant::now() < deadline, "no {text:?} in:\n{shown}");
            thread::sleep(Duration::from_millis(20));
        }
    }

    pub(crate) fn type_text(&mut self, text: &str) {
        self.input.write_all(text.as_bytes()).unwrap();
        self.input.flush().unwrap();
    }

    /// Waits for tickloop to exit; returns its exit code and all it showed.
    pub(crate) fn finish(mut self) -> (u32, String) {
        let code = self.child.wait().unwrap().exit_code();
        // The output may still be on its way once the program has exited.
        thread::sleep(Duration::from_millis(100));
        drop(self.master);
        (code, self.shown.lock().unwrap().clone())
    }
}

/// An `oauth2` auth with each `(label, issuer)`.
pub(crate) fn oauth2(issuers: &[(&str, &str)]) -> Value {
    let issuers = issuers
        .iter()
        .map(|(label, issuer)| json!({ "label": label, "issuer": issuer }))
        .collect::<Vec<_>>();
    json!({ "type": "oauth2", "issuers": issuers })
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// The package directory at `directory` as a release archive, and its SHA-256.
pub(crate) fn archive_of(directory: &Path) -> (Vec<u8>, String) {
    let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
    builder.append_dir_all(".", directory).unwrap();
    let archive = builder.into_inner().unwrap().finish().unwrap();
    let sha256 = format!("{:x}", Sha256::digest(&archive));
    (archive, sha256)
}

pub(crate) fn marketplace_json(packages: Value) -> Value {
    json!({
        "name": "tickloop",
        "owner": { "name": "Tickloop AI", "url": "https://tickloop.ai" },
        "packages": packages,
    })
}

/// The marketplace entry of `name` at `version`, downloadable for each of `downloads`' targets.
pub(crate) fn offered(name: &str, version: &str, downloads: &[(&str, &str, &str)]) -> Value {
    let downloads = downloads
        .iter()
        .map(|(target, url, sha256)| (target.to_string(), json!({ "url": url, "sha256": sha256 })))
        .collect::<serde_json::Map<_, _>>();
    json!({
        "name": name,
        "version": version,
        "description": format!("The {name} package"),
        "downloads": downloads,
    })
}

/// What the local HTTP server answers for a path.
#[derive(Clone)]
pub(crate) enum Reply {
    Body(Vec<u8>),
    Status(u16),
    /// Announces the whole body, then closes the connection halfway through it.
    Interrupted(Vec<u8>),
    /// Announces a body larger than any download may be, and sends none.
    Oversized,
    /// Sends the body without announcing its length, ending it by closing the connection.
    Unannounced(Vec<u8>),
    Redirect(String),
}

/// An HTTP server on 127.0.0.1 that answers each path as told and records the paths requested.
pub(crate) struct Server {
    origin: String,
    replies: Arc<Mutex<HashMap<String, Reply>>>,
    requests: Arc<Mutex<Vec<String>>>,
}

impl Server {
    pub(crate) fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let replies = Arc::new(Mutex::new(HashMap::new()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (shared_replies, shared_requests) = (replies.clone(), requests.clone());
        thread::spawn(move || {
            for stream in listener.incoming() {
                answer(stream.unwrap(), &shared_replies, &shared_requests);
            }
        });
        Self {
            origin,
            replies,
            requests,
        }
    }

    /// Answers `path` with `reply` from now on and returns its URL.
    pub(crate) fn serve(&self, path: &str, reply: Reply) -> String {
        self.replies.lock().unwrap().insert(path.to_owned(), reply);
        format!("{}{path}", self.origin)
    }

    fn requests_of(&self, path: &str) -> usize {
        let requests = self.requests.lock().unwrap();
        requests
            .iter()
            .filter(|requested| *requested == path)
            .count()
    }
}

fn answer(
    mut stream: TcpStream,
    replies: &Mutex<HashMap<String, Reply>>,
    requests: &Mutex<Vec<String>>,
) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request_line = String::new();
    reader.read_line(&mut request_line).unwrap();
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).unwrap() == 0 || header == "\r\n" {
            break;
        }
    }
    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_owned();
    requests.lock().unwrap().push(path.clone());
    let reply = replies.lock().unwrap().get(&path).cloned();
    let head = |status: u16, length: usize| {
        format!("HTTP/1.1 {status} Reply\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n")
    };
    let _ = match reply.unwrap_or(Reply::Status(404)) {
        Reply::Body(body) => stream
            .write_all(head(200, body.len()).as_bytes())
            .and_then(|()| stream.write_all(&body)),
        Reply::Status(status) => stream.write_all(head(status, 0).as_bytes()),
        Reply::Interrupted(body) => stream
            .write_all(head(200, body.len()).as_bytes())
            .and_then(|()| stream.write_all(&body[..body.len() / 2])),
        Reply::Oversized => stream.write_all(head(200, 257 * 1024 * 1024).as_bytes()),
        Reply::Unannounced(body) => stream
            .write_all(b"HTTP/1.1 200 Reply\r\nConnection: close\r\n\r\n")
            .and_then(|()| stream.write_all(&body)),
        Reply::Redirect(location) => stream.write_all(
            format!("HTTP/1.1 302 Reply\r\nLocation: {location}\r\nContent-Length: 0\r\n\r\n")
                .as_bytes(),
        ),
    };
    let _ = stream.shutdown(Shutdown::Both);
}

pub(crate) fn succeeded(output: &Output) -> String {
    assert!(
        output.status.success(),
        "{:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone()).unwrap()
}

pub(crate) fn failed(output: &Output) -> String {
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    String::from_utf8(output.stderr.clone()).unwrap()
}

pub(crate) fn path_text(path: &Path) -> String {
    path.display().to_string()
}

#[test]
fn a_package_installs_lists_updates_and_uninstalls_through_the_cli() {
    let fixture = Fixture::new();
    assert_eq!(
        succeeded(&fixture.tickloop(&["tick", "list"])),
        "No Ticks are installed.\n"
    );
    let first = fixture.package("1.0.0");
    let first_path = path_text(&first);
    let installed_root = path_text(&fixture.installed_root());

    assert_eq!(
        succeeded(&fixture.tickloop(&["tick", "install", "--path", &first_path])),
        format!(
            "Installed Tick sample@local 1.0.0 in {installed_root}.\nWhen opened, its Tick program runs as your user. Installing runs nothing.\n"
        )
    );
    let program = Command::new(fixture.installed_root().join("bin/tick-sample"))
        .output()
        .unwrap();
    assert_eq!(String::from_utf8(program.stdout).unwrap(), "sample 1.0.0\n");
    assert_eq!(
        succeeded(&fixture.tickloop(&["tick", "install", "--path", &first_path])),
        format!(
            "Reinstalled Tick sample@local 1.0.0 in {installed_root}.\nWhen opened, its Tick program runs as your user. Installing runs nothing.\n"
        )
    );
    assert_eq!(
        succeeded(&fixture.tickloop(&["tick", "list"])),
        "sample@local  1.0.0  enabled  no login needed\n"
    );

    let second = path_text(&fixture.package("2.0.0"));
    assert_eq!(
        succeeded(&fixture.tickloop(&["tick", "install", "--path", &second])),
        format!(
            "Replaced Tick sample@local 1.0.0 with 2.0.0 in {installed_root}.\nWhen opened, its Tick program runs as your user. Installing runs nothing.\n"
        )
    );
    assert_eq!(
        succeeded(&fixture.tickloop(&["tick", "list"])),
        "sample@local  2.0.0  enabled  no login needed\n"
    );

    assert_eq!(
        succeeded(&fixture.tickloop(&["tick", "uninstall", "sample"])),
        "Uninstalled Tick sample@local 2.0.0.\n"
    );
    assert!(!fixture.installed_root().exists());
    assert_eq!(
        failed(&fixture.tickloop(&["tick", "uninstall", "sample"])),
        "Error: plugin sample is not installed\n"
    );
    assert_eq!(
        succeeded(&fixture.tickloop(&["tick", "list"])),
        "No Ticks are installed.\n"
    );
}

#[test]
fn a_failed_install_says_why_and_keeps_the_installed_package() {
    let fixture = Fixture::new();
    succeeded(&fixture.tickloop(&[
        "tick",
        "install",
        "--path",
        &path_text(&fixture.package("1.0.0")),
    ]));
    let broken = fixture.package("2.0.0");
    fs::set_permissions(
        broken.join("bin/tick-sample"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();

    let error = failed(&fixture.tickloop(&["tick", "install", "--path", &path_text(&broken)]));

    assert_eq!(
        error,
        format!(
            "Error: cannot install {}: executable bin/tick-sample is not executable\n",
            broken.display()
        )
    );
    assert_eq!(
        succeeded(&fixture.tickloop(&["tick", "list"])),
        "sample@local  1.0.0  enabled  no login needed\n"
    );
    let missing = fixture.root.join("missing");
    let error = failed(&fixture.tickloop(&["tick", "install", "--path", &path_text(&missing)]));
    assert!(
        error.starts_with(&format!(
            "Error: cannot install {}: cannot open",
            missing.display()
        )),
        "{error}"
    );

    fs::remove_file(fixture.installed_root().join("bin/tick-sample")).unwrap();
    assert_eq!(
        succeeded(&fixture.tickloop(&["tick", "list"])),
        "sample@local  1.0.0  enabled  login unknown  unusable: executable bin/tick-sample does not exist in the \
         package; install it again\n"
    );

    let registry = fixture.root.join("config/plugins/installed.json");
    fs::write(&registry, "{").unwrap();
    let error = failed(&fixture.tickloop(&["tick", "list"]));
    assert!(
        error.starts_with(&format!("Error: {} is damaged", registry.display())),
        "{error}"
    );
    assert_eq!(fs::read(&registry).unwrap(), b"{");
}

#[test]
fn the_tick_command_explains_its_usage_and_leaves_the_console_arguments_alone() {
    let fixture = Fixture::new();
    for arguments in [
        &["tick"][..],
        &["tick", "install"],
        &["tick", "install", "--path"],
        &["tick", "install", "-p", "sample"],
        &["tick", "install", "--force"],
        &["tick", "list", "--all"],
        &["tick", "uninstall"],
        &["tick", "login"],
        &["tick", "logout", "--all"],
        &["tick", "login", "sample", "now"],
        &["tick", "login", "sample", "--issuer"],
        &["tick", "login", "--issuer", "global"],
        &["tick", "login", "sample", "--region", "global"],
        &["tick", "remove", "sample"],
        &["tick", "validate", "."],
        &["tick", "marketplace", "list"],
    ] {
        let output = fixture.tickloop(arguments);
        let error = failed(&output);
        assert!(
            error.starts_with(
                "Error: Unknown tick command. Usage: tickloop tick install TICK[@MARKETPLACE]"
            ),
            "{arguments:?}: {error}"
        );
        assert!(!error.contains("renamed"), "{error}");
    }
    for arguments in [
        &["tick", "--help"][..],
        &["tick", "-h"],
        &["tick", "install", "--help"],
        &["tick", "login", "--help"],
        &["tick", "list", "-h"],
    ] {
        let help = succeeded(&fixture.tickloop(arguments));
        assert!(
            help.starts_with(
                "Usage: tickloop tick install TICK[@MARKETPLACE]\n       tickloop tick install --path PATH\n"
            ),
            "{help}"
        );
        assert!(help.contains("TICKLOOP_CONFIG_DIR"), "{help}");
        assert!(help.contains("TICKLOOP_TICK_MARKETPLACE"), "{help}");
        assert!(
            help.contains("tickloop tick login TICK [--issuer LABEL]"),
            "{help}"
        );
        assert!(
            help.contains("login and logout manage the Tick's data source credentials"),
            "{help}"
        );
        assert!(help.contains("tickloop plugin marketplace"), "{help}");
        assert!(
            help.contains("Installing runs nothing in the Tick"),
            "{help}"
        );
    }
    let console_help = succeeded(&fixture.tickloop(&["--help"]));
    assert!(
        console_help.contains(
            "tickloop tick install TICK[@MARKETPLACE] | install --path PATH | update [TICK] | uninstall TICK [--keep-data] | enable TICK | disable TICK | list [--available] | login TICK [--issuer LABEL] | logout TICK"
        ),
        "{console_help}"
    );
    assert!(!console_help.contains("deprecated"), "{console_help}");
    assert!(!fixture.root.join("config").exists());
}

#[test]
fn a_package_installs_from_the_marketplace_and_updates_to_the_version_it_offers() {
    let fixture = Fixture::new();
    let server = Server::start();
    let (first, first_sha256) = fixture.archive("1.0.0");
    let first_url = server.serve("/tick-sample-1.0.0.tar.gz", Reply::Body(first));
    let marketplace = fixture.marketplace_file(
        "first",
        &marketplace_json(json!([offered(
            "sample",
            "1.0.0",
            &[(CURRENT_TARGET, &first_url, &first_sha256)],
        )])),
    );
    let installed_root = path_text(
        &fixture
            .root
            .join("config/plugins/cache/tickloop/sample/1.0.0"),
    );

    assert_eq!(
        succeeded(&fixture.tickloop_from(&marketplace, &["tick", "install", "sample@tickloop"])),
        format!(
            "Installed Tick sample@tickloop 1.0.0 in {installed_root}.\nWhen opened, its Tick program runs as your user. Installing runs nothing.\n"
        )
    );
    let program = Command::new(
        fixture
            .root
            .join("config/plugins/cache/tickloop/sample/1.0.0")
            .join("bin/tick-sample"),
    )
    .output()
    .unwrap();
    assert_eq!(String::from_utf8(program.stdout).unwrap(), "sample 1.0.0\n");
    let registry: Value = serde_json::from_slice(
        &fs::read(fixture.packages_directory().join("installed.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(registry["plugins"]["sample"]["source"]["url"], first_url);
    assert_eq!(registry["plugins"]["sample"]["sha256"], first_sha256);

    assert_eq!(
        succeeded(&fixture.tickloop_from(&marketplace, &["tick", "install", "sample@tickloop"])),
        format!(
            "Tick sample@tickloop 1.0.0 is already installed in {installed_root}; nothing changed.\nWhen opened, its Tick program runs as your user. Installing runs nothing.\n"
        )
    );
    assert_eq!(server.requests_of("/tick-sample-1.0.0.tar.gz"), 1);

    fs::remove_file(
        fixture
            .root
            .join("config/plugins/cache/tickloop/sample/1.0.0")
            .join("bin/tick-sample"),
    )
    .unwrap();
    assert_eq!(
        succeeded(&fixture.tickloop_from(&marketplace, &["tick", "install", "sample@tickloop"])),
        format!(
            "Reinstalled Tick sample@tickloop 1.0.0 in {installed_root}.\nWhen opened, its Tick program runs as your user. Installing runs nothing.\n"
        )
    );
    assert_eq!(server.requests_of("/tick-sample-1.0.0.tar.gz"), 2);
    assert!(
        fixture
            .root
            .join("config/plugins/cache/tickloop/sample/1.0.0")
            .join("bin/tick-sample")
            .is_file()
    );

    let (second, second_sha256) = fixture.archive("2.0.0");
    let second_url = server.serve("/tick-sample-2.0.0.tar.gz", Reply::Body(second));
    let marketplace = fixture.marketplace_file(
        "second",
        &marketplace_json(json!([offered(
            "sample",
            "2.0.0",
            &[(CURRENT_TARGET, &second_url, &second_sha256)],
        )])),
    );
    let installed_root = path_text(
        &fixture
            .root
            .join("config/plugins/cache/tickloop/sample/2.0.0"),
    );
    assert_eq!(
        succeeded(&fixture.tickloop_from(&marketplace, &["tick", "install", "sample@tickloop"])),
        format!(
            "Replaced Tick sample@tickloop 1.0.0 with 2.0.0 in {installed_root}.\nWhen opened, its Tick program runs as your user. Installing runs nothing.\n"
        )
    );
    assert_eq!(
        succeeded(&fixture.tickloop(&["tick", "list"])),
        "sample@tickloop  2.0.0  enabled  no login needed\n"
    );
    fixture.assert_no_leftovers();
}

#[test]
fn a_failed_marketplace_install_says_why_and_changes_nothing() {
    let fixture = Fixture::new();
    let server = Server::start();
    fixture.install_official(&fixture.package("1.0.0"));
    let before = fixture.packages_state();
    let (archive, sha256) = fixture.archive("2.0.0");
    let archive_url = server.serve("/tick-sample-2.0.0.tar.gz", Reply::Body(archive.clone()));
    let interrupted_url = server.serve("/interrupted.tar.gz", Reply::Interrupted(archive));
    let missing_url = server.serve("/missing.tar.gz", Reply::Status(404));
    let oversized_url = server.serve("/oversized.tar.gz", Reply::Oversized);
    let redirecting_url = server.serve(
        "/redirecting.tar.gz",
        Reply::Redirect("http://example.com/tick-sample-2.0.0.tar.gz".to_owned()),
    );
    let wrong_sha256 = "0".repeat(64);
    let offering = |label: &str, target: &str, url: &str, sha256: &str| {
        fixture.marketplace_file(
            label,
            &marketplace_json(json!([offered(
                "sample",
                "2.0.0",
                &[(target, url, sha256)]
            )])),
        )
    };
    let without_sample = fixture.marketplace_file(
        "without-sample",
        &marketplace_json(json!([offered(
            "other",
            "1.0.0",
            &[(CURRENT_TARGET, &archive_url, &sha256)],
        )])),
    );
    let invalid = fixture.text_file("invalid", "{\"name\": \"tickloop\"");
    let absent = path_text(&fixture.root.join("absent.json"));
    let closed_port = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}/marketplace.json", listener.local_addr().unwrap())
    };
    let oversized_marketplace_url = server.serve(
        "/oversized-marketplace.json",
        Reply::Unannounced(vec![b' '; 1024 * 1024 + 1]),
    );
    let refused = "is refused: only https is accepted, or http on this machine";

    let cases = [
        (
            without_sample.clone(),
            format!("Tick package sample is not in the Tick marketplace at {without_sample}"),
        ),
        (
            offering("elsewhere", OTHER_TARGET, &archive_url, &sha256),
            format!("Tick package sample 2.0.0 has no prebuilt program for {CURRENT_TARGET}"),
        ),
        (
            offering("wrong-sha256", CURRENT_TARGET, &archive_url, &wrong_sha256),
            format!(
                "cannot install sample 2.0.0 from {archive_url}: \
                 the archive's SHA-256 is {sha256}, not {wrong_sha256}"
            ),
        ),
        (
            offering("interrupted", CURRENT_TARGET, &interrupted_url, &sha256),
            format!("cannot download sample 2.0.0 from {interrupted_url}: the transfer failed ("),
        ),
        (
            offering("missing", CURRENT_TARGET, &missing_url, &sha256),
            format!(
                "cannot download sample 2.0.0 from {missing_url}: \
                 the server answered HTTP 404 Not Found"
            ),
        ),
        (
            offering(
                "insecure",
                CURRENT_TARGET,
                "http://example.com/tick-sample-2.0.0.tar.gz",
                &sha256,
            ),
            format!(
                "cannot download sample 2.0.0: http://example.com/tick-sample-2.0.0.tar.gz {refused}"
            ),
        ),
        (
            offering("oversized", CURRENT_TARGET, &oversized_url, &sha256),
            format!("cannot download sample 2.0.0 from {oversized_url}: it is larger than 256 MiB"),
        ),
        (
            offering("redirecting", CURRENT_TARGET, &redirecting_url, &sha256),
            format!(
                "cannot download sample 2.0.0 from {redirecting_url}: cannot follow its redirect \
                 (it redirects to an address that does not use https)"
            ),
        ),
        (
            invalid.clone(),
            format!("the Tick marketplace at {invalid} is invalid: its JSON is not valid"),
        ),
        (
            absent.clone(),
            format!("cannot read the Tick marketplace at {absent}: No such file or directory"),
        ),
        (
            oversized_marketplace_url.clone(),
            format!(
                "cannot read the Tick marketplace at {oversized_marketplace_url}: \
                 it is larger than 1 MiB"
            ),
        ),
        (
            closed_port.clone(),
            format!("cannot read the Tick marketplace at {closed_port}: cannot connect ("),
        ),
        (
            "http://example.com/marketplace.json".to_owned(),
            format!("TICKLOOP_TICK_MARKETPLACE: http://example.com/marketplace.json {refused}"),
        ),
    ];

    for (marketplace, reason) in cases {
        let error =
            failed(&fixture.tickloop_from(&marketplace, &["tick", "install", "sample@tickloop"]));
        assert!(
            error.starts_with(&format!("Error: {reason}")),
            "{marketplace}: {error}"
        );
        assert_eq!(fixture.packages_state(), before, "{marketplace}: {error}");
        fixture.assert_no_leftovers();
    }
    assert_eq!(server.requests_of("/interrupted.tar.gz"), 1);
    assert_eq!(
        succeeded(&fixture.tickloop(&["tick", "list"])),
        "sample@tickloop  1.0.0  enabled  no login needed\n"
    );

    let registry = fixture.packages_directory().join("installed.json");
    fs::write(&registry, "{").unwrap();
    let unreached_url = server.serve("/unreached.tar.gz", Reply::Body(Vec::new()));
    let error = failed(&fixture.tickloop_from(
        &offering("unreached", CURRENT_TARGET, &unreached_url, &sha256),
        &["tick", "install", "sample@tickloop"],
    ));
    assert!(
        error.starts_with(&format!("Error: {} is damaged", registry.display())),
        "{error}"
    );
    assert_eq!(server.requests_of("/unreached.tar.gz"), 0);
    assert_eq!(fs::read(&registry).unwrap(), b"{");
    fixture.assert_no_leftovers();
}

#[test]
fn the_available_packages_show_which_are_installed_and_which_this_platform_can_install() {
    let fixture = Fixture::new();
    let server = Server::start();
    fixture.install_official(&fixture.package_with_login("current", "1.0.0"));
    fixture.install_official(&fixture.package_named("outdated", "1.0.0"));
    succeeded(&fixture.tickloop(&[
        "tick",
        "install",
        "--path",
        &path_text(&fixture.package_named("fresh", "0.1.0")),
    ]));
    let sha256 = "a".repeat(64);
    let download = |name: &str| format!("https://example.com/tick-{name}.tar.gz");
    let marketplace_url = server.serve(
        "/marketplace.json",
        Reply::Body(
            marketplace_json(json!([
                offered(
                    "outdated",
                    "2.0.0",
                    &[(CURRENT_TARGET, &download("outdated"), &sha256)]
                ),
                offered(
                    "current",
                    "1.0.0",
                    &[(CURRENT_TARGET, &download("current"), &sha256)]
                ),
                offered(
                    "fresh",
                    "0.3.0",
                    &[(CURRENT_TARGET, &download("fresh"), &sha256)]
                ),
                offered(
                    "elsewhere",
                    "0.1.0",
                    &[(OTHER_TARGET, &download("elsewhere"), &sha256)]
                ),
            ]))
            .to_string()
            .into_bytes(),
        ),
    );

    let listing = fixture.tickloop_from(&marketplace_url, &["tick", "list", "--available"]);

    assert!(listing.status.success());
    assert!(
        String::from_utf8_lossy(&listing.stderr)
            .starts_with("Warning: cannot read marketplace claude-plugins-official: "),
        "the built-in Git marketplace stands in as an unreadable local repository"
    );
    let rows = String::from_utf8(listing.stdout)
        .unwrap()
        .lines()
        .map(|line| {
            line.split("  ")
                .map(str::trim)
                .filter(|column| !column.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let unavailable = format!("not installed; no prebuilt program for {CURRENT_TARGET}");
    assert_eq!(
        rows,
        [
            vec![
                "current@tickloop",
                "1.0.0",
                "installed",
                "The current package"
            ],
            vec![
                "elsewhere@tickloop",
                "0.1.0",
                &unavailable,
                "The elsewhere package"
            ],
            vec![
                "fresh@tickloop",
                "0.3.0",
                "not installed; fresh@local is installed",
                "The fresh package"
            ],
            vec![
                "outdated@tickloop",
                "2.0.0",
                "installed 1.0.0",
                "The outdated package"
            ],
        ]
    );

    let empty = fixture.marketplace_file("empty", &marketplace_json(json!([])));
    let listing = fixture.tickloop_from(&empty, &["tick", "list", "--available"]);
    assert!(listing.status.success());
    assert_eq!(
        String::from_utf8(listing.stdout).unwrap(),
        "No marketplace offers Ticks yet.\n"
    );
    assert_eq!(
        failed(&fixture.tickloop_from(&empty, &["tick", "install", "fresh@tickloop"])),
        format!("Error: Tick package fresh is not in the Tick marketplace at {empty}\n")
    );
}

#[test]
fn a_package_logs_in_with_oauth2_and_out_and_lists_its_login_state() {
    let fixture = Fixture::new();
    let server = AuthorizationServer::start();
    for package in [
        fixture.package_with_auth("sample", "1.0.0", oauth2(&[("Local", server.issuer())])),
        fixture.package_named("plain", "1.0.0"),
    ] {
        succeeded(&fixture.tickloop(&["tick", "install", "--path", &path_text(&package)]));
    }
    let list = || succeeded(&fixture.tickloop(&["tick", "list"]));
    let logged_out = "plain@local   1.0.0  enabled  no login needed\n\
                      sample@local  1.0.0  enabled  needs login\n";
    assert_eq!(list(), logged_out);
    assert_eq!(
        failed(&fixture.tickloop(&["tick", "login", "plain"])),
        "Error: Tick package plain needs no login\n"
    );
    assert_eq!(
        failed(&fixture.tickloop(&["tick", "logout", "plain"])),
        "Error: Tick package plain needs no login\n"
    );
    assert_eq!(
        failed(&fixture.tickloop(&["tick", "login", "missing"])),
        "Error: plugin missing is not installed; install it with tickloop tick install missing\n"
    );

    // No terminal is needed: the link and code are printed and the login waits for approval.
    server.answer_polls([TokenReply::Pending, TokenReply::Approve]);
    let shown = succeeded(&fixture.tickloop(&["tick", "login", "sample"]));
    let issuer = server.issuer();
    assert_eq!(
        shown,
        format!(
            "Log in to sample (Local): open this link in a browser on any device and confirm the code.\n\n  \
             {issuer}/verify?user_code={USER_CODE}\n\n  Code: {USER_CODE}\n\n\
             Waiting for the approval; the code expires in 10 minutes. Ctrl+C cancels.\n\
             Logged in to sample (Local).\n"
        )
    );
    let token_path = fixture.credentials("sample").join("oauth2.json");
    let saved: Value = serde_json::from_slice(&fs::read(&token_path).unwrap()).unwrap();
    assert_eq!(saved["access_token"], "access-1");
    assert_eq!(saved["issuer"], issuer);
    assert_eq!(
        fs::metadata(&token_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        list(),
        "plain@local   1.0.0  enabled  no login needed\n\
         sample@local  1.0.0  enabled  logged in\n"
    );

    server.answer_polls([TokenReply::Denied]);
    assert_eq!(
        failed(&fixture.tickloop(&["tick", "login", "sample"]))
            .lines()
            .last(),
        Some("Error: cannot log in to sample (Local): the authorization was denied")
    );
    let kept: Value = serde_json::from_slice(&fs::read(&token_path).unwrap()).unwrap();
    assert_eq!(kept, saved, "a refused login keeps the saved one");

    let mut expired = saved.clone();
    expired["expires_at"] = json!(now() - 60);
    expired.as_object_mut().unwrap().remove("refresh_token");
    fs::write(&token_path, expired.to_string()).unwrap();
    assert!(list().contains("sample@local  1.0.0  enabled  login expired"));
    fs::write(&token_path, "{").unwrap();
    assert!(list().contains("sample@local  1.0.0  enabled  login unknown"));
    fs::write(&token_path, saved.to_string()).unwrap();

    assert_eq!(
        succeeded(&fixture.tickloop(&["tick", "logout", "sample"])),
        "Logged out of sample.\n"
    );
    let revocation = &server.requests_to(REVOCATION_PATH)[0];
    assert_eq!(revocation.form["token"], "refresh-1");
    assert!(!token_path.exists());
    assert_eq!(list(), logged_out);
    assert_eq!(
        succeeded(&fixture.tickloop(&["tick", "logout", "sample"])),
        "Tick package sample was not logged in.\n"
    );

    // A failed revocation is reported and the login deleted all the same.
    server.answer_polls([TokenReply::Approve]);
    succeeded(&fixture.tickloop(&["tick", "login", "sample"]));
    server.answer_revocations(503);
    assert_eq!(
        succeeded(&fixture.tickloop(&["tick", "logout", "sample"])),
        format!(
            "The authorization server did not revoke the token ({issuer}{REVOCATION_PATH} \
             answered HTTP 503 Service Unavailable); it is deleted here all the same.\n\
             Logged out of sample.\n"
        )
    );
    assert_eq!(list(), logged_out);
    assert_eq!(
        succeeded(&fixture.tickloop(&["tick", "uninstall", "sample"])),
        format!(
            "Uninstalled Tick sample@local 1.0.0.\nIts data source login stays in {}.\n",
            path_text(&fixture.credentials("sample"))
        )
    );
}

#[test]
fn a_package_with_several_issuers_logs_in_with_the_one_chosen() {
    let fixture = Fixture::new();
    let (mainland, global) = (AuthorizationServer::start(), AuthorizationServer::start());
    let package = fixture.package_with_auth(
        "sample",
        "1.0.0",
        oauth2(&[
            ("China mainland", mainland.issuer()),
            ("Global", global.issuer()),
        ]),
    );
    succeeded(&fixture.tickloop(&["tick", "install", "--path", &path_text(&package)]));

    assert_eq!(
        failed(&fixture.tickloop(&["tick", "login", "sample"])),
        "Error: Tick package sample logs in with one of: China mainland, Global; choose one with \
         tickloop tick login sample --issuer LABEL\n"
    );
    assert_eq!(
        failed(&fixture.tickloop(&["tick", "login", "sample", "--issuer", "Mars"])),
        "Error: Tick package sample has no issuer called Mars; choose one of: China mainland, Global\n"
    );
    assert!(mainland.requests().is_empty() && global.requests().is_empty());

    global.answer_polls([TokenReply::Approve]);
    let shown = succeeded(&fixture.tickloop(&["tick", "login", "sample", "--issuer", "global"]));
    assert!(
        shown.ends_with("Logged in to sample (Global).\n"),
        "{shown}"
    );
    assert!(mainland.requests().is_empty());
    let token_path = fixture.credentials("sample").join("oauth2.json");
    let saved: Value = serde_json::from_slice(&fs::read(&token_path).unwrap()).unwrap();
    assert_eq!(saved["issuer"], global.issuer());

    // On a terminal the issuer is typed, until it names one.
    mainland.answer_polls([TokenReply::Approve]);
    let mut terminal = fixture.tickloop_on_terminal(&["tick", "login", "sample"]);
    terminal.wait_for("Type the one to use: ");
    terminal.type_text("mars\r");
    terminal.wait_for("That is none of: China mainland, Global.");
    terminal.type_text("china MAINLAND\r");
    let (code, shown) = terminal.finish();
    assert_eq!(code, 0, "{shown}");
    assert!(
        shown.starts_with("Tick package sample logs in with one of: China mainland, Global.\n"),
        "{shown}"
    );
    assert!(
        shown.contains("Logged in to sample (China mainland)."),
        "{shown}"
    );
    let saved: Value = serde_json::from_slice(&fs::read(&token_path).unwrap()).unwrap();
    assert_eq!(saved["issuer"], mainland.issuer());
}

#[test]
fn an_api_key_is_read_without_showing_it() {
    let fixture = Fixture::new();
    let package = fixture.package_with_auth(
        "keyed",
        "1.0.0",
        json!({"type": "api_key", "env": "KEYED_API_KEY", "label": "Keyed API key"}),
    );
    succeeded(&fixture.tickloop(&["tick", "install", "--path", &path_text(&package)]));
    let key_path = fixture.credentials("keyed").join("api_key");

    let mut terminal = fixture.tickloop_on_terminal(&["tick", "login", "keyed"]);
    terminal.wait_for("Keyed API key for keyed (hidden as you type): ");
    terminal.type_text("secret-key-123\r");
    let (code, shown) = terminal.finish();
    assert_eq!(code, 0, "{shown}");
    assert!(!shown.contains("secret-key-123"), "{shown}");
    assert!(
        shown.contains("Saved the Keyed API key for keyed."),
        "{shown}"
    );
    assert_eq!(fs::read(&key_path).unwrap(), b"secret-key-123");
    assert_eq!(
        fs::metadata(&key_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        succeeded(&fixture.tickloop(&["tick", "list"])),
        "keyed@local  1.0.0  enabled  logged in\n"
    );

    let mut terminal = fixture.tickloop_on_terminal(&["tick", "login", "keyed"]);
    terminal.wait_for("(hidden as you type): ");
    terminal.type_text("abandoned\x1b");
    let (code, shown) = terminal.finish();
    assert_eq!(code, 1, "{shown}");
    assert!(
        shown.contains("Error: the login to keyed was cancelled"),
        "{shown}"
    );
    assert_eq!(fs::read(&key_path).unwrap(), b"secret-key-123");

    let piped = fixture
        .command(&["tick", "login", "keyed"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    piped
        .stdin
        .as_ref()
        .unwrap()
        .write_all(b"  piped-key\nignored\n")
        .unwrap();
    succeeded(&piped.wait_with_output().unwrap());
    assert_eq!(fs::read(&key_path).unwrap(), b"piped-key");
    assert_eq!(
        failed(&fixture.tickloop(&["tick", "login", "keyed"])),
        "Error: cannot log in to keyed: a key holds 1 to 4096 bytes\n"
    );
    assert_eq!(
        failed(&fixture.tickloop(&["tick", "login", "keyed", "--issuer", "Global"])),
        "Error: Tick package keyed logs in with a key; --issuer is for OAuth logins\n"
    );

    assert_eq!(
        succeeded(&fixture.tickloop(&["tick", "logout", "keyed"])),
        "Logged out of keyed.\n"
    );
    assert!(!key_path.exists());
    assert_eq!(
        succeeded(&fixture.tickloop(&["tick", "list"])),
        "keyed@local  1.0.0  enabled  needs login\n"
    );
}
